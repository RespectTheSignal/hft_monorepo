"""심볼별 corr( gate_web step return, quote step return ) → Redis.

X_gw = (gw_mid_t - gw_mid_{t-step}) / gw_mid_{t-step}     # gate_web step return
X_qt = (qt_mid_t - qt_mid_{t-step}) / qt_mid_{t-step}     # quote step return
corr = Pearson(X_gw, X_qt) over 윈도우 안 모든 t

NOTE: 모듈 이름은 historical 으로 'price_change_gap_corr' 유지 — 실제 metric 은
두 거래소 return 의 직접 corr (이전의 Δgap 버전은 spurious ±1 문제로 폐기).

부호 해석:
  corr ≈ +1 → 두 거래소 거의 동기화 (정상 — 같은 underlying tracking)
  corr 작음 → 두 거래소 어긋남 (의미 있는 신호: 한쪽이 lag 또는 noise)
  corr < 0 → 반대 방향 (드물거나 illiquid 한쪽 artifact)

QuestDB 가 corr() 미지원이라 Pearson 을 SQL 합성:
  corr = (E[XY] - E[X]E[Y]) / (stddev_pop(X) * stddev_pop(Y))

Redis key: {prefix}:price_change_gap_corr:gate_web:{quote}:{window_minutes}

Blob:
  {
    "base_exchange": "gate_web",
    "quote_exchange": "binance",
    "window_minutes": 10,
    "return_seconds": 5,
    "updated_at_ms": ...,
    "corr_by_symbol": {"BTC_USDT": 0.42, ...},
    "n_samples_by_symbol": {"BTC_USDT": 120, ...}
  }

신뢰도가 낮은 심볼 (n < MIN_SAMPLES, stddev == 0) 은 SQL 단계에서 drop.
"""

from __future__ import annotations

import json
import time
from datetime import datetime, timedelta
from typing import Any

import redis
import structlog

from market_state_updater.jobs.common import (
    bookticker_table,
    bookticker_table_for_gap_base,
    now_ms,
)
from market_state_updater.questdb import questdb_exec

logger = structlog.get_logger(__name__)

# corr 계산에 필요한 최소 sample 수. 미만은 SQL 단계에서 drop.
MIN_SAMPLES = 30

# stddev 가 이 값 이하면 numerical 불안정 → drop. 한쪽 거래소가 사실상
# 정지된 sparse 심볼을 거르는 안전장치.
MIN_STDDEV = 1e-10

# 양 쪽 return 이 0 이 아닌 sample 이 이 수 이상이어야 함.
# sparse 심볼 (대부분 sample 이 FILL(PREV) 로 0 인데 한두 sample 만 같이 튀어서
# spurious corr=±1 나는 케이스) 방지. 진짜 움직임이 충분해야 의미 있음.
MIN_NONZERO_SAMPLES = 10

# Pearson corr 은 정의상 [-1, 1] 인데, QuestDB stddev_pop 의 부동소수점
# 한계로 |corr| 가 1 을 살짝 넘어 나올 수 있음 (catastrophic cancellation).
# 이 임계를 넘으면 artifact 로 보고 drop.
MAX_ABS_CORR = 1.001


def _format_qdb_ts(dt: datetime) -> str:
    """QuestDB 가 받는 ISO 형식. microsecond 포함."""
    return dt.strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def build_query(
    quote_exchange: str,
    window_minutes: int,
    return_seconds: int,
    end_ts: datetime | None = None,
    symbol: str | None = None,
) -> str:
    """gate_web 1-step return ↔ gate_web↔quote Δgap 의 Pearson corr SQL.

    base 는 항상 gate_web (gate_webbookticker). quote 만 가변.

    end_ts 가 None 이면 윈도우 끝 = now() (매 cycle 기본 동작).
    end_ts 명시 시 그 시점이 윈도우 끝 (backfill 용). symbol 명시 시 단일 심볼만.
    """
    window_minutes = max(int(window_minutes), 1)
    return_seconds = max(int(return_seconds), 1)
    base_tbl = bookticker_table_for_gap_base("gate_web")  # gate_webbookticker
    quote_tbl = bookticker_table(quote_exchange)
    if end_ts is None:
        time_filter = f"timestamp > dateadd('m', -{window_minutes}, now())"
    else:
        end_s = _format_qdb_ts(end_ts)
        start_s = _format_qdb_ts(end_ts - timedelta(minutes=window_minutes))
        time_filter = f"timestamp > '{start_s}' AND timestamp <= '{end_s}'"
    symbol_filter = f"\n      AND symbol = '{symbol}'" if symbol else ""
    return f"""
WITH gw AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS gw_mid,
           timestamp
    FROM {base_tbl}
    WHERE {time_filter}{symbol_filter}
    SAMPLE BY {return_seconds}s FILL(PREV)
),
qt AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS qt_mid,
           timestamp
    FROM {quote_tbl}
    WHERE {time_filter}{symbol_filter}
    SAMPLE BY {return_seconds}s FILL(PREV)
),
returns AS (
    SELECT g.symbol,
           (g.gw_mid - gp.prev_gw_mid) / gp.prev_gw_mid AS x_gw,
           (q.qt_mid - qp.prev_qt_mid) / qp.prev_qt_mid AS x_qt
    FROM gw g
    JOIN qt q  ON g.symbol = q.symbol  AND g.timestamp = q.timestamp
    JOIN (
        SELECT symbol,
               gw_mid AS prev_gw_mid,
               dateadd('s', {return_seconds}, timestamp) AS timestamp
        FROM gw
    ) gp ON g.symbol = gp.symbol AND g.timestamp = gp.timestamp
    JOIN (
        SELECT symbol,
               qt_mid AS prev_qt_mid,
               dateadd('s', {return_seconds}, timestamp) AS timestamp
        FROM qt
    ) qp ON g.symbol = qp.symbol AND g.timestamp = qp.timestamp
),
stats AS (
    SELECT symbol,
           avg(x_gw * x_qt)                                       AS exy,
           avg(x_gw)                                              AS ex,
           avg(x_qt)                                              AS ey,
           stddev_pop(x_gw)                                       AS sx,
           stddev_pop(x_qt)                                       AS sy,
           count()                                                AS n,
           sum(CASE WHEN x_gw != 0 THEN 1 ELSE 0 END)             AS nz_gw,
           sum(CASE WHEN x_qt != 0 THEN 1 ELSE 0 END)             AS nz_qt
    FROM returns
    GROUP BY symbol
)
SELECT symbol,
       (exy - ex * ey) / (sx * sy) AS corr,
       n
FROM stats
WHERE n >= {MIN_SAMPLES}
  AND sx > {MIN_STDDEV}
  AND sy > {MIN_STDDEV}
  AND nz_gw >= {MIN_NONZERO_SAMPLES}    -- gw 가 충분히 자주 움직여야 (sparse 방지)
  AND nz_qt >= {MIN_NONZERO_SAMPLES}    -- quote 도 마찬가지
ORDER BY symbol;
""".strip()


def parse_dataset(
    payload: dict[str, Any],
) -> tuple[dict[str, float], dict[str, int]]:
    """QuestDB 응답 → (symbol → corr, symbol → n)."""
    columns = payload.get("columns") or []
    dataset = payload.get("dataset") or []
    symbol_idx: int | None = None
    corr_idx: int | None = None
    n_idx: int | None = None
    for idx, col in enumerate(columns):
        name = (col or {}).get("name") if isinstance(col, dict) else None
        if name == "symbol":
            symbol_idx = idx
        elif name == "corr":
            corr_idx = idx
        elif name == "n":
            n_idx = idx
    if symbol_idx is None or corr_idx is None or n_idx is None:
        raise ValueError("QuestDB response missing symbol/corr/n column")
    corrs: dict[str, float] = {}
    counts: dict[str, int] = {}
    for row in dataset:
        if not isinstance(row, list):
            continue
        sym = str(row[symbol_idx] or "").strip()
        if not sym:
            continue
        c = row[corr_idx]
        n = row[n_idx]
        if c is None or n is None:
            continue
        try:
            cv = float(c)
        except (TypeError, ValueError):
            continue
        if cv != cv or abs(cv) > MAX_ABS_CORR:  # NaN 또는 [-1, 1] 밖 = artifact
            continue
        try:
            corrs[sym] = cv
            counts[sym] = int(n)
        except (TypeError, ValueError):
            continue
    return corrs, counts


def fetch(
    questdb_url: str,
    quote_exchange: str,
    window_minutes: int,
    return_seconds: int,
) -> tuple[dict[str, float], dict[str, int]]:
    payload = questdb_exec(
        questdb_url, build_query(quote_exchange, window_minutes, return_seconds)
    )
    return parse_dataset(payload)


def run(
    questdb_url: str,
    redis_client: redis.Redis,
    key_prefix: str,
    quote_exchange: str,
    window_minutes: int,
    return_seconds: int,
) -> bool:
    log = logger.bind(
        job="price_change_gap_corr",
        quote=quote_exchange,
        window=window_minutes,
        ret=return_seconds,
    )
    t0 = time.monotonic()
    try:
        corrs, counts = fetch(
            questdb_url, quote_exchange, window_minutes, return_seconds
        )
    except Exception as e:  # noqa: BLE001
        log.error(
            "questdb_failed",
            error=str(e),
            query_ms=int((time.monotonic() - t0) * 1000),
        )
        return False
    query_ms = int((time.monotonic() - t0) * 1000)
    blob = {
        "base_exchange": "gate_web",
        "quote_exchange": quote_exchange,
        "window_minutes": window_minutes,
        "return_seconds": return_seconds,
        "updated_at_ms": now_ms(),
        "corr_by_symbol": corrs,
        "n_samples_by_symbol": counts,
    }
    key = f"{key_prefix}:price_change_gap_corr:gate_web:{quote_exchange}:{window_minutes}"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        log.error("redis_set_failed", error=str(e), key=key)
        return False
    if not corrs:
        log.info("updated_empty", key=key, query_ms=query_ms)
        return True
    vals = list(corrs.values())
    mean = sum(vals) / len(vals)
    abs_sorted = sorted(corrs.items(), key=lambda x: abs(x[1]), reverse=True)
    top_sym, top_v = abs_sorted[0]
    log.info(
        "updated",
        symbols=len(corrs),
        mean_corr=round(mean, 4),
        top_abs_corr=round(top_v, 4),
        top_symbol=top_sym,
        query_ms=query_ms,
    )
    return True

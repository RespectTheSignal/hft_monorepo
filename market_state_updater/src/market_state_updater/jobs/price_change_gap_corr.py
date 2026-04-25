"""심볼별 corr( gate_web mid 의 1-step return, gate_web↔quote mid gap ) → Redis.

X_t = (gw_mid_t - gw_mid_{t-step}) / gw_mid_{t-step}
Y_t = (gw_mid_t - quote_mid_t) / gw_mid_t
corr = corr(X, Y) over 윈도우 안 모든 t

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

# stddev 가 이 값 이하면 numerical 불안정 → drop. FILL(PREV) 로 대부분 sample 이
# 같은 값이라 stddev ≈ 0 인 sparse 심볼을 거르는 안전장치.
MIN_STDDEV = 1e-10

# Pearson corr 은 정의상 [-1, 1] 인데, QuestDB stddev_pop 의 부동소수점
# 한계로 |corr| 가 1 을 살짝 넘어 나올 수 있음 (catastrophic cancellation).
# 이 임계를 넘으면 artifact 로 보고 drop.
MAX_ABS_CORR = 1.001


def build_query(
    quote_exchange: str,
    window_minutes: int,
    return_seconds: int,
) -> str:
    """gate_web 1-step return ↔ gate_web↔quote gap 의 Pearson corr SQL.

    base 는 항상 gate_web (gate_webbookticker). quote 만 가변.
    """
    window_minutes = max(int(window_minutes), 1)
    return_seconds = max(int(return_seconds), 1)
    base_tbl = bookticker_table_for_gap_base("gate_web")  # gate_webbookticker
    quote_tbl = bookticker_table(quote_exchange)
    return f"""
WITH gw AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS gw_mid,
           timestamp
    FROM {base_tbl}
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {return_seconds}s FILL(PREV)
),
qt AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS qt_mid,
           timestamp
    FROM {quote_tbl}
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {return_seconds}s FILL(PREV)
),
gw_prev AS (
    SELECT symbol,
           gw_mid AS prev_gw_mid,
           dateadd('s', {return_seconds}, timestamp) AS timestamp
    FROM gw
),
joined AS (
    SELECT g.symbol,
           (g.gw_mid - p.prev_gw_mid) / p.prev_gw_mid AS x_return,
           (g.gw_mid - q.qt_mid) / g.gw_mid AS y_gap
    FROM gw g
    JOIN qt q ON g.symbol = q.symbol AND g.timestamp = q.timestamp
    JOIN gw_prev p ON g.symbol = p.symbol AND g.timestamp = p.timestamp
),
stats AS (
    SELECT symbol,
           avg(x_return * y_gap) AS exy,
           avg(x_return)         AS ex,
           avg(y_gap)            AS ey,
           stddev_pop(x_return)  AS sx,
           stddev_pop(y_gap)     AS sy,
           count()               AS n
    FROM joined
    GROUP BY symbol
)
SELECT symbol,
       (exy - ex * ey) / (sx * sy) AS corr,
       n
FROM stats
WHERE n >= {MIN_SAMPLES} AND sx > {MIN_STDDEV} AND sy > {MIN_STDDEV}
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
    try:
        corrs, counts = fetch(
            questdb_url, quote_exchange, window_minutes, return_seconds
        )
    except Exception as e:  # noqa: BLE001
        log.error("questdb_failed", error=str(e))
        return False
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
        log.info("updated_empty", key=key)
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
    )
    return True

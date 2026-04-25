"""심볼별 1-lag autocorrelation of step returns → Redis. Microstructure noise indicator.

X = r_t   = (mid_t - mid_{t-step}) / mid_{t-step}
Y = r_{t-step}  (직전 step return)
ρ₁ = corr(X, Y) per symbol

해석:
  ρ₁ < 0  → bid-ask bounce / overshoot-revert → microstructure noise 강함
  ρ₁ ≈ 0  → efficient (random walk)
  ρ₁ > 0  → momentum (드물지만 가능)

|ρ₁| 가 클수록 noisy. 절대값 자체보다 "다른 거래소 / 다른 시간 대비 변화" 가 의미.

거래소마다 별도 schedule + 별도 Redis key — gate, gate_web, binance 비교 가능.

Redis key: {prefix}:{exchange}:{window_minutes}

Blob:
  {
    "exchange": "gate",
    "window_minutes": 5,
    "return_seconds": 5,
    "updated_at_ms": ...,
    "min_samples": 30,
    "autocorr_by_symbol":   {"BTC_USDT": -0.12, ...},
    "n_samples_by_symbol":  {"BTC_USDT": 58, ...}
  }
"""

from __future__ import annotations

import json
import time
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

DEFAULT_MIN_SAMPLES = 30
# r_t == 0 인 sample (FILL(PREV) 로 대부분 정지) 가 너무 많으면 spurious.
MIN_NONZERO_SAMPLES = 10


def _table_for_exchange(exchange: str) -> str:
    return (
        bookticker_table_for_gap_base(exchange)
        if exchange == "gate_web"
        else bookticker_table(exchange)
    )


def build_query(
    exchange: str,
    window_minutes: int,
    return_seconds: int,
    min_samples: int = DEFAULT_MIN_SAMPLES,
) -> str:
    """1-lag return autocorr per symbol.

    `dateadd` self-shift 두 번으로 (t, t-step, t-2*step) 세 시점 mid 를 align.
    """
    window_minutes = max(int(window_minutes), 1)
    return_seconds = max(int(return_seconds), 1)
    min_samples = max(int(min_samples), 2)
    table = _table_for_exchange(exchange)
    return f"""
WITH ex AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS mid,
           timestamp
    FROM {table}
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {return_seconds}s FILL(PREV)
),
ex_p1 AS (
    SELECT symbol, mid AS m1,
           dateadd('s', {return_seconds}, timestamp) AS timestamp
    FROM ex
),
ex_p2 AS (
    SELECT symbol, mid AS m2,
           dateadd('s', {2 * return_seconds}, timestamp) AS timestamp
    FROM ex
),
returns AS (
    SELECT e.symbol,
           (e.mid - p1.m1) / p1.m1 AS r_t,
           (p1.m1 - p2.m2) / p2.m2 AS r_prev
    FROM ex e
    JOIN ex_p1 p1 ON e.symbol = p1.symbol AND e.timestamp = p1.timestamp
    JOIN ex_p2 p2 ON e.symbol = p2.symbol AND e.timestamp = p2.timestamp
),
agg AS (
    SELECT symbol,
           corr(r_t, r_prev) AS autocorr,
           count() AS n_samples,
           sum(CASE WHEN r_t != 0 THEN 1 ELSE 0 END) AS nz_t,
           sum(CASE WHEN r_prev != 0 THEN 1 ELSE 0 END) AS nz_p
    FROM returns
    GROUP BY symbol
)
SELECT symbol, autocorr, n_samples
FROM agg
WHERE n_samples >= {min_samples}
  AND nz_t >= {MIN_NONZERO_SAMPLES}
  AND nz_p >= {MIN_NONZERO_SAMPLES}
ORDER BY symbol;
""".strip()


def parse_dataset(
    payload: dict[str, Any],
) -> tuple[dict[str, float], dict[str, int]]:
    columns = payload.get("columns") or []
    dataset = payload.get("dataset") or []
    sym_idx: int | None = None
    a_idx: int | None = None
    n_idx: int | None = None
    for i, col in enumerate(columns):
        name = (col or {}).get("name") if isinstance(col, dict) else None
        if name == "symbol":
            sym_idx = i
        elif name == "autocorr":
            a_idx = i
        elif name == "n_samples":
            n_idx = i
    if sym_idx is None or a_idx is None or n_idx is None:
        raise ValueError("QuestDB response missing symbol/autocorr/n_samples column")
    autos: dict[str, float] = {}
    counts: dict[str, int] = {}
    for row in dataset:
        if not isinstance(row, list):
            continue
        sym = str(row[sym_idx] or "").strip()
        if not sym:
            continue
        a = row[a_idx]
        n = row[n_idx]
        if a is None or n is None:
            continue
        try:
            av = float(a)
        except (TypeError, ValueError):
            continue
        if av != av:  # NaN
            continue
        try:
            autos[sym] = av
            counts[sym] = int(n)
        except (TypeError, ValueError):
            continue
    return autos, counts


def fetch(
    questdb_url: str,
    exchange: str,
    window_minutes: int,
    return_seconds: int,
    min_samples: int = DEFAULT_MIN_SAMPLES,
) -> tuple[dict[str, float], dict[str, int]]:
    sql = build_query(exchange, window_minutes, return_seconds, min_samples)
    payload = questdb_exec(questdb_url, sql)
    return parse_dataset(payload)


def run(
    questdb_url: str,
    redis_client: redis.Redis,
    key_prefix: str,
    exchange: str,
    window_minutes: int,
    return_seconds: int,
    min_samples: int = DEFAULT_MIN_SAMPLES,
) -> bool:
    log = logger.bind(
        job="return_autocorr",
        exchange=exchange,
        window=window_minutes,
        ret=return_seconds,
    )
    t0 = time.monotonic()
    try:
        autos, counts = fetch(
            questdb_url, exchange, window_minutes, return_seconds, min_samples
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
        "exchange": exchange,
        "window_minutes": window_minutes,
        "return_seconds": return_seconds,
        "updated_at_ms": now_ms(),
        "min_samples": min_samples,
        "autocorr_by_symbol": autos,
        "n_samples_by_symbol": counts,
    }
    key = f"{key_prefix}:{exchange}:{window_minutes}"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        log.error("redis_set_failed", error=str(e), key=key, query_ms=query_ms)
        return False

    if not autos:
        log.info("updated_empty", key=key, query_ms=query_ms)
        return True

    vals = list(autos.values())
    mean = sum(vals) / len(vals)
    n_neg = sum(1 for v in vals if v < 0)
    most_neg_sym, most_neg_v = min(autos.items(), key=lambda x: x[1])
    log.info(
        "updated",
        symbols=len(autos),
        mean_autocorr=round(mean, 4),
        pct_negative=round(100 * n_neg / len(vals), 1),
        most_negative=round(most_neg_v, 4),
        most_negative_symbol=most_neg_sym,
        query_ms=query_ms,
    )
    return True

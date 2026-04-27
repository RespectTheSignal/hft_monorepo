"""심볼별 Variance Ratio (Lo-MacKinlay) per (exchange, window) → Redis.

VR(k) = Var(r_k) / [k · Var(r_1)]
  r_1 = base_seconds 단위 step return
  r_k = k * base_seconds 단위 step return

해석:
  VR << 1 → mean reverting (bid-ask bounce / overshoot revert)
  VR ≈  1 → random walk (efficient)
  VR >> 1 → trending (momentum)

여러 k 값을 한 schedule 에서 처리 — k 별로 별도 SQL 호출 후 한 blob 에 nested merge.
SQL 무게 분산 + 풍부한 timescale profile 동시 제공.

Redis key: {prefix}:{exchange}:{window_minutes}

Blob:
  {
    "exchange": "gate",
    "window_minutes": 5,
    "base_seconds": 5,
    "k_values": [2, 5, 10],
    "updated_at_ms": ...,
    "min_samples": 30,
    "vr_by_symbol_by_k": {
      "BTC_USDT": {"2": 0.95, "5": 1.20, "10": 1.05},
      ...
    },
    "n_samples_by_symbol": {"BTC_USDT": 12, ...}   # 가장 큰 k 의 sample 수
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

# 큰 k 일수록 sample 수 ↓ (W*60 / (k*base_seconds)). 30 으로 두면 큰 k 가 거의 다 drop.
# 10 은 변동성 추정에 통계적으로 borderline 하지만 timescale profile 보기엔 충분.
DEFAULT_MIN_SAMPLES = 10
DEFAULT_K_VALUES: tuple[int, ...] = (2, 5, 10)


def _table_for_exchange(exchange: str) -> str:
    return (
        bookticker_table_for_gap_base(exchange)
        if exchange == "gate_web"
        else bookticker_table(exchange)
    )


def build_query(
    exchange: str,
    window_minutes: int,
    base_seconds: int,
    k: int,
    min_samples: int = DEFAULT_MIN_SAMPLES,
) -> str:
    """한 k 에 대한 VR(k) SQL.

    r_1 = SAMPLE BY base_seconds, r_k = SAMPLE BY (k * base_seconds).
    var_pop = stddev_pop(r)^2 (QuestDB var_pop 안 쓰고 일관성 위해 stddev_pop 합성).
    """
    window_minutes = max(int(window_minutes), 1)
    base_seconds = max(int(base_seconds), 1)
    k = max(int(k), 2)
    min_samples = max(int(min_samples), 2)
    table = _table_for_exchange(exchange)
    k_seconds = k * base_seconds
    return f"""
WITH ex_base AS (
    SELECT symbol, last((ask_price + bid_price) / 2.0) AS mid, timestamp
    FROM {table}
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {base_seconds}s FILL(PREV)
),
ex_base_prev AS (
    SELECT symbol, mid AS prev_mid,
           dateadd('s', {base_seconds}, timestamp) AS timestamp
    FROM ex_base
),
r1 AS (
    SELECT s.symbol, (s.mid - p.prev_mid) / p.prev_mid AS r
    FROM ex_base s
    JOIN ex_base_prev p ON s.symbol = p.symbol AND s.timestamp = p.timestamp
),
ex_k AS (
    SELECT symbol, last((ask_price + bid_price) / 2.0) AS mid, timestamp
    FROM {table}
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {k_seconds}s FILL(PREV)
),
ex_k_prev AS (
    SELECT symbol, mid AS prev_mid,
           dateadd('s', {k_seconds}, timestamp) AS timestamp
    FROM ex_k
),
rk AS (
    SELECT s.symbol, (s.mid - p.prev_mid) / p.prev_mid AS r
    FROM ex_k s
    JOIN ex_k_prev p ON s.symbol = p.symbol AND s.timestamp = p.timestamp
),
v1 AS (
    SELECT symbol, stddev_pop(r) AS sd FROM r1 GROUP BY symbol
),
vk AS (
    SELECT symbol,
           stddev_pop(r) AS sd,
           count() AS n
    FROM rk GROUP BY symbol
),
agg AS (
    SELECT vk.symbol,
           (vk.sd * vk.sd) / ({k} * v1.sd * v1.sd) AS vr,
           vk.n
    FROM vk JOIN v1 ON vk.symbol = v1.symbol
    WHERE v1.sd > 0
)
SELECT symbol, vr, n FROM agg
WHERE n >= {min_samples}
ORDER BY symbol;
""".strip()


def parse_dataset(
    payload: dict[str, Any],
) -> tuple[dict[str, float], dict[str, int]]:
    columns = payload.get("columns") or []
    dataset = payload.get("dataset") or []
    sym_idx: int | None = None
    vr_idx: int | None = None
    n_idx: int | None = None
    for i, col in enumerate(columns):
        name = (col or {}).get("name") if isinstance(col, dict) else None
        if name == "symbol":
            sym_idx = i
        elif name == "vr":
            vr_idx = i
        elif name == "n":
            n_idx = i
    if sym_idx is None or vr_idx is None or n_idx is None:
        raise ValueError("QuestDB response missing symbol/vr/n column")
    vrs: dict[str, float] = {}
    counts: dict[str, int] = {}
    for row in dataset:
        if not isinstance(row, list):
            continue
        sym = str(row[sym_idx] or "").strip()
        if not sym:
            continue
        v = row[vr_idx]
        n = row[n_idx]
        if v is None or n is None:
            continue
        try:
            vv = float(v)
        except (TypeError, ValueError):
            continue
        if vv != vv:  # NaN
            continue
        try:
            vrs[sym] = vv
            counts[sym] = int(n)
        except (TypeError, ValueError):
            continue
    return vrs, counts


def fetch_one_k(
    questdb_url: str,
    exchange: str,
    window_minutes: int,
    base_seconds: int,
    k: int,
    min_samples: int = DEFAULT_MIN_SAMPLES,
) -> tuple[dict[str, float], dict[str, int]]:
    sql = build_query(exchange, window_minutes, base_seconds, k, min_samples)
    payload = questdb_exec(questdb_url, sql)
    return parse_dataset(payload)


def run(
    questdb_url: str,
    redis_client: redis.Redis,
    key_prefix: str,
    exchange: str,
    window_minutes: int,
    base_seconds: int,
    k_values: tuple[int, ...] = DEFAULT_K_VALUES,
    min_samples: int = DEFAULT_MIN_SAMPLES,
) -> bool:
    """k_values 각각 별도 SQL → merge → blob 1개."""
    log = logger.bind(
        job="variance_ratio",
        exchange=exchange,
        window=window_minutes,
        base_secs=base_seconds,
        k_values=list(k_values),
    )
    t0 = time.monotonic()
    vr_by_k: dict[int, dict[str, float]] = {}
    n_by_k: dict[int, dict[str, int]] = {}
    try:
        for k in k_values:
            vrs, counts = fetch_one_k(
                questdb_url, exchange, window_minutes, base_seconds, k, min_samples
            )
            vr_by_k[k] = vrs
            n_by_k[k] = counts
    except Exception as e:  # noqa: BLE001
        log.error(
            "questdb_failed",
            error=str(e),
            query_ms=int((time.monotonic() - t0) * 1000),
        )
        return False
    query_ms = int((time.monotonic() - t0) * 1000)

    # Symbol 합집합 → nested dict 빌드. 한 k 라도 통과한 심볼 다 포함 (partial OK).
    # 큰 k 는 sample 부족으로 자주 drop 되니 nested dict 에서 그 k 만 빠짐.
    all_syms: set[str] = set()
    for d in vr_by_k.values():
        all_syms.update(d.keys())

    vr_by_symbol_by_k: dict[str, dict[str, float]] = {}
    n_samples_by_symbol: dict[str, int] = {}
    for s in sorted(all_syms):
        per_k = {str(k): vr_by_k[k][s] for k in k_values if s in vr_by_k[k]}
        if not per_k:
            continue
        vr_by_symbol_by_k[s] = per_k
        # n_samples 는 통과한 k 들 중 가장 작은 (보수적 신뢰도)
        n_samples_by_symbol[s] = min(
            n_by_k[k].get(s, 0) for k in k_values if s in vr_by_k[k]
        )

    blob = {
        "exchange": exchange,
        "window_minutes": window_minutes,
        "base_seconds": base_seconds,
        "k_values": list(k_values),
        "updated_at_ms": now_ms(),
        "min_samples": min_samples,
        "vr_by_symbol_by_k": vr_by_symbol_by_k,
        "n_samples_by_symbol": n_samples_by_symbol,
    }
    key = f"{key_prefix}:{exchange}:{window_minutes}"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        log.error("redis_set_failed", error=str(e), key=key, query_ms=query_ms)
        return False

    if not vr_by_symbol_by_k:
        log.info("updated_empty", key=key, query_ms=query_ms)
        return True

    # 가장 작은 k (보통 k=2) 의 mean / 분포 로깅 — 그 k 통과한 심볼만
    smallest_k = str(min(k_values))
    vrs_at_smallest = [
        d[smallest_k] for d in vr_by_symbol_by_k.values() if smallest_k in d
    ]
    if vrs_at_smallest:
        mean = sum(vrs_at_smallest) / len(vrs_at_smallest)
        n_lt1 = sum(1 for v in vrs_at_smallest if v < 1)
        n_gt1 = sum(1 for v in vrs_at_smallest if v > 1)
        log.info(
            "updated",
            symbols=len(vr_by_symbol_by_k),
            symbols_at_smallest_k=len(vrs_at_smallest),
            mean_vr_smallest_k=round(mean, 4),
            pct_mean_revert=round(100 * n_lt1 / len(vrs_at_smallest), 1),
            pct_trending=round(100 * n_gt1 / len(vrs_at_smallest), 1),
            query_ms=query_ms,
        )
    else:
        log.info(
            "updated",
            symbols=len(vr_by_symbol_by_k),
            symbols_at_smallest_k=0,
            query_ms=query_ms,
        )
    return True

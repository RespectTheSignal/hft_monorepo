"""심볼별 윈도우 내 mid-price 변화율 → Redis.

price_change = (last_mid - first_mid) / first_mid

Redis key: {prefix}:{source}:{window_minutes}
  e.g. gate_hft:price_change:gate:5
       gate_hft:price_change:binance:60

Blob:
  {
    "source": "gate",
    "window_minutes": 5,
    "updated_at_ms": ...,
    "price_change_by_symbol": {"BTC_USDT": -0.0012, ...}
  }
"""

from __future__ import annotations

import json
import time

import redis
import structlog

from market_state_updater.jobs.common import (
    bookticker_table,
    bookticker_table_for_gap_base,
    now_ms,
)
from market_state_updater.questdb import questdb_exec

logger = structlog.get_logger(__name__)

DEFAULT_PRICE_CHANGE_PREFIX = "gate_hft:price_change"


def build_query(table: str, window_minutes: int) -> str:
    """심볼별 (last_mid - first_mid) / first_mid. 2틱 미만은 제외."""
    window_minutes = max(int(window_minutes), 1)
    return f"""
WITH mids AS (
    SELECT symbol,
           first((ask_price + bid_price) / 2.0) AS first_mid,
           last((ask_price + bid_price) / 2.0) AS last_mid,
           count() AS n_ticks
    FROM {table}
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    GROUP BY symbol
)
SELECT symbol,
       (last_mid - first_mid) / first_mid AS price_change,
       first_mid,
       last_mid,
       n_ticks
FROM mids
WHERE first_mid > 0 AND n_ticks >= 2
ORDER BY abs(price_change) DESC;
""".strip()


def fetch(
    questdb_url: str,
    table: str,
    window_minutes: int,
) -> dict[str, float]:
    """symbol → price_change ratio."""
    query = build_query(table, window_minutes)
    payload = questdb_exec(questdb_url, query)
    columns = payload.get("columns") or []
    dataset = payload.get("dataset") or []
    symbol_idx: int | None = None
    change_idx: int | None = None
    for idx, col in enumerate(columns):
        name = (col or {}).get("name") if isinstance(col, dict) else None
        if name == "symbol":
            symbol_idx = idx
        elif name == "price_change":
            change_idx = idx
    if symbol_idx is None or change_idx is None:
        return {}
    result: dict[str, float] = {}
    for row in dataset:
        if not isinstance(row, list):
            continue
        sym = str(row[symbol_idx] or "").strip()
        if not sym:
            continue
        try:
            v = row[change_idx]
            if v is not None:
                result[sym] = float(v)
        except (TypeError, ValueError):
            pass
    return result


def table_for_source(source: str) -> str:
    return (
        bookticker_table_for_gap_base(source)
        if source == "gate_web"
        else bookticker_table(source)
    )


def run(
    questdb_url: str,
    redis_client: redis.Redis,
    key_prefix: str,
    source: str,
    window_minutes: int,
) -> bool:
    """source: 'gate' → gate_bookticker, 'binance' → binance_bookticker, ..."""
    log = logger.bind(job="price_change", source=source, window=window_minutes)
    table = table_for_source(source)
    t0 = time.monotonic()
    try:
        changes = fetch(questdb_url, table, window_minutes)
    except Exception as e:  # noqa: BLE001
        log.error(
            "questdb_failed",
            error=str(e),
            table=table,
            query_ms=int((time.monotonic() - t0) * 1000),
        )
        return False
    query_ms = int((time.monotonic() - t0) * 1000)
    blob = {
        "source": source,
        "window_minutes": window_minutes,
        "updated_at_ms": now_ms(),
        "price_change_by_symbol": changes,
    }
    key = f"{key_prefix}:{source}:{window_minutes}"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        log.error("redis_set_failed", error=str(e), key=key)
        return False
    if not changes:
        log.info("updated_empty", key=key, query_ms=query_ms)
        return True
    vals = list(changes.values())
    mean = sum(vals) / len(vals)
    max_sym, max_v = max(changes.items(), key=lambda x: x[1])
    min_sym, min_v = min(changes.items(), key=lambda x: x[1])
    log.info(
        "updated",
        symbols=len(changes),
        mean_bp=round(mean * 10000, 1),
        max_bp=round(max_v * 10000, 1),
        max_symbol=max_sym,
        min_bp=round(min_v * 10000, 1),
        min_symbol=min_sym,
        query_ms=query_ms,
    )
    return True

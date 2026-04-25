"""gate_bookticker vs gate_webbookticker 심볼별 평균 relative spread → Redis.

Redis key: {prefix}:gate_gate_web_spreads:{window_minutes}

Blob shape:
  {
    "window_minutes": 10,
    "updated_at_ms": ...,
    "avg_spread_gate_bookticker_by_symbol":   {"BTC_USDT": 0.0002, ...},
    "avg_spread_gate_webbookticker_by_symbol":{"BTC_USDT": 0.0003, ...}
  }
  spread = mean (ask-bid)/mid over 1s buckets where 두 feed 모두 존재.
"""

from __future__ import annotations

import json
from typing import Any

import redis
import structlog

from market_state_updater.jobs.common import (
    FILL_PREV_LOOKBACK_MINUTES,
    now_ms,
    sample_interval_for_window,
)
from market_state_updater.questdb import questdb_exec

logger = structlog.get_logger(__name__)


def build_query(window_minutes: int) -> str:
    """1m/5m 는 FILL(PREV) 로 stale forward-fill, 그 이상은 raw SAMPLE BY."""
    window_minutes = max(int(window_minutes), 1)
    sample = sample_interval_for_window(window_minutes)
    use_fill_prev = window_minutes <= 5
    if use_fill_prev:
        lookback = max(FILL_PREV_LOOKBACK_MINUTES, window_minutes)
        return f"""
WITH g_full AS (
    SELECT symbol,
           last((ask_price - bid_price) / ((ask_price + bid_price) / 2.0)) AS spread_1s,
           timestamp
    FROM gate_bookticker
    WHERE timestamp > dateadd('m', -{lookback}, now())
    SAMPLE BY {sample} FILL(PREV)
),
w_full AS (
    SELECT symbol,
           last((ask_price - bid_price) / ((ask_price + bid_price) / 2.0)) AS spread_1s,
           timestamp
    FROM gate_webbookticker
    WHERE timestamp > dateadd('m', -{lookback}, now())
    SAMPLE BY {sample} FILL(PREV)
),
g AS (
    SELECT * FROM g_full WHERE timestamp > dateadd('m', -{window_minutes}, now())
),
w AS (
    SELECT * FROM w_full WHERE timestamp > dateadd('m', -{window_minutes}, now())
),
sp AS (
    SELECT b.symbol,
           avg(b.spread_1s) AS avg_spread_gate_bookticker,
           avg(web.spread_1s) AS avg_spread_gate_webbookticker
    FROM g b
    JOIN w web ON b.symbol = web.symbol AND b.timestamp = web.timestamp
    GROUP BY b.symbol
)
SELECT * FROM sp ORDER BY symbol;
""".strip()
    return f"""
WITH g AS (
    SELECT symbol,
           last((ask_price - bid_price) / ((ask_price + bid_price) / 2.0)) AS spread_1s,
           timestamp
    FROM gate_bookticker
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {sample}
),
w AS (
    SELECT symbol,
           last((ask_price - bid_price) / ((ask_price + bid_price) / 2.0)) AS spread_1s,
           timestamp
    FROM gate_webbookticker
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {sample}
),
sp AS (
    SELECT b.symbol,
           avg(b.spread_1s) AS avg_spread_gate_bookticker,
           avg(web.spread_1s) AS avg_spread_gate_webbookticker
    FROM g b
    JOIN w web ON b.symbol = web.symbol AND b.timestamp = web.timestamp
    GROUP BY b.symbol
)
SELECT * FROM sp ORDER BY symbol;
""".strip()


def parse_dataset(
    payload: dict[str, Any],
) -> tuple[dict[str, float], dict[str, float]]:
    """QuestDB 응답 → (symbol → gate spread, symbol → gate_web spread)."""
    columns = payload.get("columns") or []
    dataset = payload.get("dataset") or []
    symbol_idx: int | None = None
    gate_idx: int | None = None
    web_idx: int | None = None
    for idx, col in enumerate(columns):
        name = (col or {}).get("name") if isinstance(col, dict) else None
        if name == "symbol":
            symbol_idx = idx
        elif name == "avg_spread_gate_bookticker":
            gate_idx = idx
        elif name == "avg_spread_gate_webbookticker":
            web_idx = idx
    if symbol_idx is None or gate_idx is None or web_idx is None:
        raise ValueError(
            "QuestDB response missing symbol or avg_spread_gate_bookticker / avg_spread_gate_webbookticker"
        )
    max_idx = max(symbol_idx, gate_idx, web_idx)
    gate_spreads: dict[str, float] = {}
    web_spreads: dict[str, float] = {}
    for row in dataset:
        if not isinstance(row, list) or len(row) <= max_idx:
            continue
        symbol = str(row[symbol_idx] or "").strip()
        if not symbol:
            continue
        try:
            gsp = float(row[gate_idx]) if row[gate_idx] is not None else 0.0
        except (TypeError, ValueError):
            gsp = 0.0
        try:
            wsp = float(row[web_idx]) if row[web_idx] is not None else 0.0
        except (TypeError, ValueError):
            wsp = 0.0
        gate_spreads[symbol] = gsp
        web_spreads[symbol] = wsp
    return gate_spreads, web_spreads


def fetch(
    questdb_url: str,
    window_minutes: int,
) -> tuple[dict[str, float], dict[str, float]]:
    payload = questdb_exec(questdb_url, build_query(window_minutes))
    return parse_dataset(payload)


def run(
    questdb_url: str,
    redis_client: redis.Redis,
    key_prefix: str,
    window_minutes: int,
) -> bool:
    log = logger.bind(job="spread_pair", window=window_minutes)
    try:
        gate_bt, web_bt = fetch(questdb_url, window_minutes)
    except Exception as e:  # noqa: BLE001
        log.error("questdb_failed", error=str(e))
        return False
    symbols = set(gate_bt) & set(web_bt)
    gate_by = {s: gate_bt[s] for s in symbols}
    web_by = {s: web_bt[s] for s in symbols}
    blob = {
        "window_minutes": window_minutes,
        "updated_at_ms": now_ms(),
        "avg_spread_gate_bookticker_by_symbol": gate_by,
        "avg_spread_gate_webbookticker_by_symbol": web_by,
    }
    key = f"{key_prefix}:gate_gate_web_spreads:{window_minutes}"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        log.error("redis_set_failed", error=str(e), key=key)
        return False

    if not gate_by:
        log.info("updated_empty", key=key)
        return True

    mean_gate = sum(gate_by.values()) / len(gate_by)
    mean_web = sum(web_by.values()) / len(web_by)
    log.info(
        "updated",
        symbols=len(gate_by),
        mean_spread_gate_bp=round(mean_gate * 10000, 2),
        mean_spread_gate_web_bp=round(mean_web * 10000, 2),
    )
    return True

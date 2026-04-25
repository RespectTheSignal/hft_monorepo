"""gate_bookticker vs gate_webbookticker 심볼별 평균 mid-price gap → Redis.

Redis key: {prefix}:gate_gate_web_mid_gap:{window_minutes}

avg_mid_gap = (gate_bookticker_mid - gate_webbookticker_mid) / gate_bookticker_mid

Blob:
  {
    "window_minutes": 10,
    "updated_at_ms": ...,
    "avg_mid_gap_by_symbol": {"BTC_USDT": 0.0001, ...}
  }
"""

from __future__ import annotations

import json
from typing import Any

import redis
import structlog

from market_state_updater.jobs.common import (
    FILL_PREV_LOOKBACK_MINUTES,
    filter_valid_gaps,
    now_ms,
    sample_interval_for_window,
)
from market_state_updater.questdb import questdb_exec

logger = structlog.get_logger(__name__)


def build_query(window_minutes: int) -> str:
    window_minutes = max(int(window_minutes), 1)
    sample = sample_interval_for_window(window_minutes)
    use_fill_prev = window_minutes <= 5
    if use_fill_prev:
        lookback = max(FILL_PREV_LOOKBACK_MINUTES, window_minutes)
        return f"""
WITH g_full AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS gate_mid,
           timestamp
    FROM gate_bookticker
    WHERE timestamp > dateadd('m', -{lookback}, now())
    SAMPLE BY {sample} FILL(PREV)
),
w_full AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS web_mid,
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
al AS (
    SELECT g.symbol AS symbol,
           avg((g.gate_mid - w.web_mid) / g.gate_mid) AS avg_mid_gap
    FROM g
    INNER JOIN w ON g.symbol = w.symbol AND g.timestamp = w.timestamp
    GROUP BY g.symbol
)
SELECT * FROM al ORDER BY abs(avg_mid_gap) DESC;
""".strip()
    return f"""
WITH g AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS gate_mid,
           timestamp
    FROM gate_bookticker
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {sample}
),
w AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS web_mid,
           timestamp
    FROM gate_webbookticker
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {sample}
),
al AS (
    SELECT g.symbol AS symbol,
           avg((g.gate_mid - w.web_mid) / g.gate_mid) AS avg_mid_gap
    FROM g
    INNER JOIN w ON g.symbol = w.symbol AND g.timestamp = w.timestamp
    GROUP BY g.symbol
)
SELECT * FROM al ORDER BY abs(avg_mid_gap) DESC;
""".strip()


def parse_dataset(payload: dict[str, Any]) -> dict[str, float]:
    columns = payload.get("columns") or []
    dataset = payload.get("dataset") or []
    symbol_idx: int | None = None
    gap_idx: int | None = None
    for idx, col in enumerate(columns):
        name = (col or {}).get("name") if isinstance(col, dict) else None
        if name == "symbol":
            symbol_idx = idx
        elif name == "avg_mid_gap":
            gap_idx = idx
    if symbol_idx is None or gap_idx is None:
        return {}
    gaps: dict[str, float] = {}
    for row in dataset:
        if not isinstance(row, list):
            continue
        sym = str(row[symbol_idx] or "").strip()
        if not sym:
            continue
        try:
            v = row[gap_idx]
            if v is None:
                continue
            gaps[sym] = float(v)
        except (TypeError, ValueError):
            pass
    return gaps


def run(
    questdb_url: str,
    redis_client: redis.Redis,
    key_prefix: str,
    window_minutes: int,
) -> bool:
    log = logger.bind(job="gate_web_gap", window=window_minutes)
    try:
        payload = questdb_exec(questdb_url, build_query(window_minutes))
        gaps = parse_dataset(payload)
    except Exception as e:  # noqa: BLE001
        log.error("questdb_failed", error=str(e))
        return False
    valid = filter_valid_gaps(gaps)
    blob = {
        "window_minutes": window_minutes,
        "updated_at_ms": now_ms(),
        "avg_mid_gap_by_symbol": valid,
    }
    key = f"{key_prefix}:gate_gate_web_mid_gap:{window_minutes}"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        log.error("redis_set_failed", error=str(e), key=key)
        return False
    if not valid:
        log.info("updated_empty", key=key)
        return True

    mean = sum(valid.values()) / len(valid)
    max_sym, max_v = max(valid.items(), key=lambda x: x[1])
    min_sym, min_v = min(valid.items(), key=lambda x: x[1])
    log.info(
        "updated",
        symbols=len(valid),
        mean_bp=round(mean * 10000, 2),
        max_bp=round(max_v * 10000, 2),
        max_symbol=max_sym,
        min_bp=round(min_v * 10000, 2),
        min_symbol=min_sym,
    )
    return True

"""base vs quote 거래소 mid-price gap 계산 → Redis.

Redis key: {prefix}:{base}:{quote}:{window_minutes}
  e.g. gate_hft:market_gap:gate:binance:10
       gate_hft:market_gap:gate_web:binance:10
       gate_hft:market_gap:gate:bybit:30

Blob shape (Rust MarketGapState 와 호환):
  {
    "base_exchange": "gate",
    "quote_exchange": "binance",
    "window_minutes": 10,
    "updated_at_ms": 1234567890123,
    "avg_mid_gap_by_symbol": {"BTC_USDT": 0.001, ...},
    "avg_spread_by_symbol":  {"BTC_USDT": 0.0002, ...}
  }
  avg_spread_by_symbol = (ask-bid)/mid (base 거래소 기준).
"""

from __future__ import annotations

import json
from typing import Any

import redis
import structlog

from market_state_updater.jobs.common import (
    FILL_PREV_LOOKBACK_MINUTES,
    bookticker_table,
    bookticker_table_for_gap_base,
    filter_valid_gaps,
    now_ms,
    sample_interval_for_window,
)
from market_state_updater.questdb import questdb_exec

logger = structlog.get_logger(__name__)


def build_query(
    base_exchange: str,
    quote_exchange: str,
    window_minutes: int,
) -> str:
    """market_watcher.rs 와 동일 로직: base mid vs quote mid gap 심볼별.

    avg_spread = (ask-bid)/mid 는 base feed 만 (gate_bookticker / gate_webbookticker).
    1m/5m 는 FILL(PREV) + 확장 lookback 으로 sparse 심볼도 stale price forward-fill.
    """
    window_minutes = max(int(window_minutes), 1)
    base_tbl = bookticker_table_for_gap_base(base_exchange)
    quote_tbl = bookticker_table(quote_exchange)
    base_alias = base_exchange.lower()
    quote_alias = quote_exchange.lower()
    sample = sample_interval_for_window(window_minutes)
    use_fill_prev = window_minutes <= 5
    if use_fill_prev:
        lookback = max(FILL_PREV_LOOKBACK_MINUTES, window_minutes)
        return f"""
WITH {base_alias}_full AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS base_mid,
           last((ask_price - bid_price) / ((ask_price + bid_price) / 2.0)) AS spread_1s,
           timestamp
    FROM {base_tbl}
    WHERE timestamp > dateadd('m', -{lookback}, now())
    SAMPLE BY {sample} FILL(PREV)
),
{quote_alias}_full AS (
    SELECT symbol, last((ask_price + bid_price) / 2.0) AS quote_mid, timestamp
    FROM {quote_tbl}
    WHERE timestamp > dateadd('m', -{lookback}, now())
    SAMPLE BY {sample} FILL(PREV)
),
{base_alias} AS (
    SELECT * FROM {base_alias}_full
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
),
{quote_alias} AS (
    SELECT * FROM {quote_alias}_full
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
),
gap AS (
    SELECT b.symbol,
           avg((b.base_mid - q.quote_mid) / b.base_mid) AS avg_mid_gap,
           avg(b.spread_1s) AS avg_spread
    FROM {base_alias} b
    JOIN {quote_alias} q ON b.symbol = q.symbol AND b.timestamp = q.timestamp
    GROUP BY b.symbol
)
SELECT * FROM gap ORDER BY abs(avg_mid_gap) DESC;
""".strip()
    return f"""
WITH {base_alias} AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS base_mid,
           last((ask_price - bid_price) / ((ask_price + bid_price) / 2.0)) AS spread_1s,
           timestamp
    FROM {base_tbl}
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {sample}
),
{quote_alias} AS (
    SELECT symbol, last((ask_price + bid_price) / 2.0) AS quote_mid, timestamp
    FROM {quote_tbl}
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {sample}
),
gap AS (
    SELECT b.symbol,
           avg((b.base_mid - q.quote_mid) / b.base_mid) AS avg_mid_gap,
           avg(b.spread_1s) AS avg_spread
    FROM {base_alias} b
    JOIN {quote_alias} q ON b.symbol = q.symbol AND b.timestamp = q.timestamp
    GROUP BY b.symbol
)
SELECT * FROM gap ORDER BY abs(avg_mid_gap) DESC;
""".strip()


def parse_dataset(
    payload: dict[str, Any],
) -> tuple[dict[str, float], dict[str, float]]:
    """QuestDB 응답 → (symbol → avg_mid_gap, symbol → avg_spread)."""
    columns = payload.get("columns") or []
    dataset = payload.get("dataset") or []
    symbol_idx: int | None = None
    gap_idx: int | None = None
    spread_idx: int | None = None
    for idx, col in enumerate(columns):
        name = (col or {}).get("name") if isinstance(col, dict) else None
        if name == "symbol":
            symbol_idx = idx
        elif name == "avg_mid_gap":
            gap_idx = idx
        elif name == "avg_spread":
            spread_idx = idx
    if symbol_idx is None or gap_idx is None:
        raise ValueError("QuestDB response missing symbol or avg_mid_gap column")
    max_idx = max(symbol_idx, gap_idx, spread_idx if spread_idx is not None else 0)
    gaps: dict[str, float] = {}
    spreads: dict[str, float] = {}
    for row in dataset:
        if not isinstance(row, list) or len(row) <= max_idx:
            continue
        symbol = str(row[symbol_idx] or "").strip()
        if not symbol:
            continue
        try:
            gap = float(row[gap_idx]) if row[gap_idx] is not None else 0.0
        except (TypeError, ValueError):
            gap = 0.0
        gaps[symbol] = gap
        if spread_idx is not None:
            try:
                spread = float(row[spread_idx]) if row[spread_idx] is not None else 0.0
            except (TypeError, ValueError):
                spread = 0.0
            spreads[symbol] = spread
    return gaps, spreads


def fetch(
    questdb_url: str,
    base_exchange: str,
    quote_exchange: str,
    window_minutes: int,
) -> tuple[dict[str, float], dict[str, float]]:
    query = build_query(base_exchange, quote_exchange, window_minutes)
    payload = questdb_exec(questdb_url, query)
    raw_gaps, raw_spreads = parse_dataset(payload)
    valid_gaps = filter_valid_gaps(raw_gaps)
    valid_spreads = {s: raw_spreads[s] for s in valid_gaps if s in raw_spreads}
    return valid_gaps, valid_spreads


def run(
    questdb_url: str,
    redis_client: redis.Redis,
    key_prefix: str,
    base_exchange: str,
    quote_exchange: str,
    window_minutes: int,
) -> bool:
    log = logger.bind(
        job="gap", base=base_exchange, quote=quote_exchange, window=window_minutes
    )
    try:
        avg_mid_gap_by_symbol, avg_spread_by_symbol = fetch(
            questdb_url, base_exchange, quote_exchange, window_minutes
        )
    except Exception as e:  # noqa: BLE001
        log.error("questdb_failed", error=str(e))
        return False
    blob = {
        "base_exchange": base_exchange,
        "quote_exchange": quote_exchange,
        "window_minutes": window_minutes,
        "updated_at_ms": now_ms(),
        "avg_mid_gap_by_symbol": avg_mid_gap_by_symbol,
        "avg_spread_by_symbol": avg_spread_by_symbol,
    }
    key = f"{key_prefix}:{base_exchange}:{quote_exchange}:{window_minutes}"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        log.error("redis_set_failed", error=str(e), key=key)
        return False

    if not avg_mid_gap_by_symbol:
        log.info("updated_empty", key=key)
        return True

    gaps = avg_mid_gap_by_symbol
    max_sym, max_gap = max(gaps.items(), key=lambda x: x[1])
    min_sym, min_gap = min(gaps.items(), key=lambda x: x[1])
    avg_gap = sum(gaps.values()) / len(gaps)
    avg_spread = (
        sum(avg_spread_by_symbol.values()) / len(avg_spread_by_symbol)
        if avg_spread_by_symbol
        else 0.0
    )
    log.info(
        "updated",
        symbols=len(gaps),
        avg_gap_bp=round(avg_gap * 10000, 2),
        max_gap_bp=round(max_gap * 10000, 2),
        max_gap_symbol=max_sym,
        min_gap_bp=round(min_gap * 10000, 2),
        min_gap_symbol=min_sym,
        avg_spread_bp=round(avg_spread * 10000, 2),
    )
    return True

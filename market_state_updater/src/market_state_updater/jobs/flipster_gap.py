"""flipster vs binance mid-price gap → Redis.

Redis key: {prefix}:flipster:binance:{window_minutes}

Blob shape (gap.py 와 동일 + binance spread 추가):
  {
    "base_exchange": "flipster",
    "quote_exchange": "binance",
    "window_minutes": 10,
    "updated_at_ms": ...,
    "avg_mid_gap_by_symbol":          {"BTC_USDT": 0.001, ...},  # (flipster_mid - binance_mid) / flipster_mid
    "avg_spread_by_symbol":           {"BTC_USDT": 0.0001, ...}, # flipster (base) side
    "avg_spread_binance_by_symbol":   {"BTC_USDT": 0.0002, ...}
  }

flipster 심볼은 `BTCUSDT.PERP` 형태라 binance `BTC_USDT` 에 맞춰
  replace(replace(symbol, '.PERP', ''), 'USDT', '_USDT')
로 SQL 안에서 정규화 후 JOIN. blob 의 symbol key 는 정규화된 형태 (BTC_USDT).
"""

from __future__ import annotations

import json
import time
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

# flipster 는 tick density 가 낮아 raw 100T/200T/1s sample 은 stale bucket 만 양산.
# 최소 5s 로 clamp — JOIN 가 binance 와 같은 bucket boundary 에 정렬돼야 하니
# 양쪽 다 동일 sample 사용.
_MIN_SAMPLE_SECS = 5.0


def _clamp_min_5s(interval: str) -> str:
    """sample interval string ('100T', '500T', '1s', '5s', ...) 을 최소 5s 로 clamp."""
    s = interval.strip()
    if s.endswith("T"):  # milliseconds — always < 5s
        return "5s"
    if s.endswith("s"):
        try:
            secs = float(s[:-1])
        except ValueError:
            return "5s"
        return s if secs >= _MIN_SAMPLE_SECS else "5s"
    return s


def build_query(window_minutes: int) -> str:
    """flipster(=base) vs binance(=quote) gap. flipster 심볼 정규화는 별도 CTE 에서.

    1m/5m 는 FILL(PREV) + lookback 으로 sparse 심볼 forward-fill.
    sample interval 은 최소 5s — flipster tick rate 낮아 sub-5s 는 빈 bucket 만 양산.
    """
    window_minutes = max(int(window_minutes), 1)
    sample = _clamp_min_5s(sample_interval_for_window(window_minutes))
    use_fill_prev = window_minutes <= 5
    if use_fill_prev:
        lookback = max(FILL_PREV_LOOKBACK_MINUTES, window_minutes)
        return f"""
WITH flipster_raw AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS base_mid,
           last((ask_price - bid_price) / ((ask_price + bid_price) / 2.0)) AS spread_1s,
           timestamp
    FROM flipster_bookticker
    WHERE timestamp > dateadd('m', -{lookback}, now())
    SAMPLE BY {sample} FILL(PREV)
),
flipster_full AS (
    SELECT replace(replace(symbol, '.PERP', ''), 'USDT', '_USDT') AS symbol,
           base_mid, spread_1s, timestamp
    FROM flipster_raw
),
binance_full AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS quote_mid,
           last((ask_price - bid_price) / ((ask_price + bid_price) / 2.0)) AS spread_1s_binance,
           timestamp
    FROM binance_bookticker
    WHERE timestamp > dateadd('m', -{lookback}, now())
    SAMPLE BY {sample} FILL(PREV)
),
flipster AS (
    SELECT * FROM flipster_full
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
),
binance AS (
    SELECT * FROM binance_full
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
),
gap AS (
    SELECT b.symbol,
           avg((b.base_mid - q.quote_mid) / b.base_mid) AS avg_mid_gap,
           avg(b.spread_1s) AS avg_spread,
           avg(q.spread_1s_binance) AS avg_spread_binance
    FROM flipster b
    JOIN binance q ON b.symbol = q.symbol AND b.timestamp = q.timestamp
    GROUP BY b.symbol
)
SELECT * FROM gap ORDER BY abs(avg_mid_gap) DESC;
""".strip()
    return f"""
WITH flipster_raw AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS base_mid,
           last((ask_price - bid_price) / ((ask_price + bid_price) / 2.0)) AS spread_1s,
           timestamp
    FROM flipster_bookticker
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {sample}
),
flipster AS (
    SELECT replace(replace(symbol, '.PERP', ''), 'USDT', '_USDT') AS symbol,
           base_mid, spread_1s, timestamp
    FROM flipster_raw
),
binance AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS quote_mid,
           last((ask_price - bid_price) / ((ask_price + bid_price) / 2.0)) AS spread_1s_binance,
           timestamp
    FROM binance_bookticker
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {sample}
),
gap AS (
    SELECT b.symbol,
           avg((b.base_mid - q.quote_mid) / b.base_mid) AS avg_mid_gap,
           avg(b.spread_1s) AS avg_spread,
           avg(q.spread_1s_binance) AS avg_spread_binance
    FROM flipster b
    JOIN binance q ON b.symbol = q.symbol AND b.timestamp = q.timestamp
    GROUP BY b.symbol
)
SELECT * FROM gap ORDER BY abs(avg_mid_gap) DESC;
""".strip()


def parse_dataset(
    payload: dict[str, Any],
) -> tuple[dict[str, float], dict[str, float], dict[str, float]]:
    """QuestDB 응답 → (avg_mid_gap, avg_spread_flipster, avg_spread_binance) per symbol."""
    columns = payload.get("columns") or []
    dataset = payload.get("dataset") or []
    symbol_idx: int | None = None
    gap_idx: int | None = None
    spread_idx: int | None = None
    bn_spread_idx: int | None = None
    for idx, col in enumerate(columns):
        name = (col or {}).get("name") if isinstance(col, dict) else None
        if name == "symbol":
            symbol_idx = idx
        elif name == "avg_mid_gap":
            gap_idx = idx
        elif name == "avg_spread":
            spread_idx = idx
        elif name == "avg_spread_binance":
            bn_spread_idx = idx
    if symbol_idx is None or gap_idx is None:
        raise ValueError("QuestDB response missing symbol or avg_mid_gap column")
    indices = [i for i in (symbol_idx, gap_idx, spread_idx, bn_spread_idx) if i is not None]
    max_idx = max(indices)
    gaps: dict[str, float] = {}
    flipster_spreads: dict[str, float] = {}
    binance_spreads: dict[str, float] = {}
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
                fsp = float(row[spread_idx]) if row[spread_idx] is not None else 0.0
            except (TypeError, ValueError):
                fsp = 0.0
            flipster_spreads[symbol] = fsp
        if bn_spread_idx is not None:
            try:
                bsp = float(row[bn_spread_idx]) if row[bn_spread_idx] is not None else 0.0
            except (TypeError, ValueError):
                bsp = 0.0
            binance_spreads[symbol] = bsp
    return gaps, flipster_spreads, binance_spreads


def fetch(
    questdb_url: str,
    window_minutes: int,
) -> tuple[dict[str, float], dict[str, float], dict[str, float]]:
    payload = questdb_exec(questdb_url, build_query(window_minutes))
    raw_gaps, raw_flipster, raw_binance = parse_dataset(payload)
    valid_gaps = filter_valid_gaps(raw_gaps)
    valid_flipster = {s: raw_flipster[s] for s in valid_gaps if s in raw_flipster}
    valid_binance = {s: raw_binance[s] for s in valid_gaps if s in raw_binance}
    return valid_gaps, valid_flipster, valid_binance


def run(
    questdb_url: str,
    redis_client: redis.Redis,
    key_prefix: str,
    window_minutes: int,
) -> bool:
    log = logger.bind(job="flipster_gap", window=window_minutes)
    t0 = time.monotonic()
    try:
        gaps_by_sym, flipster_spreads, binance_spreads = fetch(
            questdb_url, window_minutes
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
        "base_exchange": "flipster",
        "quote_exchange": "binance",
        "window_minutes": window_minutes,
        "updated_at_ms": now_ms(),
        "avg_mid_gap_by_symbol": gaps_by_sym,
        "avg_spread_by_symbol": flipster_spreads,
        "avg_spread_binance_by_symbol": binance_spreads,
    }
    key = f"{key_prefix}:flipster:binance:{window_minutes}"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        log.error("redis_set_failed", error=str(e), key=key)
        return False

    if not gaps_by_sym:
        log.info("updated_empty", key=key, query_ms=query_ms)
        return True

    max_sym, max_gap = max(gaps_by_sym.items(), key=lambda x: x[1])
    min_sym, min_gap = min(gaps_by_sym.items(), key=lambda x: x[1])
    avg_gap = sum(gaps_by_sym.values()) / len(gaps_by_sym)
    avg_fsp = (
        sum(flipster_spreads.values()) / len(flipster_spreads)
        if flipster_spreads else 0.0
    )
    avg_bsp = (
        sum(binance_spreads.values()) / len(binance_spreads)
        if binance_spreads else 0.0
    )
    log.info(
        "updated",
        symbols=len(gaps_by_sym),
        avg_gap_bp=round(avg_gap * 10000, 2),
        max_gap_bp=round(max_gap * 10000, 2),
        max_gap_symbol=max_sym,
        min_gap_bp=round(min_gap * 10000, 2),
        min_gap_symbol=min_sym,
        avg_spread_flipster_bp=round(avg_fsp * 10000, 2),
        avg_spread_binance_bp=round(avg_bsp * 10000, 2),
        query_ms=query_ms,
    )
    return True

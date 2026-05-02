"""gate_sub_account_trades 의 trade outcome 집계 → Redis.

각 trade 기준으로 lookahead_seconds 뒤 gate_webbookticker mid 를 확인해서
profitable 여부 판정. 윈도우 (window_minutes) 안에 들어온 trade 들만 집계.

Win/loss 정의 (tie = loss):
  - long  (size > 0): future_mid >  trade_price → win, else loss
  - short (size < 0): future_mid <  trade_price → win, else loss

Volume 단위: |usdt_size| (테이블에 이미 signed USDT notional 로 저장돼 있음).
|usdt_size| <= MIN_NOTIONAL_USDT 인 dust trade 는 SQL 단계에서 제외 (default 10).
fee < 0 (rebate / 환불 등 비정상) trade 도 SQL 단계에서 제외.

Redis key: {prefix}:trade_outcomes:gate:{window_minutes}m:{lookahead_seconds}s

Blob:
  {
    "window_minutes": 5,
    "lookahead_seconds": 30,
    "updated_at_ms": ...,
    "by_symbol": {
      "BTC_USDT": {
        "long_win_count":   3, "long_loss_count":   2,
        "long_win_volume":  412.3, "long_loss_volume":  280.1,
        "short_win_count":  1, "short_loss_count":  4,
        "short_win_volume": 80.0,  "short_loss_volume": 320.5
      },
      ...
    }
  }

Degenerate combo (window_minutes*60 <= lookahead_seconds) 는 by_symbol={} 로 떨어짐
— SQL filter 에서 자연스럽게 비게 됨.
"""

from __future__ import annotations

import json
import time
from typing import Any

import redis
import structlog

from market_state_updater.jobs.common import now_ms
from market_state_updater.questdb import questdb_exec

logger = structlog.get_logger(__name__)

# |usdt_size| <= 이 값 이하 trade 는 dust 로 보고 집계에서 제외.
MIN_NOTIONAL_USDT: float = 10.0

_NUMERIC_FIELDS: tuple[str, ...] = (
    "long_win_count",
    "long_loss_count",
    "long_win_volume",
    "long_loss_volume",
    "short_win_count",
    "short_loss_count",
    "short_win_volume",
    "short_loss_volume",
)


def build_query(window_minutes: int, lookahead_seconds: int) -> str:
    """trade_outcomes 집계 SQL.

    구조:
      1. webmids: gate_webbookticker SAMPLE BY 1s FILL(PREV) — 모든 sec 버킷 채움.
      2. JOIN: trade.contract = mid.symbol AND date_trunc('second', trade_ts + lookahead) = mid.timestamp
      3. CASE WHEN 으로 long/short × win/loss 4 buckets 집계 (count + abs(usdt_size) sum).
    """
    window_minutes = max(int(window_minutes), 1)
    lookahead_seconds = max(int(lookahead_seconds), 1)
    # mid 는 trade ts 보다 lookahead 만큼 미래까지 필요 — 하지만 trade ts 자체가
    # `<= now() - lookahead` 로 잘려 있어서 lookup_ts 최대값은 ~now(). 따라서 window 만 커버하면 됨.
    # FILL(PREV) 가 prior 를 forward-fill 하도록 +1m buffer.
    mid_lookback_min = window_minutes + 1
    return f"""
WITH webmids AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS mid,
           timestamp
    FROM gate_webbookticker
    WHERE timestamp > dateadd('m', -{mid_lookback_min}, now())
    SAMPLE BY 1s FILL(PREV)
),
agg AS (
    SELECT t.contract AS symbol,
           sum(case when t.size > 0 and m.mid >  t.price then 1 else 0 end) AS long_win_count,
           sum(case when t.size > 0 and m.mid <= t.price then 1 else 0 end) AS long_loss_count,
           sum(case when t.size > 0 and m.mid >  t.price then abs(t.usdt_size) else 0 end) AS long_win_volume,
           sum(case when t.size > 0 and m.mid <= t.price then abs(t.usdt_size) else 0 end) AS long_loss_volume,
           sum(case when t.size < 0 and m.mid <  t.price then 1 else 0 end) AS short_win_count,
           sum(case when t.size < 0 and m.mid >= t.price then 1 else 0 end) AS short_loss_count,
           sum(case when t.size < 0 and m.mid <  t.price then abs(t.usdt_size) else 0 end) AS short_win_volume,
           sum(case when t.size < 0 and m.mid >= t.price then abs(t.usdt_size) else 0 end) AS short_loss_volume
    FROM gate_sub_account_trades t
    JOIN webmids m ON t.contract = m.symbol
                  AND date_trunc('second', dateadd('s', {lookahead_seconds}, t.timestamp)) = m.timestamp
    WHERE t.timestamp > dateadd('m', -{window_minutes}, now())
      AND t.timestamp <= dateadd('s', -{lookahead_seconds}, now())
      AND abs(t.usdt_size) > {MIN_NOTIONAL_USDT}
      AND t.fee >= 0
    GROUP BY symbol
)
SELECT * FROM agg ORDER BY symbol;
""".strip()


def parse_dataset(payload: dict[str, Any]) -> dict[str, dict[str, float]]:
    """QuestDB 응답 → {symbol: {long_win_count: ..., ...}}."""
    columns = payload.get("columns") or []
    dataset = payload.get("dataset") or []
    name_to_idx: dict[str, int] = {}
    for idx, col in enumerate(columns):
        name = (col or {}).get("name") if isinstance(col, dict) else None
        if name:
            name_to_idx[name] = idx
    if "symbol" not in name_to_idx:
        raise ValueError("QuestDB response missing symbol column")
    missing = [f for f in _NUMERIC_FIELDS if f not in name_to_idx]
    if missing:
        raise ValueError(f"QuestDB response missing columns: {missing}")
    sym_idx = name_to_idx["symbol"]
    field_indices = [(f, name_to_idx[f]) for f in _NUMERIC_FIELDS]
    out: dict[str, dict[str, float]] = {}
    for row in dataset:
        if not isinstance(row, list):
            continue
        sym = str(row[sym_idx] or "").strip()
        if not sym:
            continue
        rec: dict[str, float] = {}
        for field, idx in field_indices:
            v = row[idx] if idx < len(row) else None
            try:
                rec[field] = float(v) if v is not None else 0.0
            except (TypeError, ValueError):
                rec[field] = 0.0
        # count 필드는 정수로 노출
        for cf in (
            "long_win_count",
            "long_loss_count",
            "short_win_count",
            "short_loss_count",
        ):
            rec[cf] = int(rec[cf])
        out[sym] = rec
    return out


def fetch(
    questdb_url: str, window_minutes: int, lookahead_seconds: int
) -> dict[str, dict[str, float]]:
    payload = questdb_exec(
        questdb_url, build_query(window_minutes, lookahead_seconds)
    )
    return parse_dataset(payload)


def run(
    questdb_url: str,
    redis_client: redis.Redis,
    key_prefix: str,
    window_minutes: int,
    lookahead_seconds: int,
) -> bool:
    log = logger.bind(
        job="trade_outcomes", window=window_minutes, lookahead_s=lookahead_seconds
    )
    t0 = time.monotonic()
    try:
        by_symbol = fetch(questdb_url, window_minutes, lookahead_seconds)
    except Exception as e:  # noqa: BLE001
        log.error(
            "questdb_failed",
            error=str(e),
            query_ms=int((time.monotonic() - t0) * 1000),
        )
        return False
    query_ms = int((time.monotonic() - t0) * 1000)
    blob = {
        "window_minutes": window_minutes,
        "lookahead_seconds": lookahead_seconds,
        "updated_at_ms": now_ms(),
        "by_symbol": by_symbol,
    }
    key = f"{key_prefix}:trade_outcomes:gate:{window_minutes}m:{lookahead_seconds}s"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        log.error("redis_set_failed", error=str(e), key=key)
        return False

    if not by_symbol:
        log.info("updated_empty", key=key, query_ms=query_ms)
        return True

    total_long_win = sum(r["long_win_count"] for r in by_symbol.values())
    total_long_loss = sum(r["long_loss_count"] for r in by_symbol.values())
    total_short_win = sum(r["short_win_count"] for r in by_symbol.values())
    total_short_loss = sum(r["short_loss_count"] for r in by_symbol.values())
    total_volume = sum(
        r["long_win_volume"]
        + r["long_loss_volume"]
        + r["short_win_volume"]
        + r["short_loss_volume"]
        for r in by_symbol.values()
    )
    n_total = total_long_win + total_long_loss + total_short_win + total_short_loss
    win_rate = (
        (total_long_win + total_short_win) / n_total if n_total > 0 else 0.0
    )
    log.info(
        "updated",
        symbols=len(by_symbol),
        trades=n_total,
        long_win=total_long_win,
        long_loss=total_long_loss,
        short_win=total_short_win,
        short_loss=total_short_loss,
        win_rate=round(win_rate, 4),
        volume_usdt=round(total_volume, 2),
        query_ms=query_ms,
    )
    return True

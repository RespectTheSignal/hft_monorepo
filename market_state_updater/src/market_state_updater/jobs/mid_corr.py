"""심볼별 corr(base_mid, quote_mid) — 두 거래소 mid level 의 Pearson corr → Redis.

원본: gate_hft/update_market_mid_correlation.py 패턴.

한 cycle 호출이 두 base (gate, gate_web) 모두 처리해서 blob 1개에 묶어 Redis SET
(원본과 호환되는 shape).

Metric 의미:
  - mid level 의 corr 은 두 거래소가 같은 underlying 을 트래킹하니 보통 +0.99 가까움
  - 어긋나는 심볼 / lag 큰 심볼만 corr 가 0.9 이하로 떨어짐
  - level corr 은 highly persistent 한 변수의 corr 라 lead-lag 분석엔 부적합 (그건
    price_change_gap_corr 의 두 거래소 step return corr 이 더 적절)
  - 이 job 의 가치: "두 feed 가 정렬돼 있는가" 의 health-check 같은 baseline

QuestDB corr(x, y) 직접 사용 (두 다른 컬럼이면 지원됨).

Redis key: {prefix}:{quote}:{window}

Blob:
  {
    "quote_exchange": "binance",
    "window_minutes": 10,
    "updated_at_ms": ...,
    "min_samples": 30,
    "corr_gate_bookticker_vs_quote_by_symbol":    {"BTC_USDT": 0.995, ...},
    "corr_gate_webbookticker_vs_quote_by_symbol": {"BTC_USDT": 0.991, ...},
    "n_samples_by_symbol": {"BTC_USDT": 580, ...}     # min(gate, gate_web)
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
    sample_interval_for_window,
)
from market_state_updater.questdb import questdb_exec

logger = structlog.get_logger(__name__)

DEFAULT_MIN_SAMPLES = 30


def build_query(
    base_exchange: str,
    quote_exchange: str,
    window_minutes: int,
    min_samples: int = DEFAULT_MIN_SAMPLES,
) -> str:
    """SAMPLE BY 단위는 jobs.common.sample_interval_for_window 따라감 (윈도우별).

    base_exchange: 'gate' → gate_bookticker, 'gate_web' → gate_webbookticker.
    """
    window_minutes = max(int(window_minutes), 1)
    min_samples = max(int(min_samples), 2)
    base_tbl = bookticker_table_for_gap_base(base_exchange)
    quote_tbl = bookticker_table(quote_exchange)
    sample = sample_interval_for_window(window_minutes)
    quote_alias = quote_exchange.lower()
    return f"""
WITH g AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS base_mid,
           timestamp
    FROM {base_tbl}
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {sample} FILL(PREV)
),
{quote_alias} AS (
    SELECT symbol,
           last((ask_price + bid_price) / 2.0) AS quote_mid,
           timestamp
    FROM {quote_tbl}
    WHERE timestamp > dateadd('m', -{window_minutes}, now())
    SAMPLE BY {sample} FILL(PREV)
),
al AS (
    SELECT g.symbol, g.base_mid, q.quote_mid
    FROM g
    INNER JOIN {quote_alias} q ON g.symbol = q.symbol AND g.timestamp = q.timestamp
),
agg AS (
    SELECT symbol,
           corr(base_mid, quote_mid) AS corr_mid,
           count() AS n_samples
    FROM al
    GROUP BY symbol
)
SELECT * FROM agg
WHERE n_samples >= {min_samples}
ORDER BY symbol;
""".strip()


def parse_dataset(
    payload: dict[str, Any],
) -> tuple[dict[str, float], dict[str, int]]:
    """QuestDB 응답 → (symbol → corr_mid, symbol → n_samples)."""
    columns = payload.get("columns") or []
    dataset = payload.get("dataset") or []
    sym_idx: int | None = None
    corr_idx: int | None = None
    n_idx: int | None = None
    for i, col in enumerate(columns):
        name = (col or {}).get("name") if isinstance(col, dict) else None
        if name == "symbol":
            sym_idx = i
        elif name == "corr_mid":
            corr_idx = i
        elif name == "n_samples":
            n_idx = i
    if sym_idx is None or corr_idx is None or n_idx is None:
        raise ValueError("QuestDB response missing symbol/corr_mid/n_samples column")
    corrs: dict[str, float] = {}
    counts: dict[str, int] = {}
    for row in dataset:
        if not isinstance(row, list):
            continue
        sym = str(row[sym_idx] or "").strip()
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
        if cv != cv:  # NaN
            continue
        try:
            corrs[sym] = cv
            counts[sym] = int(n)
        except (TypeError, ValueError):
            continue
    return corrs, counts


def fetch(
    questdb_url: str,
    base_exchange: str,
    quote_exchange: str,
    window_minutes: int,
    min_samples: int = DEFAULT_MIN_SAMPLES,
) -> tuple[dict[str, float], dict[str, int]]:
    sql = build_query(base_exchange, quote_exchange, window_minutes, min_samples)
    payload = questdb_exec(questdb_url, sql)
    return parse_dataset(payload)


def run(
    questdb_url: str,
    redis_client: redis.Redis,
    key_prefix: str,
    quote_exchange: str,
    window_minutes: int,
    min_samples: int = DEFAULT_MIN_SAMPLES,
) -> bool:
    """gate, gate_web 두 base 모두 fetch → 한 blob 으로 묶어 Redis SET."""
    log = logger.bind(job="mid_corr", quote=quote_exchange, window=window_minutes)
    t0 = time.monotonic()
    try:
        cg, ng = fetch(questdb_url, "gate", quote_exchange, window_minutes, min_samples)
        cw, nw = fetch(
            questdb_url, "gate_web", quote_exchange, window_minutes, min_samples
        )
    except Exception as e:  # noqa: BLE001
        log.error(
            "questdb_failed",
            error=str(e),
            query_ms=int((time.monotonic() - t0) * 1000),
        )
        return False
    query_ms = int((time.monotonic() - t0) * 1000)

    all_syms = set(cg) | set(cw)
    n_samples_by_symbol: dict[str, int] = {}
    for s in all_syms:
        a = ng.get(s)
        b = nw.get(s)
        if a is not None and b is not None:
            n_samples_by_symbol[s] = min(a, b)
        elif a is not None:
            n_samples_by_symbol[s] = a
        elif b is not None:
            n_samples_by_symbol[s] = b

    blob = {
        "quote_exchange": quote_exchange,
        "window_minutes": window_minutes,
        "updated_at_ms": now_ms(),
        "min_samples": min_samples,
        "corr_gate_bookticker_vs_quote_by_symbol": cg,
        "corr_gate_webbookticker_vs_quote_by_symbol": cw,
        "n_samples_by_symbol": n_samples_by_symbol,
    }
    key = f"{key_prefix}:{quote_exchange}:{window_minutes}"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        log.error("redis_set_failed", error=str(e), key=key, query_ms=query_ms)
        return False

    if not cg and not cw:
        log.info("updated_empty", key=key, query_ms=query_ms)
        return True

    mean_g = sum(cg.values()) / len(cg) if cg else 0.0
    mean_w = sum(cw.values()) / len(cw) if cw else 0.0
    log.info(
        "updated",
        symbols_gate=len(cg),
        symbols_gate_web=len(cw),
        mean_corr_gate=round(mean_g, 4),
        mean_corr_gate_web=round(mean_w, 4),
        query_ms=query_ms,
    )
    return True

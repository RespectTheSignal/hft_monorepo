"""CLI + env 를 합쳐 AppConfig 를 만든다."""

from __future__ import annotations

import argparse
import os
from dataclasses import dataclass
from typing import Literal

from market_state_updater.jobs.common import parse_corr_return_seconds_overrides

DEFAULT_QUESTDB_URL = "http://localhost:9000"
DEFAULT_REDIS_URL = "redis://localhost:6379"
DEFAULT_MARKET_GAP_PREFIX = "gate_hft:market_gap"
DEFAULT_MARKET_GAP_BASE = "gate"
DEFAULT_MARKET_GAP_QUOTE_EXCHANGES = "binance"
DEFAULT_PRICE_CHANGE_PREFIX = "gate_hft:price_change"
DEFAULT_PRICE_CHANGE_SOURCES = "gate,binance"
DEFAULT_CORR_QUOTE_EXCHANGES = "binance"
DEFAULT_UPDATE_INTERVAL_SECS = 10
DEFAULT_HEARTBEAT_PREFIX = "gate_hft:_meta:market_state_updater"
DEFAULT_ALERT_AFTER_CONSECUTIVE_FAILURES = 5

WindowMode = Literal["fast", "slow", "all"]


def _truthy(v: str | None) -> bool:
    return (v or "").strip().lower() not in ("0", "false", "no", "")


def _split_csv(s: str) -> tuple[str, ...]:
    return tuple(x.strip().lower() for x in s.split(",") if x.strip())


@dataclass(frozen=True, slots=True)
class AppConfig:
    questdb_url: str
    redis_url: str
    interval_secs: int  # 0 = once
    once: bool

    window_mode: WindowMode  # fast | slow | all

    market_gap_prefix: str
    base_exchange: str
    quote_exchanges: tuple[str, ...]

    include_gate_web: bool
    include_spread_pair: bool
    include_gate_gate_web_gap: bool

    include_price_change: bool
    price_change_prefix: str
    price_change_sources: tuple[str, ...]

    # gate_web price-change ↔ gap correlation
    include_price_change_gap_corr: bool
    corr_quote_exchanges: tuple[str, ...]
    corr_return_seconds_overrides: dict[int, int]

    # heartbeat: {heartbeat_prefix}:{mode} 키에 cycle 결과 JSON SET
    heartbeat_prefix: str

    # 텔레그램 알림 (token+chat_id 둘 다 있을 때만 활성)
    telegram_bot_token: str | None
    telegram_chat_id: str | None
    alert_after_consecutive_failures: int

    @property
    def gap_bases(self) -> tuple[str, ...]:
        """gap job 의 base 거래소 목록. include_gate_web 이면 gate_web 추가."""
        if self.include_gate_web and self.base_exchange != "gate_web":
            return (self.base_exchange, "gate_web")
        return (self.base_exchange,)


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="market-state-updater",
        description="QuestDB → Redis 시장 gap/spread/price-change 상태 업데이터.",
    )
    parser.add_argument(
        "--once", action="store_true", help="Run once and exit (no loop)."
    )
    parser.add_argument(
        "--interval",
        type=int,
        default=int(os.environ.get("UPDATE_INTERVAL_SECS", DEFAULT_UPDATE_INTERVAL_SECS)),
        help="Update interval in seconds (default: env UPDATE_INTERVAL_SECS or 10).",
    )
    parser.add_argument(
        "--windows",
        choices=("fast", "slow", "all"),
        default=os.environ.get("WINDOW_MODE", "all"),
        help="윈도우 모드. fast=≤5m, slow=>5m, all=전부 (default: env WINDOW_MODE or all). "
        "운영에서는 두 데몬으로 분리 권장 (fast 빠른 interval, slow 긴 interval).",
    )
    parser.add_argument(
        "--questdb-url",
        default=os.environ.get("QUESTDB_URL", DEFAULT_QUESTDB_URL),
        help="QuestDB HTTP URL. With auth: http://user:pass@host:9000",
    )
    parser.add_argument(
        "--redis-url",
        default=os.environ.get("REDIS_URL", DEFAULT_REDIS_URL),
        help="Redis URL.",
    )
    parser.add_argument(
        "--prefix",
        default=os.environ.get("MARKET_GAP_REDIS_PREFIX", DEFAULT_MARKET_GAP_PREFIX),
        help="Redis key prefix for gap-family jobs.",
    )
    parser.add_argument(
        "--base-exchange",
        dest="base_exchange",
        default=os.environ.get("MARKET_GAP_BASE", DEFAULT_MARKET_GAP_BASE),
    )
    parser.add_argument(
        "--exchanges",
        default=os.environ.get(
            "MARKET_GAP_QUOTE_EXCHANGES", DEFAULT_MARKET_GAP_QUOTE_EXCHANGES
        ),
        help="Quote exchange(s), comma-separated. e.g. binance,bybit,bitget",
    )
    return parser


def load_config(argv: list[str] | None = None) -> AppConfig:
    args = build_arg_parser().parse_args(argv)
    quote_exchanges = _split_csv(args.exchanges)
    if not quote_exchanges:
        raise SystemExit(
            "At least one quote exchange required (--exchanges or MARKET_GAP_QUOTE_EXCHANGES)"
        )
    window_mode: WindowMode = args.windows  # type: ignore[assignment]
    return AppConfig(
        questdb_url=args.questdb_url,
        redis_url=args.redis_url,
        interval_secs=0 if args.once else max(1, args.interval),
        once=args.once,
        window_mode=window_mode,
        market_gap_prefix=args.prefix,
        base_exchange=args.base_exchange.strip().lower(),
        quote_exchanges=quote_exchanges,
        include_gate_web=_truthy(os.getenv("MARKET_GAP_INCLUDE_GATE_WEB", "1")),
        include_spread_pair=_truthy(
            os.getenv("MARKET_GAP_INCLUDE_GATE_SPREAD_PAIR", "1")
        ),
        include_gate_gate_web_gap=_truthy(
            os.getenv("MARKET_GAP_INCLUDE_GATE_GATE_WEB_GAP", "1")
        ),
        include_price_change=_truthy(os.getenv("MARKET_GAP_INCLUDE_PRICE_CHANGE", "1")),
        price_change_prefix=os.getenv(
            "PRICE_CHANGE_REDIS_PREFIX", DEFAULT_PRICE_CHANGE_PREFIX
        ),
        price_change_sources=_split_csv(
            os.getenv("PRICE_CHANGE_SOURCES", DEFAULT_PRICE_CHANGE_SOURCES)
        ),
        include_price_change_gap_corr=_truthy(
            os.getenv("MARKET_GAP_INCLUDE_PRICE_CHANGE_GAP_CORR", "1")
        ),
        corr_quote_exchanges=_split_csv(
            os.getenv("CORR_QUOTE_EXCHANGES", DEFAULT_CORR_QUOTE_EXCHANGES)
        ),
        corr_return_seconds_overrides=parse_corr_return_seconds_overrides(
            os.getenv("CORR_RETURN_SECONDS_OVERRIDES", "")
        ),
        heartbeat_prefix=os.getenv("HEARTBEAT_REDIS_PREFIX", DEFAULT_HEARTBEAT_PREFIX),
        telegram_bot_token=os.getenv("TELEGRAM_BOT_TOKEN") or None,
        telegram_chat_id=os.getenv("TELEGRAM_CHAT_ID") or None,
        alert_after_consecutive_failures=int(
            os.getenv(
                "ALERT_AFTER_CONSECUTIVE_FAILURES",
                str(DEFAULT_ALERT_AFTER_CONSECUTIVE_FAILURES),
            )
        ),
    )

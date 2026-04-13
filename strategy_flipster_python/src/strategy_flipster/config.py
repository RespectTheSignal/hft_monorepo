"""TOML 설정 로딩 — 환경변수 우선, TOML fallback"""

from __future__ import annotations

import os
import sys
from dataclasses import dataclass, field
from pathlib import Path

if sys.version_info >= (3, 11):
    import tomllib
else:
    try:
        import tomllib  # type: ignore[import-not-found]
    except ModuleNotFoundError:
        import tomli as tomllib  # type: ignore[no-redef,import-untyped]

from strategy_flipster.error import AppError, AppException, ErrorKind


@dataclass(frozen=True, slots=True)
class FlipsterApiConfig:
    api_key: str
    api_secret: str
    base_url: str = "https://trading-api.flipster.io"
    ws_url: str = "wss://trading-api.flipster.io/api/v1/stream"


@dataclass(frozen=True, slots=True)
class FlipsterFeedConfig:
    """Flipster 시장 데이터 피드 설정.

    mode:
      - "zmq": ZMQ PUB에 직접 SUB (원격 가능, data_subscriber 불필요)
      - "ipc": data_subscriber IPC Unix socket (로컬 전용)
    """
    mode: str = "zmq"  # "zmq" 또는 "ipc"
    zmq_address: str = "tcp://211.181.122.104:7000"
    ipc_socket_path: str = "/tmp/flipster_data_subscriber.sock"
    symbols: list[str] = field(default_factory=list)
    process_id: str = "strategy_main"


@dataclass(frozen=True, slots=True)
class ExchangeFeedConfig:
    """범용 거래소 ZMQ 피드 설정 (binance, gate, bybit, bitget, okx)"""
    exchange: str
    zmq_address: str
    enabled: bool = True
    topics: list[str] = field(default_factory=list)


@dataclass(frozen=True, slots=True)
class StrategyConfig:
    tick_interval_ms: int = 10


@dataclass(frozen=True, slots=True)
class MarketStatsConfig:
    """전체 심볼 ticker 주기 풀링 설정"""
    enabled: bool = True
    interval_ms: int = 10_000


@dataclass(frozen=True, slots=True)
class AppConfig:
    flipster_api: FlipsterApiConfig
    flipster_feed: FlipsterFeedConfig
    exchange_feeds: list[ExchangeFeedConfig]
    strategy: StrategyConfig
    market_stats: MarketStatsConfig


# 거래소별 기본 포트
_DEFAULT_PORTS: dict[str, int] = {
    "binance": 6000,
    "gate": 5559,
    "bybit": 5558,
    "bitget": 6010,
    "okx": 6011,
}


def load_config(path: Path | str) -> AppConfig:
    """TOML 파일에서 설정 로딩. API 키는 환경변수 우선."""
    config_path = Path(path)
    if not config_path.exists():
        raise AppException(AppError(
            kind=ErrorKind.CONFIG,
            message=f"설정 파일 없음: {config_path}",
        ))

    with open(config_path, "rb") as f:
        raw = tomllib.load(f)

    # ── Flipster API ──
    flipster_api_raw = raw.get("flipster_api", {})
    api_key = os.environ.get("FLIPSTER_API_KEY", flipster_api_raw.get("api_key", ""))
    api_secret = os.environ.get("FLIPSTER_API_SECRET", flipster_api_raw.get("api_secret", ""))

    if not api_key or not api_secret:
        raise AppException(AppError(
            kind=ErrorKind.CONFIG,
            message="FLIPSTER_API_KEY/FLIPSTER_API_SECRET 미설정",
        ))

    flipster_api = FlipsterApiConfig(
        api_key=api_key,
        api_secret=api_secret,
        base_url=flipster_api_raw.get("base_url", "https://trading-api.flipster.io"),
        ws_url=flipster_api_raw.get("ws_url", "wss://trading-api.flipster.io/api/v1/stream"),
    )

    # ── Flipster Feed ──
    feed_raw = raw.get("flipster_feed", {})
    flipster_feed = FlipsterFeedConfig(
        mode=feed_raw.get("mode", "zmq"),
        zmq_address=feed_raw.get("zmq_address", "tcp://211.181.122.104:7000"),
        ipc_socket_path=feed_raw.get("ipc_socket_path", "/tmp/flipster_data_subscriber.sock"),
        symbols=feed_raw.get("symbols", []),
        process_id=feed_raw.get("process_id", "strategy_main"),
    )

    # ── Exchange Feeds (멀티 거래소) ──
    exchange_feeds: list[ExchangeFeedConfig] = []

    # [[exchange_feeds]] 배열 형태
    for entry in raw.get("exchange_feeds", []):
        exchange = entry.get("exchange", "")
        if not exchange:
            continue
        default_port = _DEFAULT_PORTS.get(exchange, 6000)
        zmq_addr = entry.get("zmq_address", f"tcp://127.0.0.1:{default_port}")
        exchange_feeds.append(ExchangeFeedConfig(
            exchange=exchange,
            zmq_address=zmq_addr,
            enabled=entry.get("enabled", True),
            topics=entry.get("topics", []),
        ))

    # 하위 호환: [binance_feed] 섹션이 있으면 exchange_feeds에 추가
    binance_raw = raw.get("binance_feed")
    if binance_raw is not None:
        # 이미 exchange_feeds에 binance가 없을 때만 추가
        has_binance = any(f.exchange == "binance" for f in exchange_feeds)
        if not has_binance:
            exchange_feeds.append(ExchangeFeedConfig(
                exchange="binance",
                zmq_address=binance_raw.get("zmq_address", "tcp://211.181.122.3:6000"),
                enabled=True,
                topics=binance_raw.get("topics", []),
            ))

    # ── Strategy ──
    strategy_raw = raw.get("strategy", {})
    strategy = StrategyConfig(
        tick_interval_ms=strategy_raw.get("tick_interval_ms", 10),
    )

    # ── Market Stats Poller ──
    stats_raw = raw.get("market_stats", {})
    market_stats = MarketStatsConfig(
        enabled=stats_raw.get("enabled", True),
        interval_ms=stats_raw.get("interval_ms", 10_000),
    )

    return AppConfig(
        flipster_api=flipster_api,
        flipster_feed=flipster_feed,
        exchange_feeds=exchange_feeds,
        strategy=strategy,
        market_stats=market_stats,
    )

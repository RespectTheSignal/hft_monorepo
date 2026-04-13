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
    ipc_socket_path: str = "/tmp/flipster_data_subscriber.sock"
    symbols: list[str] = field(default_factory=list)
    process_id: str = "strategy_main"


@dataclass(frozen=True, slots=True)
class BinanceFeedConfig:
    zmq_address: str = "tcp://211.181.122.3:6000"
    topics: list[str] = field(default_factory=list)


@dataclass(frozen=True, slots=True)
class StrategyConfig:
    tick_interval_ms: int = 10


@dataclass(frozen=True, slots=True)
class AppConfig:
    flipster_api: FlipsterApiConfig
    flipster_feed: FlipsterFeedConfig
    binance_feed: BinanceFeedConfig
    strategy: StrategyConfig


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

    # Flipster API — 환경변수 우선
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

    # Flipster Feed
    feed_raw = raw.get("flipster_feed", {})
    flipster_feed = FlipsterFeedConfig(
        ipc_socket_path=feed_raw.get("ipc_socket_path", "/tmp/flipster_data_subscriber.sock"),
        symbols=feed_raw.get("symbols", []),
        process_id=feed_raw.get("process_id", "strategy_main"),
    )

    # Binance Feed
    binance_raw = raw.get("binance_feed", {})
    binance_feed = BinanceFeedConfig(
        zmq_address=binance_raw.get("zmq_address", "tcp://211.181.122.3:6000"),
        topics=binance_raw.get("topics", []),
    )

    # Strategy
    strategy_raw = raw.get("strategy", {})
    strategy = StrategyConfig(
        tick_interval_ms=strategy_raw.get("tick_interval_ms", 10),
    )

    return AppConfig(
        flipster_api=flipster_api,
        flipster_feed=flipster_feed,
        binance_feed=binance_feed,
        strategy=strategy,
    )

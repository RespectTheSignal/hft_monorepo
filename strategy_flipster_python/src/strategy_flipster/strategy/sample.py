"""샘플 전략 — 구조 데모용.

Binance-Flipster 스프레드 모니터링 전략.
실제 매매 로직은 비어 있음 — 인터페이스 사용 예시.
"""

from __future__ import annotations

import time

import structlog

from strategy_flipster.market_data.stats_cache import MarketStatsCache
from strategy_flipster.types import BookTicker, OrderRequest
from strategy_flipster.user_data.state import UserState

logger = structlog.get_logger(__name__)


class SampleStrategy:
    """Binance/Flipster 스프레드 모니터링 — 샘플 구현"""

    def __init__(self) -> None:
        # 심볼별 최신 호가 캐시
        self._binance_tickers: dict[str, BookTicker] = {}
        self._flipster_tickers: dict[str, BookTicker] = {}
        self._last_log_ns: int = 0

    async def on_book_ticker(
        self,
        ticker: BookTicker,
        state: UserState,
        market_stats: MarketStatsCache,
    ) -> list[OrderRequest]:
        # 거래소별 캐시 업데이트
        if ticker.exchange == "binance":
            self._binance_tickers[ticker.symbol] = ticker
        elif ticker.exchange == "flipster":
            self._flipster_tickers[ticker.symbol] = ticker

        # 주기적 로깅 (1초마다)
        now = time.time_ns()
        if now - self._last_log_ns > 1_000_000_000:
            self._last_log_ns = now
            logger.info(
                "strategy_tick",
                binance_symbols=len(self._binance_tickers),
                flipster_symbols=len(self._flipster_tickers),
                positions=len(state.positions),
                stats_symbols=len(market_stats),
            )

        return []

    async def on_timer(
        self,
        state: UserState,
        market_stats: MarketStatsCache,
    ) -> list[OrderRequest]:
        return []

    async def on_start(
        self,
        state: UserState,
        market_stats: MarketStatsCache,
    ) -> None:
        logger.info("sample_strategy_started")

    async def on_stop(self) -> None:
        logger.info("sample_strategy_stopped")

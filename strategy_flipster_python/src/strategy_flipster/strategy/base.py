"""Strategy Protocol — Rust trait 대응"""

from __future__ import annotations

from typing import Protocol

from strategy_flipster.market_data.history import SnapshotHistory
from strategy_flipster.market_data.latest_cache import LatestTickerCache
from strategy_flipster.market_data.stats_cache import MarketStatsCache
from strategy_flipster.types import BookTicker, OrderRequest
from strategy_flipster.user_data.state import UserState


class Strategy(Protocol):
    """전략 인터페이스.

    state: 사용자 계정/포지션/잔고 (WS + REST 갱신)
    latest: (exchange, symbol) → 최신 BookTicker 라이브 캐시
    market_stats: 주기 ticker 풀링 캐시 (funding/OI/24h volume 등)
    history: 호가 시계열 롤링 버퍼 (기본 50ms/60s)
    """

    async def on_book_ticker(
        self,
        ticker: BookTicker,
        state: UserState,
        latest: LatestTickerCache,
        market_stats: MarketStatsCache,
        history: SnapshotHistory,
    ) -> list[OrderRequest]:
        """호가 업데이트 시 호출. 주문 요청 리스트 반환."""
        ...

    async def on_timer(
        self,
        state: UserState,
        latest: LatestTickerCache,
        market_stats: MarketStatsCache,
        history: SnapshotHistory,
    ) -> list[OrderRequest]:
        """타이머 트리거 — bookticker 없어도 주기적 실행."""
        ...

    async def on_start(
        self,
        state: UserState,
        latest: LatestTickerCache,
        market_stats: MarketStatsCache,
        history: SnapshotHistory,
    ) -> None:
        """전략 시작 시 초기화"""
        ...

    async def on_stop(self) -> None:
        """전략 종료 시 정리"""
        ...

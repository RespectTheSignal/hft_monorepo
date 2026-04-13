"""Strategy Protocol — Rust trait 대응"""

from __future__ import annotations

from typing import Protocol

from strategy_flipster.market_data.stats_cache import MarketStatsCache
from strategy_flipster.types import BookTicker, OrderRequest
from strategy_flipster.user_data.state import UserState


class Strategy(Protocol):
    """전략 인터페이스.

    - on_book_ticker: 새 호가 데이터 수신 시 호출
    - on_timer: 일정 간격 타이머 트리거 (bookticker 없어도 실행)
    - on_start / on_stop: 전략 라이프사이클

    state: 사용자 계정/포지션/잔고 (WS + REST 갱신)
    market_stats: 주기적 ticker 풀링 캐시 (funding/OI/24h volume 등)
    """

    async def on_book_ticker(
        self,
        ticker: BookTicker,
        state: UserState,
        market_stats: MarketStatsCache,
    ) -> list[OrderRequest]:
        """호가 업데이트 시 호출. 주문 요청 리스트 반환."""
        ...

    async def on_timer(
        self,
        state: UserState,
        market_stats: MarketStatsCache,
    ) -> list[OrderRequest]:
        """타이머 트리거 — bookticker 없어도 주기적 실행."""
        ...

    async def on_start(
        self,
        state: UserState,
        market_stats: MarketStatsCache,
    ) -> None:
        """전략 시작 시 초기화"""
        ...

    async def on_stop(self) -> None:
        """전략 종료 시 정리"""
        ...

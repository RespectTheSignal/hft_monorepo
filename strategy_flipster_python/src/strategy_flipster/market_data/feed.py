"""MarketDataFeed Protocol — Rust trait 대응"""

from __future__ import annotations

from typing import Protocol

from strategy_flipster.types import BookTicker


class MarketDataFeed(Protocol):
    """거래소 데이터 피드 인터페이스.

    각 거래소별 구현체가 이 Protocol을 만족해야 함.
    Rust에서는 `trait MarketDataFeed`로 번역.
    """

    async def connect(self) -> None:
        """피드 연결"""
        ...

    async def disconnect(self) -> None:
        """피드 연결 해제"""
        ...

    async def recv(self) -> BookTicker | None:
        """다음 BookTicker 수신. 연결 종료 시 None 반환."""
        ...

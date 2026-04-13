"""멀티 피드 Aggregator — asyncio.Queue 기반 병합"""

from __future__ import annotations

import asyncio
from typing import Any

import structlog

from strategy_flipster.market_data.feed import MarketDataFeed
from strategy_flipster.types import BookTicker

logger = structlog.get_logger(__name__)


class MarketDataAggregator:
    """여러 MarketDataFeed를 병합하여 단일 스트림으로 제공.

    각 피드별 asyncio.Task가 recv() 루프를 돌며 Queue에 적재.
    Rust 대응: tokio::sync::mpsc::channel
    """

    def __init__(
        self,
        feeds: list[Any],  # list[MarketDataFeed] — Protocol이라 Any 사용
        queue_size: int = 10_000,
    ) -> None:
        self._feeds: list[Any] = feeds
        self._queue: asyncio.Queue[BookTicker] = asyncio.Queue(maxsize=queue_size)
        self._tasks: list[asyncio.Task[None]] = []
        self._running: bool = False

    async def start(self) -> None:
        """모든 피드 연결 및 수신 태스크 시작"""
        self._running = True
        for feed in self._feeds:
            await feed.connect()
            task = asyncio.create_task(self._feed_loop(feed))
            self._tasks.append(task)
        logger.info("aggregator_started", feed_count=len(self._feeds))

    async def stop(self) -> None:
        """모든 피드 태스크 중지 및 연결 해제"""
        self._running = False
        for task in self._tasks:
            task.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()

        for feed in self._feeds:
            await feed.disconnect()
        logger.info("aggregator_stopped")

    async def recv(self) -> BookTicker:
        """다음 BookTicker 반환 (어느 거래소든)"""
        return await self._queue.get()

    def recv_nowait(self) -> BookTicker | None:
        """Non-blocking 수신. 큐가 비어있으면 None."""
        try:
            return self._queue.get_nowait()
        except asyncio.QueueEmpty:
            return None

    @property
    def pending_count(self) -> int:
        """큐에 대기 중인 메시지 수"""
        return self._queue.qsize()

    async def _feed_loop(self, feed: Any) -> None:
        """개별 피드 수신 루프"""
        while self._running:
            try:
                ticker = await feed.recv()
                if ticker is None:
                    logger.warning("feed_recv_returned_none")
                    break
                # 큐가 가득 차면 오래된 데이터 버리고 새 데이터 적재
                if self._queue.full():
                    try:
                        self._queue.get_nowait()
                    except asyncio.QueueEmpty:
                        pass
                self._queue.put_nowait(ticker)
            except asyncio.CancelledError:
                break
            except Exception:
                logger.exception("feed_loop_error")
                await asyncio.sleep(1.0)

"""멀티 피드 Aggregator — asyncio.Queue 기반 병합 + 피드별 재연결"""

from __future__ import annotations

import asyncio
import time
from typing import Any

import structlog

from strategy_flipster.market_data.latest_cache import LatestTickerCache
from strategy_flipster.types import BookTicker

logger = structlog.get_logger(__name__)

# 피드가 N초 이상 무음이면 stale 경고
STALE_THRESHOLD_NS: int = 30 * 1_000_000_000


class MarketDataAggregator:
    """여러 MarketDataFeed를 병합하여 단일 스트림으로 제공.

    각 피드별 asyncio.Task가 recv() 루프를 돌며 Queue에 적재.
    피드가 끊기거나 예외 발생 시 지수 백오프 재연결 루프로 복구.
    Rust 대응: tokio::sync::mpsc::channel
    """

    def __init__(
        self,
        feeds: list[Any],  # list[MarketDataFeed] — Protocol이라 Any 사용
        queue_size: int = 10_000,
        latest_cache: LatestTickerCache | None = None,
    ) -> None:
        self._feeds: list[Any] = feeds
        self._queue: asyncio.Queue[BookTicker] = asyncio.Queue(maxsize=queue_size)
        self._tasks: list[asyncio.Task[None]] = []
        self._running: bool = False
        self._last_recv_ns: dict[int, int] = {}  # id(feed) → ns
        self._latest_cache: LatestTickerCache | None = latest_cache

    async def start(self) -> None:
        """모든 피드 연결 및 수신 태스크 시작"""
        self._running = True
        for feed in self._feeds:
            try:
                await feed.connect()
            except Exception:
                logger.exception("feed_initial_connect_failed")
                # 초기 연결 실패해도 _feed_loop가 재시도
            self._last_recv_ns[id(feed)] = time.time_ns()
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
            try:
                await feed.disconnect()
            except Exception:
                logger.exception("feed_disconnect_failed")
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

    def last_recv_ns(self, feed: Any) -> int:
        return self._last_recv_ns.get(id(feed), 0)

    def stale_feeds(self, threshold_ns: int = STALE_THRESHOLD_NS) -> list[Any]:
        """지정 임계치 이상 무음인 피드 목록"""
        now = time.time_ns()
        return [f for f in self._feeds if now - self.last_recv_ns(f) > threshold_ns]

    async def _feed_loop(self, feed: Any) -> None:
        """개별 피드 수신 루프. 종료/예외/stale 시 재연결 with 지수 백오프.

        recv()는 asyncio.wait_for로 감싸서 stale 감지. ZMQ 소켓은 publisher 사망 시
        내부 재연결만 하고 recv는 무한 대기할 수 있어서 wait_for로 강제 재연결.
        """
        backoff = 1.0
        max_backoff = 30.0
        stale_timeout = STALE_THRESHOLD_NS / 1_000_000_000
        while self._running:
            try:
                ticker = await asyncio.wait_for(feed.recv(), timeout=stale_timeout)
                if ticker is None:
                    logger.warning("feed_recv_returned_none_reconnecting")
                    await self._reconnect(feed)
                    continue
                backoff = 1.0
                self._last_recv_ns[id(feed)] = time.time_ns()
                if self._latest_cache is not None:
                    self._latest_cache.update(ticker)
                if self._queue.full():
                    try:
                        self._queue.get_nowait()
                    except asyncio.QueueEmpty:
                        pass
                self._queue.put_nowait(ticker)
            except asyncio.TimeoutError:
                logger.warning("feed_stale_reconnecting", timeout_sec=stale_timeout)
                await self._reconnect(feed)
            except asyncio.CancelledError:
                break
            except Exception:
                logger.exception("feed_loop_error", backoff=backoff)
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, max_backoff)
                await self._reconnect(feed)

    async def _reconnect(self, feed: Any) -> None:
        """피드 disconnect → connect. 실패는 상위 루프의 백오프로 처리."""
        try:
            await feed.disconnect()
        except Exception:
            pass
        try:
            await feed.connect()
            logger.info("feed_reconnected")
        except Exception:
            logger.exception("feed_reconnect_failed")
            # 다음 루프 반복이 백오프 후 재시도

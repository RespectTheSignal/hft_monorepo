"""Flipster ZMQ PUB 직접 연결 피드.

data_subscriber IPC 대신 Flipster publisher에 ZMQ SUB으로 직접 연결.
Wire format: ZMQ multipart [topic_bytes, 104B payload]
"""

from __future__ import annotations

import structlog
import zmq
import zmq.asyncio

from strategy_flipster.types import (
    FLIPSTER_BT_SIZE,
    BookTicker,
    parse_flipster_bookticker,
)

logger = structlog.get_logger(__name__)


class FlipsterZmqFeed:
    """Flipster data_publisher ZMQ PUB에 직접 SUB 연결.

    IPC Unix socket 대신 ZMQ multipart로 수신:
      [topic_bytes][104B payload]
    topic 예: flipster_bookticker_BTC-USDT-PERP
    """

    def __init__(
        self,
        zmq_address: str,
        topics: list[str] | None = None,
    ) -> None:
        self._address: str = zmq_address
        self._topics: list[str] = topics or []
        self._ctx: zmq.asyncio.Context | None = None
        self._socket: zmq.asyncio.Socket | None = None

    async def connect(self) -> None:
        self._ctx = zmq.asyncio.Context()
        self._socket = self._ctx.socket(zmq.SUB)

        self._socket.setsockopt(zmq.RCVHWM, 100_000)
        self._socket.setsockopt(zmq.RCVBUF, 4 * 1024 * 1024)

        # 토픽 필터: 심볼별 구독 또는 전체
        if self._topics:
            for topic in self._topics:
                # "BTC-USDT-PERP" → "flipster_bookticker_BTC-USDT-PERP"
                prefix = f"flipster_bookticker_{topic}" if not topic.startswith("flipster_") else topic
                self._socket.subscribe(prefix.encode("utf-8"))
        else:
            self._socket.subscribe(b"")

        self._socket.connect(self._address)
        logger.info(
            "flipster_zmq_connected",
            address=self._address,
            topics=self._topics,
        )

    async def disconnect(self) -> None:
        if self._socket is not None:
            self._socket.close()
            self._socket = None
        if self._ctx is not None:
            self._ctx.term()
            self._ctx = None
        logger.info("flipster_zmq_disconnected")

    async def recv(self) -> BookTicker | None:
        """다음 BookTicker 수신"""
        if self._socket is None:
            return None

        parts = await self._socket.recv_multipart()
        if len(parts) != 2:
            return None

        payload = parts[1]
        if len(payload) != FLIPSTER_BT_SIZE:
            return None

        return parse_flipster_bookticker(payload)

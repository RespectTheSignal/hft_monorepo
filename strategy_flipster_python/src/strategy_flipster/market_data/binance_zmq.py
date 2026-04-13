"""Binance ZMQ SUB 클라이언트 — 직접 ZMQ 연결"""

from __future__ import annotations

import structlog
import zmq
import zmq.asyncio

from strategy_flipster.types import (
    BINANCE_BT_SIZE,
    BINANCE_TRADE_SIZE,
    BookTicker,
    Trade,
    parse_binance_bookticker,
    parse_binance_trade,
)

logger = structlog.get_logger(__name__)


class BinanceZmqFeed:
    """Binance ZMQ PUB에 SUB으로 연결하여 BookTicker 수신.

    Wire format: 단일 ZMQ 프레임
      [1B type_len][type_str][payload]
      type = "bookticker" → 120B payload
      type = "trade"      → 128B payload
    """

    def __init__(self, zmq_address: str, topics: list[str] | None = None) -> None:
        self._address: str = zmq_address
        self._topics: list[str] = topics or []
        self._ctx: zmq.asyncio.Context | None = None
        self._socket: zmq.asyncio.Socket | None = None

    async def connect(self) -> None:
        self._ctx = zmq.asyncio.Context()
        self._socket = self._ctx.socket(zmq.SUB)

        # 성능 튜닝
        self._socket.setsockopt(zmq.RCVHWM, 100_000)
        self._socket.setsockopt(zmq.RCVBUF, 4 * 1024 * 1024)

        # 토픽 필터 (빈 리스트 = 모든 메시지 수신)
        if self._topics:
            for topic in self._topics:
                self._socket.subscribe(topic.encode("utf-8"))
        else:
            self._socket.subscribe(b"")

        self._socket.connect(self._address)
        logger.info("binance_zmq_connected", address=self._address, topics=self._topics)

    async def disconnect(self) -> None:
        if self._socket is not None:
            self._socket.close()
            self._socket = None
        if self._ctx is not None:
            self._ctx.term()
            self._ctx = None
        logger.info("binance_zmq_disconnected")

    async def recv(self) -> BookTicker | None:
        """다음 BookTicker 수신. trade 메시지는 무시."""
        if self._socket is None:
            return None

        while True:
            frame: bytes = await self._socket.recv()
            result = self._parse_frame(frame)
            if result is not None:
                return result

    async def recv_any(self) -> BookTicker | Trade | None:
        """BookTicker 또는 Trade 수신"""
        if self._socket is None:
            return None

        frame: bytes = await self._socket.recv()
        return self._parse_frame_any(frame)

    def _parse_frame(self, frame: bytes) -> BookTicker | None:
        """프레임 파싱 — bookticker만 반환"""
        if len(frame) < 2:
            return None

        type_len = frame[0]
        msg_type = frame[1 : 1 + type_len]
        payload = frame[1 + type_len :]

        if msg_type == b"bookticker" and len(payload) == BINANCE_BT_SIZE:
            return parse_binance_bookticker(payload)

        return None

    def _parse_frame_any(self, frame: bytes) -> BookTicker | Trade | None:
        """프레임 파싱 — bookticker, trade 모두 반환"""
        if len(frame) < 2:
            return None

        type_len = frame[0]
        msg_type = frame[1 : 1 + type_len]
        payload = frame[1 + type_len :]

        if msg_type == b"bookticker" and len(payload) == BINANCE_BT_SIZE:
            return parse_binance_bookticker(payload)
        elif msg_type == b"trade" and len(payload) == BINANCE_TRADE_SIZE:
            return parse_binance_trade(payload)

        return None

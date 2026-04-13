"""범용 ZMQ SUB 피드 — 멀티 거래소 publisher 대응.

지원 거래소: binance, gate, bybit, bitget, okx (동일 wire format)
기본 포트: binance=6000, gate=5559, bybit=5558, bitget=6010, okx=6011

Wire format (ZMQ multipart):
  part 0: topic 예) b"binance_bookticker_BTC_USDT", b"binance_trade_BTC_USDT"
  part 1: 120B bookticker payload 또는 128B trade payload
"""

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

_BOOKTICKER_TAG: bytes = b"_bookticker_"
_TRADE_TAG: bytes = b"_trade_"

# 거래소별 기본 ZMQ PUB 포트
DEFAULT_PORTS: dict[str, int] = {
    "binance": 6000,
    "gate": 5559,
    "bybit": 5558,
    "bitget": 6010,
    "okx": 6011,
}


class ExchangeZmqFeed:
    """범용 ZMQ PUB → SUB 피드.

    멀티 거래소 data_publisher와 동일 wire format 사용:
    - BookTicker: 120B (exchange(16) + symbol(32) + 4×f64 + 5×i64)
    - Trade: 128B
    - 프레이밍: [1B type_len][type_str][payload]
    """

    def __init__(
        self,
        zmq_address: str,
        exchange_name: str = "",
        topics: list[str] | None = None,
    ) -> None:
        self._address: str = zmq_address
        self._exchange_name: str = exchange_name
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
        logger.info(
            "zmq_feed_connected",
            address=self._address,
            exchange=self._exchange_name,
            topics=self._topics,
        )

    async def disconnect(self) -> None:
        if self._socket is not None:
            self._socket.close()
            self._socket = None
        if self._ctx is not None:
            self._ctx.term()
            self._ctx = None
        logger.info("zmq_feed_disconnected", exchange=self._exchange_name)

    async def recv(self) -> BookTicker | None:
        """다음 BookTicker 수신. trade 메시지와 파싱 실패 프레임은 skip.

        None 반환 = 소켓 종료. 호출자(aggregator)가 재연결.
        """
        while True:
            if self._socket is None:
                return None
            parts = await self._socket.recv_multipart()
            if len(parts) != 2:
                continue
            topic, payload = parts
            if _BOOKTICKER_TAG in topic and len(payload) == BINANCE_BT_SIZE:
                return parse_binance_bookticker(payload)
            # trade 또는 알 수 없는 프레임은 skip

    async def recv_any(self) -> BookTicker | Trade | None:
        """BookTicker 또는 Trade 수신 (파싱 실패 시 None)"""
        if self._socket is None:
            return None
        parts = await self._socket.recv_multipart()
        if len(parts) != 2:
            return None
        topic, payload = parts
        if _BOOKTICKER_TAG in topic and len(payload) == BINANCE_BT_SIZE:
            return parse_binance_bookticker(payload)
        if _TRADE_TAG in topic and len(payload) == BINANCE_TRADE_SIZE:
            return parse_binance_trade(payload)
        return None

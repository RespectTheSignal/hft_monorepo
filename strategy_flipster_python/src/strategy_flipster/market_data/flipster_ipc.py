"""Flipster IPC Unix socket 클라이언트 — data_subscriber 연결"""

from __future__ import annotations

import asyncio
import struct

import structlog

from strategy_flipster.types import (
    FLIPSTER_BT_SIZE,
    BookTicker,
    parse_flipster_bookticker,
)

logger = structlog.get_logger(__name__)

# IPC 프레임 길이 필드 (4바이트 LE u32)
_LEN_STRUCT: struct.Struct = struct.Struct("<I")


class FlipsterIpcFeed:
    """Flipster data_subscriber IPC Unix socket 클라이언트.

    프로토콜:
      1. 연결 후 등록: REGISTER:{process_id}:{symbols}\n
      2. 수신 프레임: [4B topic_len LE][topic][4B payload_len LE][104B payload]
    """

    def __init__(
        self,
        socket_path: str,
        process_id: str,
        symbols: list[str] | None = None,
    ) -> None:
        self._socket_path: str = socket_path
        self._process_id: str = process_id
        self._symbols: list[str] = symbols or []
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None

    async def connect(self) -> None:
        self._reader, self._writer = await asyncio.open_unix_connection(self._socket_path)

        # 등록 메시지 전송
        symbols_str = ",".join(self._symbols)
        reg_msg = f"REGISTER:{self._process_id}:{symbols_str}\n"
        self._writer.write(reg_msg.encode("utf-8"))
        await self._writer.drain()

        logger.info(
            "flipster_ipc_connected",
            socket_path=self._socket_path,
            symbols=self._symbols,
        )

    async def disconnect(self) -> None:
        if self._writer is not None:
            self._writer.close()
            await self._writer.wait_closed()
            self._writer = None
            self._reader = None
        logger.info("flipster_ipc_disconnected")

    async def recv(self) -> BookTicker | None:
        """다음 BookTicker 수신. 연결 종료 시 None 반환."""
        if self._reader is None:
            return None

        try:
            # topic_len (4B LE)
            topic_len_bytes = await self._reader.readexactly(4)
            (topic_len,) = _LEN_STRUCT.unpack(topic_len_bytes)

            # topic
            _topic = await self._reader.readexactly(topic_len)

            # payload_len (4B LE)
            payload_len_bytes = await self._reader.readexactly(4)
            (payload_len,) = _LEN_STRUCT.unpack(payload_len_bytes)

            # payload
            payload = await self._reader.readexactly(payload_len)

            if payload_len == FLIPSTER_BT_SIZE:
                return parse_flipster_bookticker(payload)

            logger.warning("flipster_ipc_unknown_payload", payload_len=payload_len)
            return None

        except asyncio.IncompleteReadError:
            logger.warning("flipster_ipc_connection_closed")
            return None

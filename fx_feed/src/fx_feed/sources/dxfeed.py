"""dxFeed DXLink WebSocket 소스.

DXLink 프로토콜 (JSON over WSS):
  1) SETUP         → keepaliveTimeout, acceptKeepaliveTimeout
  2) AUTH          → token
  3) CHANNEL_REQUEST(channel=3, service="FEED")
  4) FEED_SETUP    → acceptEventFields { Quote: [...] }
  5) FEED_SUBSCRIPTION → add [{type:"Quote", symbol:"EUR/USD:FXCM"}, ...]
  → 이후 FEED_DATA 메시지로 Quote 배열 수신

Quote 필드: bidPrice, askPrice, bidSize, askSize, time (ms epoch UTC)

참고: https://dxfeed.com/dxlink/
데모 토큰 없으면 env DXFEED_TOKEN 필요. 토큰 없으면 즉시 종료.
"""

from __future__ import annotations

import asyncio
import json
import math
import os
import time

import structlog
import websockets

from fx_feed.questdb_sink import QuestDbSink
from fx_feed.symbol_map import SymbolMap
from fx_feed.types import FxTick

logger = structlog.get_logger(__name__)

_QUOTE_FIELDS: list[str] = [
    "eventSymbol",
    "bidPrice",
    "askPrice",
    "bidSize",
    "askSize",
    "time",
]


class DxFeedSource:
    name: str = "dxfeed"

    def __init__(
        self,
        symbols: list[str],
        symbol_map: SymbolMap,
        ws_url: str,
        token: str | None = None,
    ) -> None:
        self._symbols: list[str] = symbols
        self._map: SymbolMap = symbol_map
        self._ws_url: str = ws_url
        self._token: str = token or os.environ.get("DXFEED_TOKEN", "")

    async def run(self, sink: QuestDbSink) -> None:
        if not self._token:
            logger.error("dxfeed_no_token", hint="set DXFEED_TOKEN in env")
            return

        backoff = 1.0
        while True:
            try:
                logger.info("dxfeed_connecting", symbols=len(self._symbols))
                async with websockets.connect(
                    self._ws_url,
                    ping_interval=20,
                    ping_timeout=10,
                ) as ws:
                    await self._handshake(ws)
                    logger.info("dxfeed_connected")
                    backoff = 1.0
                    async for raw in ws:
                        await self._handle(raw, ws, sink)
            except asyncio.CancelledError:
                raise
            except Exception as e:  # noqa: BLE001
                logger.warning("dxfeed_ws_error", error=str(e), backoff=backoff)
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30.0)

    async def _handshake(self, ws: websockets.WebSocketClientProtocol) -> None:
        await ws.send(json.dumps({
            "type": "SETUP",
            "channel": 0,
            "version": "0.1-fx-feed",
            "keepaliveTimeout": 60,
            "acceptKeepaliveTimeout": 60,
        }))
        await ws.send(json.dumps({
            "type": "AUTH",
            "channel": 0,
            "token": self._token,
        }))
        await ws.send(json.dumps({
            "type": "CHANNEL_REQUEST",
            "channel": 3,
            "service": "FEED",
            "parameters": {"contract": "AUTO"},
        }))
        await ws.send(json.dumps({
            "type": "FEED_SETUP",
            "channel": 3,
            "acceptAggregationPeriod": 0.0,
            "acceptDataFormat": "COMPACT",
            "acceptEventFields": {"Quote": _QUOTE_FIELDS},
        }))
        await ws.send(json.dumps({
            "type": "FEED_SUBSCRIPTION",
            "channel": 3,
            "add": [{"type": "Quote", "symbol": s} for s in self._symbols],
        }))

    async def _handle(
        self,
        raw: str | bytes,
        ws: websockets.WebSocketClientProtocol,
        sink: QuestDbSink,
    ) -> None:
        recv_ns = time.time_ns()
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            return
        parse_ns = time.time_ns()

        mtype = msg.get("type")
        if mtype == "KEEPALIVE":
            await ws.send(json.dumps({"type": "KEEPALIVE", "channel": 0}))
            return
        if mtype != "FEED_DATA":
            return

        # data = [["Quote", [...compact fields...]]]
        # COMPACT format: [eventType, [field1_val1, field2_val1, ..., field1_val2, ...]]
        for entry in msg.get("data", []):
            if not isinstance(entry, list) or len(entry) != 2:
                continue
            evt_type, values = entry
            if evt_type != "Quote" or not isinstance(values, list):
                continue
            n = len(_QUOTE_FIELDS)
            for i in range(0, len(values), n):
                chunk = values[i : i + n]
                if len(chunk) < n:
                    break
                self._emit_quote(chunk, recv_ns, parse_ns, sink)

    def _emit_quote(
        self,
        v: list[object],
        recv_ns: int,
        parse_ns: int,
        sink: QuestDbSink,
    ) -> None:
        try:
            raw_sym = str(v[0])
            bid = float(v[1])  # type: ignore[arg-type]
            ask = float(v[2])  # type: ignore[arg-type]
            bid_sz = float(v[3]) if v[3] is not None else math.nan  # type: ignore[arg-type]
            ask_sz = float(v[4]) if v[4] is not None else math.nan  # type: ignore[arg-type]
            time_ms = int(v[5]) if v[5] is not None else 0  # type: ignore[arg-type]
        except (ValueError, TypeError, IndexError):
            return

        source_ns: int | None = time_ms * 1_000_000 if time_ms else None
        tick = FxTick(
            source=self.name,
            symbol=self._map.to_gate(raw_sym),
            raw_symbol=raw_sym,
            bid_price=bid,
            ask_price=ask,
            bid_size=bid_sz,
            ask_size=ask_sz,
            source_ns=source_ns,
            source_send_ns=None,
            recv_ns=recv_ns,
            parse_ns=parse_ns,
        )
        sink.put_nowait(tick)

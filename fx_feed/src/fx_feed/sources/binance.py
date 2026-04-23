"""Binance spot bookTicker WebSocket 소스.

FX majors는 Binance에 없고, FX-adjacent stable-quoted 쌍만 있음:
  EURUSDT, GBPUSDT, AUDUSDT, JPYUSDT 등.

Wire (combined stream, JSON):
  wss://stream.binance.com:9443/stream?streams=eurusdt@bookTicker/...
  메시지: {"stream": "eurusdt@bookTicker", "data": {
    "u": update_id, "s": "EURUSDT",
    "b": "1.08", "B": "1000", "a": "1.081", "A": "500",
    "E": event_time_ms, "T": transaction_time_ms
  }}

주의: Binance **spot** bookTicker 스트림엔 `E` (event_time) 필드가 없음.
EURUSDT/GBPUSDT 등 FX-adjacent pair는 spot에만 있으므로 source_ns = recv_ns이고,
delay_ms는 NaN으로 기록 (정직하게 "측정 불가"). USD-M futures 스트림엔 E/T가 있지만
거긴 BTC/ETH 등 코인만 있고 FX pair는 없음.

"""

from __future__ import annotations

import asyncio
import json
import math
import time

import structlog
import websockets

from fx_feed.questdb_sink import QuestDbSink
from fx_feed.symbol_map import SymbolMap
from fx_feed.types import FxTick

logger = structlog.get_logger(__name__)


class BinanceSource:
    name: str = "binance"

    def __init__(
        self,
        symbols: list[str],
        symbol_map: SymbolMap,
        ws_url: str = "wss://stream.binance.com:9443/stream",
    ) -> None:
        self._symbols: list[str] = symbols
        self._map: SymbolMap = symbol_map
        self._ws_url: str = ws_url

    def _stream_url(self) -> str:
        streams = "/".join(f"{s.lower()}@bookTicker" for s in self._symbols)
        return f"{self._ws_url}?streams={streams}"

    async def run(self, sink: QuestDbSink) -> None:
        backoff = 1.0
        while True:
            url = self._stream_url()
            try:
                logger.info("binance_connecting", symbols=len(self._symbols))
                async with websockets.connect(
                    url,
                    ping_interval=20,
                    ping_timeout=10,
                    max_size=2**20,
                ) as ws:
                    logger.info("binance_connected")
                    backoff = 1.0
                    async for raw in ws:
                        self._handle(raw, sink)
            except asyncio.CancelledError:
                raise
            except Exception as e:  # noqa: BLE001
                logger.warning("binance_ws_error", error=str(e), backoff=backoff)
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30.0)

    def _handle(self, raw: str | bytes, sink: QuestDbSink) -> None:
        recv_ns = time.time_ns()
        try:
            msg = json.loads(raw)
            data = msg.get("data") or msg
            raw_sym = data["s"]
            bid = float(data["b"])
            ask = float(data["a"])
            bid_sz = float(data.get("B", "nan") or "nan")
            ask_sz = float(data.get("A", "nan") or "nan")
            # Binance 필드:
            #   T = transaction_time (거래소 매칭 시각)
            #   E = event_time (거래소 송신 시각)
            # spot bookTicker엔 둘 다 없음. USD-M futures엔 둘 다 있음.
            tx_ms = int(data.get("T") or 0)
            ev_ms = int(data.get("E") or 0)
        except (KeyError, ValueError, TypeError, json.JSONDecodeError) as e:
            logger.warning("binance_parse_failed", error=str(e))
            return

        parse_ns = time.time_ns()
        source_ns: int | None = tx_ms * 1_000_000 if tx_ms else (
            ev_ms * 1_000_000 if ev_ms else None
        )
        source_send_ns: int | None = (
            ev_ms * 1_000_000 if (ev_ms and tx_ms and ev_ms != tx_ms) else None
        )
        tick = FxTick(
            source=self.name,
            symbol=self._map.to_gate(raw_sym),
            raw_symbol=raw_sym,
            bid_price=bid,
            ask_price=ask,
            bid_size=bid_sz if not math.isnan(bid_sz) else math.nan,
            ask_size=ask_sz if not math.isnan(ask_sz) else math.nan,
            source_ns=source_ns,
            source_send_ns=source_send_ns,
            recv_ns=recv_ns,
            parse_ns=parse_ns,
        )
        sink.put_nowait(tick)

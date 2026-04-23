"""Databento Live 스트리밍 소스.

`databento` Python SDK (optional extra)의 Live 클라이언트로 실시간 MBP-1(또는 유사) 스트림을 받아
Quote 업데이트를 FxTick으로 변환.

설치: `uv sync --extra databento`

Dataset/schema는 config로 받는다. FX는 계약에 따라 dataset이 바뀜:
  - "EFX.FXCM"  : FXCM spot FX
  - "IFEU.IMPACT" : ICE Futures Europe (FX futures 일부)
  - 기타 FX 전용 dataset은 계정마다 권한 다름

schema:
  - "mbp-1"   : top-of-book (bid/ask/size)
  - "bbo-1s"  : 1초 바닥 BBO
  - "trades"  : 체결만 (FX top-of-book 용도엔 부적합)

Databento 메시지는 `ts_event`(ns) 필드가 이미 ns 단위. 그대로 source_ns로 사용.

API key 없으면 즉시 종료.
"""

from __future__ import annotations

import asyncio
import math
import os
import time
from typing import Any

import structlog

from fx_feed.questdb_sink import QuestDbSink
from fx_feed.symbol_map import SymbolMap
from fx_feed.types import FxTick

logger = structlog.get_logger(__name__)


class DatabentoSource:
    name: str = "databento"

    def __init__(
        self,
        symbols: list[str],
        symbol_map: SymbolMap,
        dataset: str,
        schema: str = "mbp-1",
        api_key: str | None = None,
    ) -> None:
        self._symbols: list[str] = symbols
        self._map: SymbolMap = symbol_map
        self._dataset: str = dataset
        self._schema: str = schema
        self._api_key: str = api_key or os.environ.get("DATABENTO_API_KEY", "")

    async def run(self, sink: QuestDbSink) -> None:
        if not self._api_key:
            logger.error("databento_no_api_key", hint="set DATABENTO_API_KEY in env")
            return

        try:
            import databento as db  # type: ignore[import-not-found]
        except ImportError:
            logger.error(
                "databento_sdk_missing",
                hint="uv sync --extra databento  (또는 pip install databento)",
            )
            return

        backoff = 2.0
        while True:
            try:
                logger.info(
                    "databento_connecting",
                    dataset=self._dataset,
                    schema=self._schema,
                    symbols=len(self._symbols),
                )
                client = db.Live(key=self._api_key)
                client.subscribe(
                    dataset=self._dataset,
                    schema=self._schema,
                    symbols=self._symbols,
                    stype_in="raw_symbol",
                )
                backoff = 2.0
                # Databento SDK는 동기 iterator — 별도 스레드에서 돌린다
                await asyncio.get_running_loop().run_in_executor(
                    None, self._consume, client, sink
                )
            except asyncio.CancelledError:
                raise
            except Exception as e:  # noqa: BLE001
                logger.warning("databento_error", error=str(e), backoff=backoff)
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 60.0)

    def _consume(self, client: Any, sink: QuestDbSink) -> None:
        """동기 블로킹 루프. Live iterator가 끊기면 예외 올라감."""
        # 심볼 ID → raw symbol 매핑은 `SymbolMappingMsg`로 들어옴
        sym_by_id: dict[int, str] = {}

        client.start()
        for msg in client:
            # symbol mapping 메시지 처리
            name = type(msg).__name__
            if name == "SymbolMappingMsg":
                try:
                    sym_by_id[int(msg.instrument_id)] = str(msg.stype_in_symbol)
                except Exception:  # noqa: BLE001
                    pass
                continue

            # MBP-1 / BBO 메시지만 관심
            if not hasattr(msg, "levels") and not (
                hasattr(msg, "bid_px") and hasattr(msg, "ask_px")
            ):
                continue

            recv_ns = time.time_ns()
            try:
                raw_sym = sym_by_id.get(int(msg.instrument_id), str(msg.instrument_id))
                if hasattr(msg, "levels") and msg.levels:
                    lvl = msg.levels[0]
                    bid = _px(lvl.bid_px)
                    ask = _px(lvl.ask_px)
                    bid_sz = float(lvl.bid_sz) if lvl.bid_sz is not None else math.nan
                    ask_sz = float(lvl.ask_sz) if lvl.ask_sz is not None else math.nan
                else:
                    bid = _px(msg.bid_px)
                    ask = _px(msg.ask_px)
                    bid_sz = float(getattr(msg, "bid_sz", math.nan) or math.nan)
                    ask_sz = float(getattr(msg, "ask_sz", math.nan) or math.nan)
                source_ns = int(msg.ts_event)
                # Databento는 ts_recv(거래소 수신) vs ts_event(우리쪽 수신) 구분해서 줌
                ts_recv = getattr(msg, "ts_recv", None)
                source_send_ns: int | None = int(ts_recv) if ts_recv else None
            except (AttributeError, ValueError, TypeError):
                continue

            if not (math.isfinite(bid) and math.isfinite(ask)):
                continue

            parse_ns = time.time_ns()
            tick = FxTick(
                source=self.name,
                symbol=self._map.to_gate(raw_sym),
                raw_symbol=raw_sym,
                bid_price=bid,
                ask_price=ask,
                bid_size=bid_sz,
                ask_size=ask_sz,
                source_ns=source_ns,
                source_send_ns=source_send_ns,
                recv_ns=recv_ns,
                parse_ns=parse_ns,
            )
            sink.put_nowait(tick)


def _px(v: Any) -> float:
    """Databento 가격은 fixed-point int (1e-9 스케일) 또는 float.
    int면 1e-9 곱해서 float 변환, float이면 그대로.
    """
    if isinstance(v, int):
        return v * 1e-9
    try:
        return float(v)
    except (TypeError, ValueError):
        return math.nan

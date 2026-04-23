"""QuestDB ILP sink — FxTick 레코드를 `fx_bookticker` 테이블에 적재.

delay_ms 포함. recv_ns를 designated timestamp로 사용.
"""

from __future__ import annotations

import asyncio
import math
import time
from typing import Any

import structlog
from questdb.ingress import Sender, TimestampNanos

from fx_feed.types import FxTick

logger = structlog.get_logger(__name__)


class QuestDbSink:
    """비동기 큐 → 별도 스레드에서 동기 flush.

    QuestDB Python SDK v4 API:
      - Sender는 context manager / establish() 호출 필요
      - Buffer는 sender.new_buffer()로 생성
      - buffer.row(table, symbols={...}, columns={...}, at=...) 고수준 API
    """

    def __init__(
        self,
        conf: str,
        table: str,
        flush_interval_ms: int = 200,
        queue_max: int = 100_000,
    ) -> None:
        self._conf: str = conf
        self._table: str = table
        self._flush_dur: float = flush_interval_ms / 1000.0
        self._queue: asyncio.Queue[FxTick] = asyncio.Queue(maxsize=queue_max)
        self._stop: asyncio.Event = asyncio.Event()

        self._sender: Sender | None = None
        self._buffer: Any = None

        self._recv_count: int = 0
        self._flushed_count: int = 0
        self._flush_errors: int = 0
        self._dropped: int = 0

    async def start(self) -> None:
        loop = asyncio.get_running_loop()
        self._sender = await loop.run_in_executor(None, Sender.from_conf, self._conf)
        assert self._sender is not None
        await loop.run_in_executor(None, self._sender.establish)
        self._buffer = self._sender.new_buffer()
        logger.info("questdb_connected", table=self._table)

    def put_nowait(self, tick: FxTick) -> None:
        """소스에서 호출. 큐 가득차면 drop하고 카운트만 증가."""
        try:
            self._queue.put_nowait(tick)
        except asyncio.QueueFull:
            self._dropped += 1

    async def run(self) -> None:
        """메인 루프: 큐에서 꺼내 Buffer에 쌓고 주기적으로 flush."""
        assert self._sender is not None
        assert self._buffer is not None
        loop = asyncio.get_running_loop()

        last_flush = time.monotonic()
        last_log = time.monotonic()
        buffered = 0

        while not self._stop.is_set():
            timeout = max(0.01, self._flush_dur - (time.monotonic() - last_flush))
            try:
                tick = await asyncio.wait_for(self._queue.get(), timeout=timeout)
                self._buffer_row(tick)
                buffered += 1
                self._recv_count += 1
            except asyncio.TimeoutError:
                pass

            now = time.monotonic()
            if buffered > 0 and (now - last_flush) >= self._flush_dur:
                flushed = await loop.run_in_executor(None, self._do_flush, buffered)
                if flushed >= 0:
                    self._flushed_count += flushed
                else:
                    self._flush_errors += 1
                buffered = 0
                last_flush = now

            if (now - last_log) >= 5.0:
                logger.info(
                    "stats",
                    recv=self._recv_count,
                    flushed=self._flushed_count,
                    dropped=self._dropped,
                    errors=self._flush_errors,
                    qsize=self._queue.qsize(),
                )
                last_log = now

        # 종료 전 최종 flush
        if buffered > 0:
            await loop.run_in_executor(None, self._do_flush, buffered)

    async def stop(self) -> None:
        self._stop.set()

    def _buffer_row(self, tick: FxTick) -> None:
        """FxTick → ILP row. 여러 단계의 delay를 모두 기록한다.

        타임스탬프 체인:
          source_ns (거래소 event)
            → source_send_ns (거래소 송신, 있으면)
            → recv_ns        (WS 수신)
            → parse_ns       (JSON parse 완료)
            → buffer_ns      (ILP buffer에 기록, =write_ns)
            → (flush_ns)     (실제 QuestDB 전송, 암묵적)

        Delay breakdown:
          exchange_delay_ms  = source_send - source_ns  (거래소 내부)
          wire_delay_ms      = recv_ns - source_ns       (네트워크 포함)
          parse_delay_ms     = parse_ns - recv_ns        (JSON 파싱)
          queue_delay_ms     = buffer_ns - parse_ns      (async queue 대기)
          total_delay_ms     = buffer_ns - source_ns     (end-to-end)
        """
        assert self._buffer is not None
        buffer_ns = time.time_ns()

        cols: dict[str, Any] = {
            "bid_price": tick.bid_price,
            "ask_price": tick.ask_price,
            "recv_ts": TimestampNanos(tick.recv_ns),
            "parse_ts": TimestampNanos(tick.parse_ns),
            "buffer_ts": TimestampNanos(buffer_ns),
            "parse_delay_ms": tick.parse_delay_ms,
            "queue_delay_ms": (buffer_ns - tick.parse_ns) / 1e6,
            "local_delay_ms": (buffer_ns - tick.recv_ns) / 1e6,
        }
        if tick.source_ns is not None:
            cols["source_ts"] = TimestampNanos(tick.source_ns)
            cols["wire_delay_ms"] = tick.wire_delay_ms
            # delay_ms는 wire_delay_ms의 별칭 — 익숙한 이름 유지
            cols["delay_ms"] = tick.wire_delay_ms
            cols["total_delay_ms"] = (buffer_ns - tick.source_ns) / 1e6
        if tick.source_send_ns is not None and tick.source_ns is not None:
            cols["source_send_ts"] = TimestampNanos(tick.source_send_ns)
            cols["exchange_delay_ms"] = tick.exchange_delay_ms
        if not math.isnan(tick.bid_size):
            cols["bid_size"] = tick.bid_size
        if not math.isnan(tick.ask_size):
            cols["ask_size"] = tick.ask_size
        self._buffer.row(
            self._table,
            symbols={
                "source": tick.source,
                "symbol": tick.symbol,
                "raw_symbol": tick.raw_symbol,
            },
            columns=cols,
            at=TimestampNanos(buffer_ns),
        )

    def _do_flush(self, n: int) -> int:
        """동기 flush. 성공 시 row 수, 실패 시 -1."""
        assert self._sender is not None
        assert self._buffer is not None
        try:
            self._sender.flush(self._buffer)
            return n
        except Exception as e:  # noqa: BLE001
            logger.error("questdb_flush_failed", rows=n, error=str(e))
            self._buffer.clear()
            return -1

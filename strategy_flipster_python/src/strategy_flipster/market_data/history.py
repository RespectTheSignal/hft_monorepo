"""SnapshotHistory — (거래소, 심볼)별 numpy 링 버퍼 시계열 저장.

설계:
- 심볼당 고정 capacity 의 columnar 저장 (ts/bid/ask/mid 각 1D numpy array)
- 2×capacity scratch 영역에 sequential 추가, 가득 차면 뒤쪽 capacity만큼을
  앞으로 memcpy (amortized O(1) append, 항상 chronological 순서 보장)
- 조회는 np.searchsorted 바이너리 검색 + 슬라이스 뷰 (concat 없음)

Rust 번역:
- SymbolRingBuffer → struct { ts: Vec<i64>, bid/ask/mid: Vec<f64>,
                              capacity: usize, n: usize }
- numpy slice → Rust &[f64] 슬라이스
- searchsorted → slice.partition_point(|&x| x < cutoff)
- SnapshotHistory → HashMap<(String, String), SymbolRingBuffer>
- SnapshotSampler → tokio::time::interval + 루프
"""

from __future__ import annotations

import asyncio
import time

import numpy as np
import structlog

from strategy_flipster.clock import Clock, LiveClock
from strategy_flipster.market_data.latest_cache import LatestTickerCache

logger = structlog.get_logger(__name__)

# capacity 여유율 — 1200 샘플을 안정적으로 담기 위해 실제 capacity는 1.1배로 확보
_CAPACITY_SLACK: float = 1.1


def _required_capacity(interval_sec: float, max_age_sec: float) -> int:
    base = max(1, int(max_age_sec / interval_sec))
    return int(base * _CAPACITY_SLACK) + 1


class SymbolRingBuffer:
    """단일 (exchange, symbol) 의 numpy 링 버퍼.

    scratch 크기 = 2 * capacity. append로 n이 scratch에 도달하면
    뒤쪽 capacity개를 앞으로 memcpy. 결과적으로 ts[0:n]은 항상 시간순 정렬.
    """

    __slots__ = ("_capacity", "_scratch", "_n", "ts", "bid", "ask", "mid")

    def __init__(self, capacity: int) -> None:
        self._capacity: int = capacity
        self._scratch: int = capacity * 2
        self._n: int = 0
        self.ts: np.ndarray = np.zeros(self._scratch, dtype=np.int64)
        self.bid: np.ndarray = np.zeros(self._scratch, dtype=np.float64)
        self.ask: np.ndarray = np.zeros(self._scratch, dtype=np.float64)
        self.mid: np.ndarray = np.zeros(self._scratch, dtype=np.float64)

    @property
    def count(self) -> int:
        return self._n

    @property
    def capacity(self) -> int:
        return self._capacity

    def append(self, ts_ns: int, bid: float, ask: float) -> None:
        i = self._n
        self.ts[i] = ts_ns
        self.bid[i] = bid
        self.ask[i] = ask
        # mid: 한쪽이라도 0이면 0 (결측 처리)
        self.mid[i] = (bid + ask) * 0.5 if bid > 0.0 and ask > 0.0 else 0.0
        self._n = i + 1
        if self._n >= self._scratch:
            # 뒤쪽 capacity개를 앞으로 shift — 연속 구간이라 numpy copy 1회
            cap = self._capacity
            self.ts[:cap] = self.ts[self._scratch - cap:self._scratch]
            self.bid[:cap] = self.bid[self._scratch - cap:self._scratch]
            self.ask[:cap] = self.ask[self._scratch - cap:self._scratch]
            self.mid[:cap] = self.mid[self._scratch - cap:self._scratch]
            self._n = cap

    def _start_index(self, cutoff_ns: int) -> int:
        """ts[start_idx:_n] >= cutoff_ns 가 되는 최소 인덱스"""
        if self._n == 0:
            return 0
        return int(np.searchsorted(self.ts[:self._n], cutoff_ns, side="left"))

    # ── 슬라이스 뷰 ──

    def mid_slice(self, cutoff_ns: int) -> np.ndarray:
        start = self._start_index(cutoff_ns)
        return self.mid[start:self._n]

    def bid_slice(self, cutoff_ns: int) -> np.ndarray:
        start = self._start_index(cutoff_ns)
        return self.bid[start:self._n]

    def ask_slice(self, cutoff_ns: int) -> np.ndarray:
        start = self._start_index(cutoff_ns)
        return self.ask[start:self._n]

    def ts_slice(self, cutoff_ns: int) -> np.ndarray:
        start = self._start_index(cutoff_ns)
        return self.ts[start:self._n]

    def spread_slice(self, cutoff_ns: int) -> np.ndarray:
        """심볼 자체의 (ask − bid) 시계열 — numpy 벡터 감산"""
        start = self._start_index(cutoff_ns)
        end = self._n
        return self.ask[start:end] - self.bid[start:end]

    def latest(self) -> tuple[int, float, float, float] | None:
        if self._n == 0:
            return None
        i = self._n - 1
        return int(self.ts[i]), float(self.bid[i]), float(self.ask[i]), float(self.mid[i])


class SnapshotHistory:
    """(exchange, symbol) → SymbolRingBuffer 맵 + 전략용 헬퍼.

    clock: now_ns() 소스. 라이브는 LiveClock, 백테스트는 SimClock 주입.
    """

    __slots__ = (
        "_interval_sec", "_max_age_sec", "_max_age_ns",
        "_capacity", "_buffers", "_clock",
    )

    def __init__(
        self,
        interval_sec: float = 0.05,
        max_age_sec: float = 60.0,
        clock: Clock | None = None,
    ) -> None:
        self._interval_sec: float = interval_sec
        self._max_age_sec: float = max_age_sec
        self._max_age_ns: int = int(max_age_sec * 1_000_000_000)
        self._capacity: int = _required_capacity(interval_sec, max_age_sec)
        self._buffers: dict[tuple[str, str], SymbolRingBuffer] = {}
        self._clock: Clock = clock if clock is not None else LiveClock()

    @property
    def capacity(self) -> int:
        return self._capacity

    @property
    def max_age_sec(self) -> float:
        return self._max_age_sec

    def _get_or_create(self, exchange: str, symbol: str) -> SymbolRingBuffer:
        key = (exchange, symbol)
        buf = self._buffers.get(key)
        if buf is None:
            buf = SymbolRingBuffer(self._capacity)
            self._buffers[key] = buf
        return buf

    def append(self, exchange: str, symbol: str, ts_ns: int, bid: float, ask: float) -> None:
        self._get_or_create(exchange, symbol).append(ts_ns, bid, ask)

    def keys(self) -> list[tuple[str, str]]:
        return list(self._buffers.keys())

    def buffer(self, exchange: str, symbol: str) -> SymbolRingBuffer | None:
        return self._buffers.get((exchange, symbol))

    def __len__(self) -> int:
        return sum(b.count for b in self._buffers.values())

    def series_count(self) -> int:
        return len(self._buffers)

    # ── 전략 조회 API (numpy 벡터화) ──

    def _cutoff_ns(self, duration_sec: float | None) -> int:
        if duration_sec is None:
            return 0
        return self._clock.now_ns() - int(duration_sec * 1_000_000_000)

    def mid_array(
        self, exchange: str, symbol: str, duration_sec: float | None = None,
    ) -> np.ndarray:
        """최근 구간 mid 배열 (numpy view)"""
        buf = self._buffers.get((exchange, symbol))
        if buf is None:
            return np.empty(0, dtype=np.float64)
        return buf.mid_slice(self._cutoff_ns(duration_sec))

    def bid_array(
        self, exchange: str, symbol: str, duration_sec: float | None = None,
    ) -> np.ndarray:
        buf = self._buffers.get((exchange, symbol))
        if buf is None:
            return np.empty(0, dtype=np.float64)
        return buf.bid_slice(self._cutoff_ns(duration_sec))

    def ask_array(
        self, exchange: str, symbol: str, duration_sec: float | None = None,
    ) -> np.ndarray:
        buf = self._buffers.get((exchange, symbol))
        if buf is None:
            return np.empty(0, dtype=np.float64)
        return buf.ask_slice(self._cutoff_ns(duration_sec))

    def ts_array(
        self, exchange: str, symbol: str, duration_sec: float | None = None,
    ) -> np.ndarray:
        buf = self._buffers.get((exchange, symbol))
        if buf is None:
            return np.empty(0, dtype=np.int64)
        return buf.ts_slice(self._cutoff_ns(duration_sec))

    def spread_array(
        self, exchange: str, symbol: str, duration_sec: float | None = None,
    ) -> np.ndarray:
        """심볼의 (ask − bid) 시계열 — 호가 스프레드"""
        buf = self._buffers.get((exchange, symbol))
        if buf is None:
            return np.empty(0, dtype=np.float64)
        return buf.spread_slice(self._cutoff_ns(duration_sec))

    def latest(self, exchange: str, symbol: str) -> tuple[int, float, float, float] | None:
        buf = self._buffers.get((exchange, symbol))
        if buf is None:
            return None
        return buf.latest()

    # ── aggregate helpers ──

    def mid_mean(
        self, exchange: str, symbol: str, duration_sec: float,
    ) -> float:
        arr = self.mid_array(exchange, symbol, duration_sec)
        if arr.size == 0:
            return 0.0
        return float(arr.mean())

    def avg_spread(
        self, exchange: str, symbol: str, duration_sec: float,
    ) -> float:
        """심볼 자체 호가 스프레드(ask − bid)의 구간 평균"""
        arr = self.spread_array(exchange, symbol, duration_sec)
        if arr.size == 0:
            return 0.0
        return float(arr.mean())

    def cross_mid_diff_array(
        self,
        exchange_a: str, symbol_a: str,
        exchange_b: str, symbol_b: str,
        duration_sec: float | None = None,
    ) -> np.ndarray:
        """거래소 간 mid 차이 시계열 (a − b). 시간 정렬됨.

        SnapshotSampler가 매 tick마다 동일 ts로 모든 심볼을 쓰므로,
        두 심볼 모두 존재하는 tick부터는 ts가 elementwise 일치. 공통 시작 시각
        이후로 잘라서 길이 맞춰 감산.
        """
        buf_a = self._buffers.get((exchange_a, symbol_a))
        buf_b = self._buffers.get((exchange_b, symbol_b))
        if buf_a is None or buf_b is None:
            return np.empty(0, dtype=np.float64)
        cutoff = self._cutoff_ns(duration_sec)
        ts_a = buf_a.ts_slice(cutoff)
        ts_b = buf_b.ts_slice(cutoff)
        if ts_a.size == 0 or ts_b.size == 0:
            return np.empty(0, dtype=np.float64)
        mid_a = buf_a.mid_slice(cutoff)
        mid_b = buf_b.mid_slice(cutoff)
        # 더 늦게 시작한 쪽 기준으로 정렬
        t_start = max(int(ts_a[0]), int(ts_b[0]))
        idx_a = int(np.searchsorted(ts_a, t_start, side="left"))
        idx_b = int(np.searchsorted(ts_b, t_start, side="left"))
        a = mid_a[idx_a:]
        b = mid_b[idx_b:]
        n = min(a.size, b.size)
        if n == 0:
            return np.empty(0, dtype=np.float64)
        return a[:n] - b[:n]

    def cross_mid_diff_mean(
        self,
        exchange_a: str, symbol_a: str,
        exchange_b: str, symbol_b: str,
        duration_sec: float,
    ) -> float:
        """거래소 간 mid 차이 구간 평균 (정렬된 시계열 기준)"""
        arr = self.cross_mid_diff_array(
            exchange_a, symbol_a, exchange_b, symbol_b, duration_sec,
        )
        if arr.size == 0:
            return 0.0
        return float(arr.mean())

    def cross_mid_diff_latest(
        self,
        exchange_a: str, symbol_a: str,
        exchange_b: str, symbol_b: str,
    ) -> float:
        """현재 거래소 간 mid 차이 (마지막 공통 tick)"""
        arr = self.cross_mid_diff_array(
            exchange_a, symbol_a, exchange_b, symbol_b, duration_sec=None,
        )
        if arr.size == 0:
            return 0.0
        return float(arr[-1])

    # 하위 호환 alias — 기존 spread_mean 사용처가 있다면 바로 안 깨지도록 유지
    spread_mean = cross_mid_diff_mean


class SnapshotSampler:
    """LatestTickerCache → SnapshotHistory 주기 샘플링 태스크.

    interval 주기로 latest_cache 전체를 스캔해서 각 심볼의 현재 bid/ask를
    history에 append. 스케줄 드리프트 보정.
    """

    def __init__(
        self,
        cache: LatestTickerCache,
        history: SnapshotHistory,
        interval_sec: float = 0.05,
    ) -> None:
        self._cache: LatestTickerCache = cache
        self._history: SnapshotHistory = history
        self._interval: float = interval_sec
        self._running: bool = False
        self._sample_count: int = 0
        self._last_elapsed_us: int = 0

    @property
    def sample_count(self) -> int:
        return self._sample_count

    @property
    def last_elapsed_us(self) -> int:
        return self._last_elapsed_us

    async def start(self) -> None:
        self._running = True
        logger.info(
            "snapshot_sampler_started",
            interval_ms=int(self._interval * 1000),
            capacity=self._history.capacity,
        )
        next_tick = time.monotonic()
        # 지역 바인딩으로 속성 조회 비용 제거
        cache = self._cache
        hist = self._history
        while self._running:
            t0 = time.perf_counter_ns()
            now_ns = time.time_ns()
            try:
                # dict_items 뷰 이터레이션 — 리스트 할당 없음. asyncio 단일 스레드라
                # 이 await 없는 구간에서는 dict 변경이 발생하지 않음.
                for (exch, sym), ticker in cache.items():
                    hist.append(exch, sym, now_ns, ticker.bid_price, ticker.ask_price)
                self._sample_count += 1
                self._last_elapsed_us = (time.perf_counter_ns() - t0) // 1000
            except asyncio.CancelledError:
                break
            except Exception:
                logger.exception("snapshot_sample_failed")

            next_tick += self._interval
            delay = next_tick - time.monotonic()
            if delay < 0:
                next_tick = time.monotonic()
                delay = 0.0
            try:
                await asyncio.sleep(delay)
            except asyncio.CancelledError:
                break
        logger.info("snapshot_sampler_stopped", total_samples=self._sample_count)

    async def stop(self) -> None:
        self._running = False

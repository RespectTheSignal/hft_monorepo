"""백테스트 러너 — QuestDB 이벤트 머지 + 시뮬 루프.

시간순 머지: binance_bookticker iterator + flipster_bookticker iterator 를
heapq 기반 k-way merge 로 병합. 각 이벤트마다:
  1. SimClock 전진
  2. LatestTickerCache 갱신
  3. 다음 50ms 스냅샷 경계 도달 시 SnapshotHistory.append 전체 심볼
  4. Strategy.on_book_ticker 호출 → 주문 리스트
  5. FillSimulator.submit + FillSimulator.process (현재 시각 기준)

이벤트 스트림이 끝나면 pending 주문들을 end_ns 에서 강제 처리.
"""

from __future__ import annotations

import asyncio
import heapq
import time
from collections.abc import Iterator
from dataclasses import dataclass, field

import structlog

from strategy_flipster.backtest.fill_simulator import FillSimulator
from strategy_flipster.backtest.pnl_tracker import PnlTracker
from strategy_flipster.clock import SimClock
from strategy_flipster.market_data.history import SnapshotHistory
from strategy_flipster.market_data.latest_cache import LatestTickerCache
from strategy_flipster.market_data.stats_cache import MarketStatsCache
from strategy_flipster.strategy.basis_meanrev import BasisMeanRevStrategy
from strategy_flipster.types import BookTicker
from strategy_flipster.user_data.state import UserState

logger = structlog.get_logger(__name__)


@dataclass
class BacktestConfig:
    start_ns: int
    end_ns: int
    snapshot_interval_ns: int = 50_000_000    # 50ms
    history_max_age_sec: float = 60.0
    progress_log_interval_sec: float = 30.0


@dataclass
class BacktestResult:
    start_ns: int
    end_ns: int
    wall_elapsed_sec: float
    events_processed: int
    snapshots_taken: int
    orders_submitted: int
    orders_filled: int
    orders_missed: int
    trades: int
    total_realized: float
    total_fees: float
    total_volume: float
    net_pnl: float
    peak_equity: float
    max_drawdown: float
    wins: int
    losses: int
    peak_equity_mtm: float = 0.0
    max_drawdown_mtm: float = 0.0
    worst_unrealized: float = 0.0


def _merge_events(
    streams: list[Iterator[BookTicker]],
) -> Iterator[BookTicker]:
    """timestamp 오름차순 k-way merge (heapq). 동시 이벤트는 stream index 순."""
    heap: list[tuple[int, int, BookTicker]] = []
    iters: list[Iterator[BookTicker]] = list(streams)

    for idx, it in enumerate(iters):
        try:
            tick = next(it)
            heapq.heappush(heap, (tick.recv_ts_ns, idx, tick))
        except StopIteration:
            pass

    while heap:
        ts, idx, tick = heapq.heappop(heap)
        yield tick
        try:
            nxt = next(iters[idx])
            heapq.heappush(heap, (nxt.recv_ts_ns, idx, nxt))
        except StopIteration:
            pass


class BacktestRunner:
    """단일 백테스트 실행기"""

    def __init__(
        self,
        config: BacktestConfig,
        event_streams: list[Iterator[BookTicker]],
        strategy: BasisMeanRevStrategy,
        clock: SimClock,
        history: SnapshotHistory,
        latest: LatestTickerCache,
        user_state: UserState,
        fill_sim: FillSimulator,
        pnl: PnlTracker,
        market_stats: MarketStatsCache,
    ) -> None:
        self._cfg: BacktestConfig = config
        self._streams: list[Iterator[BookTicker]] = event_streams
        self._strategy: BasisMeanRevStrategy = strategy
        self._clock: SimClock = clock
        self._history: SnapshotHistory = history
        self._latest: LatestTickerCache = latest
        self._user_state: UserState = user_state
        self._fill_sim: FillSimulator = fill_sim
        self._pnl: PnlTracker = pnl
        self._market_stats: MarketStatsCache = market_stats

    async def run(self) -> BacktestResult:
        wall_t0 = time.monotonic()
        events = 0
        snapshots = 0
        next_snapshot_ns = self._cfg.start_ns
        next_progress_ns = self._cfg.start_ns + int(self._cfg.progress_log_interval_sec * 1e9)

        self._clock.set(self._cfg.start_ns)
        await self._strategy.on_start(
            self._user_state, self._latest, self._market_stats, self._history,
        )

        logger.info(
            "backtest_start",
            start_ns=self._cfg.start_ns,
            end_ns=self._cfg.end_ns,
            duration_sec=(self._cfg.end_ns - self._cfg.start_ns) / 1e9,
        )

        merged = _merge_events(self._streams)

        for tick in merged:
            if tick.recv_ts_ns >= self._cfg.end_ns:
                break
            events += 1

            # 1. SimClock 전진
            self._clock.set(tick.recv_ts_ns)

            # 2. latest_cache 갱신
            self._latest.update(tick)

            # 3. 스냅샷 경계 체크 — 여러 번 뛰어넘을 수 있음 (gap)
            while next_snapshot_ns <= tick.recv_ts_ns:
                self._take_snapshot(next_snapshot_ns)
                snapshots += 1
                # MTM equity 갱신 (스냅샷 경계마다 1회)
                self._mark_to_market()
                next_snapshot_ns += self._cfg.snapshot_interval_ns

            # 4. pending 주문 체결 시도
            self._fill_sim.process(tick.recv_ts_ns, self._latest)

            # 5. strategy 호출
            orders = await self._strategy.on_book_ticker(
                tick, self._user_state, self._latest, self._market_stats, self._history,
            )
            if orders:
                self._fill_sim.submit(orders, tick.recv_ts_ns)

            # 진행 로그
            if tick.recv_ts_ns >= next_progress_ns:
                elapsed_sim = (tick.recv_ts_ns - self._cfg.start_ns) / 1e9
                total = (self._cfg.end_ns - self._cfg.start_ns) / 1e9
                s = self._fill_sim.stats
                p = self._pnl.stats
                logger.info(
                    "backtest_progress",
                    sim_sec=round(elapsed_sim, 1),
                    total_sec=round(total, 1),
                    events=events,
                    snapshots=snapshots,
                    filled=s.filled,
                    missed=s.missed,
                    trades=p.total_trades,
                    realized=round(p.total_realized, 4),
                    fees=round(p.total_fees, 4),
                    net=round(p.total_realized - p.total_fees, 4),
                )
                next_progress_ns += int(self._cfg.progress_log_interval_sec * 1e9)

        # 종료 — 남은 pending 을 end_ns 에서 처리
        self._clock.set(self._cfg.end_ns)
        self._fill_sim.process(self._cfg.end_ns, self._latest)
        await self._strategy.on_stop()

        wall_elapsed = time.monotonic() - wall_t0
        s = self._fill_sim.stats
        p = self._pnl.stats
        return BacktestResult(
            start_ns=self._cfg.start_ns,
            end_ns=self._cfg.end_ns,
            wall_elapsed_sec=wall_elapsed,
            events_processed=events,
            snapshots_taken=snapshots,
            orders_submitted=s.submitted,
            orders_filled=s.filled,
            orders_missed=s.missed,
            trades=p.total_trades,
            total_realized=p.total_realized,
            total_fees=p.total_fees,
            total_volume=p.total_volume_usd,
            net_pnl=p.total_realized - p.total_fees,
            peak_equity=p.peak_equity,
            max_drawdown=p.max_drawdown,
            wins=p.wins,
            losses=p.losses,
            peak_equity_mtm=p.peak_equity_mtm,
            max_drawdown_mtm=p.max_drawdown_mtm,
            worst_unrealized=p.worst_unrealized,
        )

    def _take_snapshot(self, ns: int) -> None:
        """현재 latest_cache 상태를 history 에 sample. clock 은 이미 set 돼 있음."""
        for (exch, sym), ticker in self._latest.items():
            self._history.append(exch, sym, ns, ticker.bid_price, ticker.ask_price)

    def _mark_to_market(self) -> None:
        """현재 포지션을 Flipster mid 로 평가해 PnlTracker 에 반영"""
        # positions 는 fl_sym 키 — Flipster latest 에서 mid 추출
        if not self._user_state.positions:
            return
        from strategy_flipster.market_data.symbol import EXCHANGE_FLIPSTER
        marks: dict[str, float] = {}
        for sym in self._user_state.positions.keys():
            t = self._latest.get(EXCHANGE_FLIPSTER, sym)
            if t is None:
                continue
            mid = (t.bid_price + t.ask_price) * 0.5
            if mid > 0:
                marks[sym] = mid
        if marks:
            self._pnl.mark_to_market(marks)

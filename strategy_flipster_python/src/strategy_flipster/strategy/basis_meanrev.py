"""Basis mean-reversion 전략 — Flipster 단방향.

시그널:
  z_t = (basis_t − mean(basis, window)) / std(basis, window)
  basis_t = fl_mid − bn_mid  (마지막 공통 tick 기준)

진입:
  z_t ≥  k_in   → SELL Flipster (basis 높음, flipster 가 하락 반전 기대)
  z_t ≤ −k_in   → BUY  Flipster (basis 낮음, flipster 가 상승 반전 기대)

청산:
  |z| ≤ k_out          → 복귀 (target)
  |z| ≥ k_stop         → 손절
  hold_ns ≥ timeout_ns → 시간 손절

주문:
  LIMIT IOC, price = 반대편 호가 (immediate cross). 수량은 notional_usd / mid.

상태:
  strategy 내부에 symbol 별 "활성 포지션 의도"(SELL/BUY + 진입 ts + 진입가) 저장.
  UserState.positions 는 보조 — FillSimulator/실계정이 갱신.

Rust:
  struct BasisMeanRev { params, active: HashMap<Symbol, ActiveTrade> }
"""

from __future__ import annotations

from dataclasses import dataclass, field
from decimal import Decimal

import structlog

from strategy_flipster.clock import Clock, LiveClock
from strategy_flipster.market_data.history import SnapshotHistory
from strategy_flipster.market_data.latest_cache import LatestTickerCache
from strategy_flipster.market_data.stats_cache import MarketStatsCache
from strategy_flipster.market_data.symbol import (
    EXCHANGE_BINANCE,
    EXCHANGE_FLIPSTER,
    to_exchange_symbol,
)
from strategy_flipster.types import (
    BookTicker,
    OrderRequest,
    OrderSide,
    OrderType,
    TimeInForce,
)
from strategy_flipster.user_data.state import UserState

logger = structlog.get_logger(__name__)


@dataclass(frozen=True, slots=True)
class BasisMeanRevParams:
    """전략 파라미터"""

    canonicals: tuple[str, ...]          # 거래 대상 (예: ("EPIC", "ETH"))
    window_sec: float = 30.0             # z-score 계산 윈도우
    k_in: float = 2.0                    # 진입 threshold
    k_out: float = 0.5                   # 청산 threshold
    k_stop: float = 4.0                  # 손절 threshold
    timeout_sec: float = 10.0            # 시간 손절
    notional_usd: float = 20.0           # 1 주문 notional
    min_std: float = 1e-8                # std 이하이면 skip
    warmup_samples: int = 200            # 최소 샘플
    max_concurrent_per_symbol: int = 1   # 심볼별 동시 포지션 상한


@dataclass
class _ActiveTrade:
    """활성 포지션 의도 — OrderManager 응답 없이 로컬 관리"""

    canonical: str
    fl_sym: str
    side: OrderSide                      # 우리가 Flipster 에 건 방향
    entry_ns: int
    entry_price: float
    qty: Decimal


@dataclass
class BasisMeanRevStats:
    """실행 통계 — 디버그/분석용"""

    entries: int = 0
    exits_target: int = 0
    exits_stop: int = 0
    exits_timeout: int = 0
    signals_seen: int = 0
    skips_no_data: int = 0
    skips_low_std: int = 0
    skips_already_active: int = 0
    last_log_ns: int = 0


class BasisMeanRevStrategy:
    """Flipster 단방향 basis mean-reversion.

    Live/Backtest 공통. clock 주입으로 현재 시각 획득.
    """

    def __init__(
        self,
        params: BasisMeanRevParams,
        clock: Clock | None = None,
    ) -> None:
        self._p: BasisMeanRevParams = params
        self._clock: Clock = clock if clock is not None else LiveClock()
        self._active: dict[str, _ActiveTrade] = {}  # fl_sym → trade
        self._stats: BasisMeanRevStats = BasisMeanRevStats()

    @property
    def stats(self) -> BasisMeanRevStats:
        return self._stats

    @property
    def active_positions(self) -> dict[str, _ActiveTrade]:
        return self._active

    # ── Strategy Protocol ──

    async def on_start(
        self,
        state: UserState,
        latest: LatestTickerCache,
        market_stats: MarketStatsCache,
        history: SnapshotHistory,
    ) -> None:
        logger.info(
            "basis_meanrev_started",
            canonicals=list(self._p.canonicals),
            window_sec=self._p.window_sec,
            k_in=self._p.k_in,
            k_out=self._p.k_out,
            k_stop=self._p.k_stop,
            timeout_sec=self._p.timeout_sec,
            notional_usd=self._p.notional_usd,
        )

    async def on_stop(self) -> None:
        s = self._stats
        logger.info(
            "basis_meanrev_stopped",
            entries=s.entries,
            target=s.exits_target,
            stop=s.exits_stop,
            timeout=s.exits_timeout,
            signals=s.signals_seen,
        )

    async def on_book_ticker(
        self,
        ticker: BookTicker,
        state: UserState,
        latest: LatestTickerCache,
        market_stats: MarketStatsCache,
        history: SnapshotHistory,
    ) -> list[OrderRequest]:
        return self._evaluate_all(latest, history)

    async def on_timer(
        self,
        state: UserState,
        latest: LatestTickerCache,
        market_stats: MarketStatsCache,
        history: SnapshotHistory,
    ) -> list[OrderRequest]:
        return self._evaluate_all(latest, history)

    # ── 내부 로직 ──

    def _evaluate_all(
        self,
        latest: LatestTickerCache,
        history: SnapshotHistory,
    ) -> list[OrderRequest]:
        orders: list[OrderRequest] = []
        now_ns = self._clock.now_ns()
        for canonical in self._p.canonicals:
            fl_sym = to_exchange_symbol(EXCHANGE_FLIPSTER, canonical)
            bn_sym = to_exchange_symbol(EXCHANGE_BINANCE, canonical)
            self._stats.signals_seen += 1
            order = self._evaluate_symbol(
                canonical, fl_sym, bn_sym, now_ns, latest, history,
            )
            if order is not None:
                orders.append(order)
        return orders

    def _evaluate_symbol(
        self,
        canonical: str,
        fl_sym: str,
        bn_sym: str,
        now_ns: int,
        latest: LatestTickerCache,
        history: SnapshotHistory,
    ) -> OrderRequest | None:
        # 1. 활성 포지션이 있으면 청산 조건만 확인
        active = self._active.get(fl_sym)
        if active is not None:
            return self._maybe_exit(active, now_ns, latest, history)

        # 2. 신규 진입 조건 평가
        diff = history.cross_mid_diff_array(
            EXCHANGE_FLIPSTER, fl_sym,
            EXCHANGE_BINANCE, bn_sym,
            duration_sec=self._p.window_sec,
        )
        if diff.size < self._p.warmup_samples:
            self._stats.skips_no_data += 1
            return None

        mean = float(diff.mean())
        std = float(diff.std())
        if std < self._p.min_std:
            self._stats.skips_low_std += 1
            return None

        basis_now = float(diff[-1])
        z = (basis_now - mean) / std

        if abs(z) < self._p.k_in:
            return None  # 진입 threshold 미달

        # 3. 현재 호가 필요 — Flipster 쪽
        fl_ticker = latest.get(EXCHANGE_FLIPSTER, fl_sym)
        if fl_ticker is None:
            self._stats.skips_no_data += 1
            return None
        fl_mid = (fl_ticker.bid_price + fl_ticker.ask_price) * 0.5
        if fl_mid <= 0:
            return None
        qty = Decimal(str(self._p.notional_usd / fl_mid))
        if qty <= 0:
            return None

        # basis 가 높으면 SELL (flipster overpriced), 낮으면 BUY
        if z > 0:
            side = OrderSide.SELL
            # SELL LIMIT IOC at bid (즉시 크로스)
            price = Decimal(str(fl_ticker.bid_price))
            entry_price = fl_ticker.bid_price
        else:
            side = OrderSide.BUY
            price = Decimal(str(fl_ticker.ask_price))
            entry_price = fl_ticker.ask_price

        self._active[fl_sym] = _ActiveTrade(
            canonical=canonical,
            fl_sym=fl_sym,
            side=side,
            entry_ns=now_ns,
            entry_price=entry_price,
            qty=qty,
        )
        self._stats.entries += 1
        logger.info(
            "basis_meanrev_entry",
            canonical=canonical,
            side=side.value,
            z=round(z, 3),
            basis_now=round(basis_now, 6),
            basis_mean=round(mean, 6),
            basis_std=round(std, 6),
            entry_price=entry_price,
            qty=str(qty),
        )
        return OrderRequest(
            symbol=fl_sym,
            side=side,
            order_type=OrderType.LIMIT,
            quantity=qty,
            price=price,
            time_in_force=TimeInForce.IOC,
        )

    def _maybe_exit(
        self,
        active: _ActiveTrade,
        now_ns: int,
        latest: LatestTickerCache,
        history: SnapshotHistory,
    ) -> OrderRequest | None:
        fl_sym = active.fl_sym
        canonical = active.canonical
        bn_sym = to_exchange_symbol(EXCHANGE_BINANCE, canonical)

        diff = history.cross_mid_diff_array(
            EXCHANGE_FLIPSTER, fl_sym,
            EXCHANGE_BINANCE, bn_sym,
            duration_sec=self._p.window_sec,
        )
        if diff.size < 10:
            return None
        mean = float(diff.mean())
        std = float(diff.std())
        basis_now = float(diff[-1])
        z = (basis_now - mean) / std if std > self._p.min_std else 0.0

        hold_ns = now_ns - active.entry_ns
        timeout_hit = hold_ns >= int(self._p.timeout_sec * 1e9)
        target_hit = abs(z) <= self._p.k_out
        stop_hit = abs(z) >= self._p.k_stop

        if not (target_hit or stop_hit or timeout_hit):
            return None

        # 청산 방향은 진입 반대
        exit_side = OrderSide.BUY if active.side == OrderSide.SELL else OrderSide.SELL
        fl_ticker = latest.get(EXCHANGE_FLIPSTER, fl_sym)
        if fl_ticker is None:
            return None
        exit_price = fl_ticker.ask_price if exit_side == OrderSide.BUY else fl_ticker.bid_price
        if exit_price <= 0:
            return None

        reason = "target" if target_hit else ("stop" if stop_hit else "timeout")
        if target_hit:
            self._stats.exits_target += 1
        elif stop_hit:
            self._stats.exits_stop += 1
        else:
            self._stats.exits_timeout += 1

        logger.info(
            "basis_meanrev_exit",
            canonical=canonical,
            side=exit_side.value,
            reason=reason,
            z=round(z, 3),
            hold_ms=hold_ns // 1_000_000,
            entry_price=active.entry_price,
            exit_price=exit_price,
            qty=str(active.qty),
        )
        # 활성 제거
        self._active.pop(fl_sym, None)

        return OrderRequest(
            symbol=fl_sym,
            side=exit_side,
            order_type=OrderType.LIMIT,
            quantity=active.qty,
            price=Decimal(str(exit_price)),
            time_in_force=TimeInForce.IOC,
            reduce_only=True,
        )

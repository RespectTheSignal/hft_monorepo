"""Basis mean-reversion 전략 — Flipster 단방향, 신호 기반 포지션 전환.

시그널:
  z_t = (basis_t − mean(basis, window)) / std(basis, window)
  basis_t = fl_mid − bn_mid

포지션 상태(목표):
  z <= −k_in  → LONG   (Flipster 저평가 → 매수)
  z >=  k_in  → SHORT  (Flipster 고평가 → 매도)
  |z| <= k_out → FLAT  (평균 근처 → 청산)
  그 외      → 이전 상태 유지 (hysteresis)

필터 (진입/전환 시 적용):
  std / price >= min_std_bps   (drift 방어)
  |basis − mean| / price >= min_dev_bps  (경제적 의미 있는 이탈)
  필터 실패 시 상태 변경 안 함 (현 포지션 유지).
  단, 이미 FLAT 이던 심볼은 그대로 FLAT.

포지션 전환:
  target_qty 계산 후 UserState 의 현재 qty 와 delta 산출.
  |delta| > 0 이면 단일 LIMIT IOC 로 반대편 호가 주문 (flip 이면 2 × unit).
  손절 없음. 타임아웃 없음. 신호가 바뀔 때만 전환.

재전송(miss):
  체결 실패 후 cooldown_ms 경과 시 재평가해 필요하면 재전송.

Rust:
  struct BasisMeanRev { params, state: HashMap<Symbol, PositionIntent> }
"""

from __future__ import annotations

from dataclasses import dataclass, field
from decimal import Decimal
from enum import Enum

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


class Intent(Enum):
    FLAT = 0
    LONG = 1
    SHORT = -1


@dataclass(frozen=True, slots=True)
class BasisMeanRevParams:
    """전략 파라미터 (open/close 분리 구조).

    Threshold (독립):
      open_k  : |z| >= open_k  → LONG 또는 SHORT intent (진입)
      close_k : |z| <= close_k → FLAT intent (청산)
      그 외   : 현 intent 유지 (히스테리시스)

    사이즈 (독립):
      max_position_size   : 심볼당 순포지션 한도 notional (LONG/SHORT 목표)
      open_order_size     : 진입/포지션 확장 시 1주문 최대 notional
      close_order_size    : 청산/포지션 축소 시 1주문 최대 notional
      portfolio_max_size  : 전체 포트폴리오 |sum(notional)| 한도 (0 이면 무제한)

    동작:
      - 같은 방향 시그널 지속 시 open_order_size 단위로 max_position_size 까지 점진 빌드
      - Flip (LONG↔SHORT) 시 첫 chunk 는 close_order_size (줄이기), 이후 open_order_size (확장)
      - Portfolio cap 초과 시 open 주문 축소/skip, close 주문은 제약 없음
    """

    canonicals: tuple[str, ...]             # 거래 대상
    window_sec: float = 30.0                # z-score 계산 윈도우

    # Threshold (독립)
    open_k: float = 2.0                     # 진입 z-score
    close_k: float = 0.5                    # 청산 z-score

    # 사이즈 (독립)
    max_position_size: float = 20.0         # 심볼당 포지션 한도 notional
    open_order_size: float = 20.0           # 진입/확장 1주문 상한
    close_order_size: float = 20.0          # 청산/축소 1주문 상한
    portfolio_max_size: float = 0.0         # 전체 포트폴리오 한도 (0=무제한)

    # 필터
    min_dev_bps: float = 3.0                # |dev|/price 하한 (bp)
    min_std_bps: float = 0.5                # std/price 하한 (bp), drift 방어

    # 기타
    warmup_samples: int = 200               # 최소 샘플
    cooldown_ms: int = 500                  # miss 재전송 cooldown
    qty_epsilon: float = 1e-9               # delta 무시 임계


@dataclass
class _SymbolState:
    """심볼별 목표 상태 + 최근 전송 정보.

    target_qty 는 intent 전환 시점에 한 번 계산 후 고정. 이후 intent 유지
    중에는 가격 변동으로 흔들리지 않음 (미세 delta 재전송 방지).

    actual_at_last_emit 은 직전 주문 제출 시점의 실제 포지션. 이후 actual
    이 변했으면 해당 fill 이 반영된 것 → 즉시 다음 chunk 허용. 변화 없으면
    miss 가능성 → cooldown 적용.
    """

    intent: Intent = Intent.FLAT
    target_qty: float = 0.0                  # 현재 intent 목표 (고정)
    last_emit_ns: int = 0
    actual_at_last_emit: float = 0.0


@dataclass
class BasisMeanRevStats:
    """실행 통계"""

    signals_seen: int = 0
    intent_changes: int = 0
    longs: int = 0
    shorts: int = 0
    flats: int = 0
    orders_emitted: int = 0
    open_orders: int = 0
    close_orders: int = 0
    reemits: int = 0
    builds: int = 0                        # intent 유지 중 추가 빌드
    skips_no_data: int = 0
    skips_low_std: int = 0
    skips_below_z: int = 0
    skips_low_dev_bps: int = 0
    skips_cooldown: int = 0
    skips_portfolio_cap: int = 0
    holds_hysteresis: int = 0


class BasisMeanRevStrategy:
    """Flipster 단방향 basis mean-reversion — position-target 모델"""

    def __init__(
        self,
        params: BasisMeanRevParams,
        clock: Clock | None = None,
    ) -> None:
        self._p: BasisMeanRevParams = params
        self._clock: Clock = clock if clock is not None else LiveClock()
        self._state: dict[str, _SymbolState] = {}
        self._stats: BasisMeanRevStats = BasisMeanRevStats()

    @property
    def stats(self) -> BasisMeanRevStats:
        return self._stats

    @property
    def symbol_states(self) -> dict[str, _SymbolState]:
        return self._state

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
            open_k=self._p.open_k,
            close_k=self._p.close_k,
            max_position_size=self._p.max_position_size,
            open_order_size=self._p.open_order_size,
            close_order_size=self._p.close_order_size,
            portfolio_max_size=self._p.portfolio_max_size,
            min_dev_bps=self._p.min_dev_bps,
            min_std_bps=self._p.min_std_bps,
        )

    async def on_stop(self) -> None:
        s = self._stats
        logger.info(
            "basis_meanrev_stopped",
            signals=s.signals_seen,
            intent_changes=s.intent_changes,
            longs=s.longs,
            shorts=s.shorts,
            flats=s.flats,
            orders=s.orders_emitted,
            open_orders=s.open_orders,
            close_orders=s.close_orders,
            reemits=s.reemits,
            builds=s.builds,
            port_cap_hits=s.skips_portfolio_cap,
        )

    async def on_book_ticker(
        self,
        ticker: BookTicker,
        state: UserState,
        latest: LatestTickerCache,
        market_stats: MarketStatsCache,
        history: SnapshotHistory,
    ) -> list[OrderRequest]:
        return self._evaluate_all(state, latest, history)

    async def on_timer(
        self,
        state: UserState,
        latest: LatestTickerCache,
        market_stats: MarketStatsCache,
        history: SnapshotHistory,
    ) -> list[OrderRequest]:
        return self._evaluate_all(state, latest, history)

    # ── 내부 로직 ──

    def _evaluate_all(
        self,
        user_state: UserState,
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
                canonical, fl_sym, bn_sym, now_ns, user_state, latest, history,
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
        user_state: UserState,
        latest: LatestTickerCache,
        history: SnapshotHistory,
    ) -> OrderRequest | None:
        sym_state = self._state.get(fl_sym)
        if sym_state is None:
            sym_state = _SymbolState()
            self._state[fl_sym] = sym_state

        # 1. Basis 시계열
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
        basis_now = float(diff[-1])

        # 2. 현재 호가 (가격 기준으로 bp 계산 + 주문 가격)
        fl_ticker = latest.get(EXCHANGE_FLIPSTER, fl_sym)
        if fl_ticker is None:
            self._stats.skips_no_data += 1
            return None
        fl_mid = (fl_ticker.bid_price + fl_ticker.ask_price) * 0.5
        if fl_mid <= 0:
            self._stats.skips_no_data += 1
            return None

        # 3. 목표 intent 결정
        std_bps = (std / fl_mid) * 10000.0
        dev_bps = (abs(basis_now - mean) / fl_mid) * 10000.0
        filters_ok = (
            std_bps >= self._p.min_std_bps
            and dev_bps >= self._p.min_dev_bps
        )

        z = (basis_now - mean) / std if std > 1e-12 else 0.0
        new_intent = self._next_intent(sym_state.intent, z, filters_ok)

        intent_changed = new_intent != sym_state.intent

        if intent_changed:
            self._stats.intent_changes += 1
            if new_intent == Intent.LONG:
                self._stats.longs += 1
            elif new_intent == Intent.SHORT:
                self._stats.shorts += 1
            else:
                self._stats.flats += 1
            sym_state.target_qty = self._target_qty(new_intent, fl_mid)
            sym_state.intent = new_intent

        target_qty = sym_state.target_qty
        actual_qty = self._actual_qty(user_state, fl_sym)
        delta = target_qty - actual_qty

        # 5. delta 무시할 수준이면 skip
        if abs(delta) <= self._p.qty_epsilon:
            return None

        # 6. Mode 판별 — delta 가 actual 과 반대부호이거나 target 이 0 이면 CLOSE
        #    (포지션을 줄이는 방향). 그 외엔 OPEN (포지션을 확장/개시).
        if abs(actual_qty) <= self._p.qty_epsilon:
            mode_open = True  # 0 → 어느 방향 → OPEN
        elif actual_qty * delta < 0:
            mode_open = False  # 반대 방향 → CLOSE (포지션 축소/flip 초입)
        else:
            mode_open = True  # 동일 방향 확장 → OPEN (build)

        # 7. Cooldown — intent 변경 / fill 진행 시 즉시 허용
        cooldown_ns = self._p.cooldown_ms * 1_000_000
        fill_progressed = abs(actual_qty - sym_state.actual_at_last_emit) > self._p.qty_epsilon
        if (
            not intent_changed
            and not fill_progressed
            and (now_ns - sym_state.last_emit_ns) < cooldown_ns
        ):
            self._stats.skips_cooldown += 1
            return None

        # 8. Mode 별 order_size 상한
        order_size_notional = (
            self._p.open_order_size if mode_open else self._p.close_order_size
        )
        max_order_qty = order_size_notional / fl_mid if fl_mid > 0 else 0.0
        if max_order_qty <= 0:
            return None
        order_qty = min(abs(delta), max_order_qty)
        if order_qty <= self._p.qty_epsilon:
            return None

        # 9. Portfolio cap (OPEN 만 대상, CLOSE 는 노출 줄이므로 제약 없음)
        if mode_open and self._p.portfolio_max_size > 0:
            portfolio_notional = self._portfolio_notional(user_state, latest)
            remaining = self._p.portfolio_max_size - portfolio_notional
            if remaining <= 0:
                self._stats.skips_portfolio_cap += 1
                return None
            capped_notional = min(order_qty * fl_mid, remaining)
            order_qty = capped_notional / fl_mid
            if order_qty <= self._p.qty_epsilon:
                self._stats.skips_portfolio_cap += 1
                return None

        # 10. 주문 생성
        side = OrderSide.BUY if delta > 0 else OrderSide.SELL
        qty = Decimal(str(order_qty))
        price = fl_ticker.ask_price if side == OrderSide.BUY else fl_ticker.bid_price
        if price <= 0:
            return None

        sym_state.last_emit_ns = now_ns
        sym_state.actual_at_last_emit = actual_qty
        self._stats.orders_emitted += 1
        if mode_open:
            self._stats.open_orders += 1
        else:
            self._stats.close_orders += 1
        if not intent_changed and abs(actual_qty) > self._p.qty_epsilon and mode_open:
            self._stats.builds += 1
        if not intent_changed:
            self._stats.reemits += 1

        partial = order_qty < abs(delta) - self._p.qty_epsilon

        logger.info(
            "basis_meanrev_transition",
            canonical=canonical,
            intent=new_intent.name,
            mode="open" if mode_open else "close",
            side=side.value,
            order_qty=round(order_qty, 8),
            delta=round(delta, 8),
            target=round(target_qty, 8),
            actual=round(actual_qty, 8),
            partial=partial,
            z=round(z, 3),
            dev_bps=round(dev_bps, 2),
            std_bps=round(std_bps, 2),
            price=price,
        )

        return OrderRequest(
            symbol=fl_sym,
            side=side,
            order_type=OrderType.LIMIT,
            quantity=qty,
            price=Decimal(str(price)),
            time_in_force=TimeInForce.IOC,
        )

    def _portfolio_notional(
        self,
        user_state: UserState,
        latest: LatestTickerCache,
    ) -> float:
        """현재 보유 포지션의 총 notional (절대값) 합산"""
        total = 0.0
        for sym, pos in user_state.positions.items():
            ticker = latest.get(EXCHANGE_FLIPSTER, sym)
            if ticker is None:
                continue
            mid = (ticker.bid_price + ticker.ask_price) * 0.5
            if mid <= 0:
                continue
            total += abs(float(pos.position_amount)) * mid
        return total

    # ── 헬퍼 ──

    def _next_intent(
        self,
        current: Intent,
        z: float,
        filters_ok: bool,
    ) -> Intent:
        """z 와 필터로부터 다음 목표 포지션 결정.

        필터 실패: 현 상태 유지.
        """
        if not filters_ok:
            self._stats.holds_hysteresis += 1
            return current

        if z <= -self._p.open_k:
            return Intent.LONG
        if z >= self._p.open_k:
            return Intent.SHORT
        if abs(z) <= self._p.close_k:
            return Intent.FLAT

        self._stats.holds_hysteresis += 1
        return current

    def _target_qty(self, intent: Intent, fl_mid: float) -> float:
        if intent == Intent.FLAT or fl_mid <= 0:
            return 0.0
        unit = self._p.max_position_size / fl_mid
        return unit if intent == Intent.LONG else -unit

    @staticmethod
    def _actual_qty(user_state: UserState, fl_sym: str) -> float:
        pos = user_state.get_position(fl_sym)
        if pos is None:
            return 0.0
        return float(pos.position_amount)

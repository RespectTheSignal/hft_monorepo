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
    execution_exchange: str = EXCHANGE_FLIPSTER
    signal_mode: str = "basis"              # basis | leadlag
    signal_horizon_sec: float = 1.0         # lead-lag return horizon
    window_sec: float = 30.0                # z-score 계산 윈도우

    # Threshold (독립)
    open_k: float = 2.0                     # 진입 z-score
    close_k: float = 0.5                    # 청산 z-score

    # 사이즈 (독립)
    max_position_size: float = 100.0         # 심볼당 포지션 한도 notional
    open_order_size: float = 20.0           # 진입/확장 1주문 상한
    close_order_size: float = 20.0          # 청산/축소 1주문 상한
    portfolio_max_size: float = 0.0         # 전체 포트폴리오 한도 (0=무제한)

    # 필터 — open (진입/확장) 과 close (청산) 분리
    min_open_dev_bps: float = 3.0           # 진입 시 |dev|/price 하한 (bp)
    min_open_std_bps: float = 0.5           # 진입 시 std/price 하한 (drift 방어)
    min_close_dev_bps: float = 1.0          # 청산 시 |dev|/price 하한 (noise 방어)
    min_close_std_bps: float = 0.0          # 청산 시 std/price 하한

    # 스프레드 인식 진입 필터
    spread_aware_filter: bool = True        # 스프레드 + 수수료 대비 edge 확인
    beta_fl_assumption: float = 0.5         # Flipster 기여 가정 (0..1)
    fee_bps_cost: float = 0.45              # per side (Flipster taker)
    spread_edge_safety: float = 1.0         # 안전 배율 (1.0 = 손익분기)

    # Binance 가격 변화 후 cooldown — 방금 바뀐 구간에선 새 주문 skip.
    # 목적: 순수 PnL 최적화가 아니라 stealth — 너무 노골적으로 Binance lead 를
    # 따라가면 Flipster 가 arb 로 판단해 API 차단할 위험. open 에만 기본 200ms.
    # 0 으로 설정하면 최대 PnL 모드 (감지 위험 상승).
    binance_open_cooldown_ms: int = 0
    binance_close_cooldown_ms: int = 0

    # 기타
    warmup_samples: int = 200               # 최소 샘플
    cooldown_ms: int = 500                  # miss 재전송 cooldown
    allow_same_intent_reemit: bool = True   # fill 진전 없으면 같은 intent 재전송 허용 여부
    same_intent_rearm_price_bps: float = 0.0  # 같은 intent 재시도 최소 가격 개선 폭
    same_intent_rearm_z_delta: float = 0.0    # 같은 intent 재시도 최소 |z| 강화 폭
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
    last_emit_price: float = 0.0
    last_emit_abs_z: float = 0.0


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
    skips_spread_cost: int = 0             # spread + fee > edge 로 차단
    skips_binance_cooldown: int = 0        # Binance 가격 변화 직후 cooldown
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
        # canonical → (last_bid, last_ask), last 변경 ns
        self._bn_last_quote: dict[str, tuple[float, float]] = {}
        self._bn_last_change_ns: dict[str, int] = {}

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
            execution_exchange=self._p.execution_exchange,
            signal_mode=self._p.signal_mode,
            signal_horizon_sec=self._p.signal_horizon_sec,
            window_sec=self._p.window_sec,
            open_k=self._p.open_k,
            close_k=self._p.close_k,
            max_position_size=self._p.max_position_size,
            open_order_size=self._p.open_order_size,
            close_order_size=self._p.close_order_size,
            portfolio_max_size=self._p.portfolio_max_size,
            min_open_dev_bps=self._p.min_open_dev_bps,
            min_open_std_bps=self._p.min_open_std_bps,
            min_close_dev_bps=self._p.min_close_dev_bps,
            min_close_std_bps=self._p.min_close_std_bps,
            allow_same_intent_reemit=self._p.allow_same_intent_reemit,
            same_intent_rearm_price_bps=self._p.same_intent_rearm_price_bps,
            same_intent_rearm_z_delta=self._p.same_intent_rearm_z_delta,
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
            exec_sym = to_exchange_symbol(self._p.execution_exchange, canonical)
            bn_sym = to_exchange_symbol(EXCHANGE_BINANCE, canonical)
            self._stats.signals_seen += 1
            order = self._evaluate_symbol(
                canonical, exec_sym, bn_sym, now_ns, user_state, latest, history,
            )
            if order is not None:
                orders.append(order)
        return orders

    def _evaluate_symbol(
        self,
        canonical: str,
        exec_sym: str,
        bn_sym: str,
        now_ns: int,
        user_state: UserState,
        latest: LatestTickerCache,
        history: SnapshotHistory,
    ) -> OrderRequest | None:
        sym_state = self._state.get(exec_sym)
        if sym_state is None:
            sym_state = _SymbolState()
            self._state[exec_sym] = sym_state

        # 1. 현재 호가 (가격 기준으로 bp 계산 + 주문 가격)
        exec_ticker = latest.get(self._p.execution_exchange, exec_sym)
        if exec_ticker is None:
            self._stats.skips_no_data += 1
            return None
        exec_mid = (exec_ticker.bid_price + exec_ticker.ask_price) * 0.5
        if exec_mid <= 0:
            self._stats.skips_no_data += 1
            return None

        # 2. Signal 시계열
        z, dev_bps, std_bps = self._signal_inputs(history, exec_sym, bn_sym, exec_mid)
        if z is None or dev_bps is None or std_bps is None:
            self._stats.skips_no_data += 1
            return None

        # 3. Binance 최신 호가 확인 → 변화 감지 (cooldown 용)
        bn_ticker = latest.get(EXCHANGE_BINANCE, bn_sym)
        if bn_ticker is not None:
            cur_quote = (bn_ticker.bid_price, bn_ticker.ask_price)
            prev_quote = self._bn_last_quote.get(canonical)
            if prev_quote is None or prev_quote != cur_quote:
                self._bn_last_quote[canonical] = cur_quote
                self._bn_last_change_ns[canonical] = now_ns

        # 4. 목표 intent 결정 — z/필터/현재 intent 에 따라 결정
        exec_spread_bps = (
            (exec_ticker.ask_price - exec_ticker.bid_price) / exec_mid * 10000.0
            if exec_mid > 0 else 0.0
        )

        # Binance cooldown 체크 — open/close 별도
        bn_open_cooldown = False
        bn_close_cooldown = False
        last_chg = self._bn_last_change_ns.get(canonical, 0)
        if last_chg > 0:
            elapsed_ns = now_ns - last_chg
            if self._p.binance_open_cooldown_ms > 0 and elapsed_ns < self._p.binance_open_cooldown_ms * 1_000_000:
                bn_open_cooldown = True
            if self._p.binance_close_cooldown_ms > 0 and elapsed_ns < self._p.binance_close_cooldown_ms * 1_000_000:
                bn_close_cooldown = True

        new_intent = self._next_intent(
            sym_state.intent, z, dev_bps, std_bps, exec_spread_bps,
            bn_open_cooldown=bn_open_cooldown,
            bn_close_cooldown=bn_close_cooldown,
        )

        intent_changed = new_intent != sym_state.intent

        if intent_changed:
            self._stats.intent_changes += 1
            if new_intent == Intent.LONG:
                self._stats.longs += 1
            elif new_intent == Intent.SHORT:
                self._stats.shorts += 1
            else:
                self._stats.flats += 1
            sym_state.target_qty = self._target_qty(new_intent, exec_mid)
            sym_state.intent = new_intent

        target_qty = sym_state.target_qty
        actual_qty = self._actual_qty(user_state, exec_sym)
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
        side = OrderSide.BUY if delta > 0 else OrderSide.SELL
        current_price = exec_ticker.ask_price if side == OrderSide.BUY else exec_ticker.bid_price
        if current_price <= 0:
            return None
        if (
            not intent_changed
            and not fill_progressed
            and not self._p.allow_same_intent_reemit
        ):
            if not self._same_intent_rearm_ready(sym_state, side, current_price, z):
                return None
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
        max_order_qty = order_size_notional / exec_mid if exec_mid > 0 else 0.0
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
            capped_notional = min(order_qty * exec_mid, remaining)
            order_qty = capped_notional / exec_mid
            if order_qty <= self._p.qty_epsilon:
                self._stats.skips_portfolio_cap += 1
                return None

        # 10. 주문 생성
        qty = Decimal(str(order_qty))
        price = current_price

        sym_state.last_emit_ns = now_ns
        sym_state.actual_at_last_emit = actual_qty
        sym_state.last_emit_price = price
        sym_state.last_emit_abs_z = abs(z)
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
            symbol=exec_sym,
            side=side,
            order_type=OrderType.LIMIT,
            quantity=qty,
            price=Decimal(str(price)),
            time_in_force=TimeInForce.IOC,
        )

    def _same_intent_rearm_ready(
        self,
        sym_state: _SymbolState,
        side: OrderSide,
        current_price: float,
        z: float,
    ) -> bool:
        if sym_state.last_emit_ns <= 0:
            return True

        price_improved_bps = 0.0
        if sym_state.last_emit_price > 0:
            if side == OrderSide.BUY:
                price_improved_bps = max(
                    0.0,
                    (sym_state.last_emit_price - current_price) / sym_state.last_emit_price * 10000.0,
                )
            else:
                price_improved_bps = max(
                    0.0,
                    (current_price - sym_state.last_emit_price) / sym_state.last_emit_price * 10000.0,
                )

        z_improved = abs(z) - sym_state.last_emit_abs_z
        return (
            price_improved_bps >= self._p.same_intent_rearm_price_bps
            or z_improved >= self._p.same_intent_rearm_z_delta
        )

    def _portfolio_notional(
        self,
        user_state: UserState,
        latest: LatestTickerCache,
    ) -> float:
        """현재 보유 포지션의 총 notional (절대값) 합산"""
        total = 0.0
        for sym, pos in user_state.positions.items():
            ticker = latest.get(self._p.execution_exchange, sym)
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
        dev_bps: float,
        std_bps: float,
        exec_spread_bps: float,
        bn_open_cooldown: bool = False,
        bn_close_cooldown: bool = False,
    ) -> Intent:
        """z / 필터 / 현재 intent 에 따라 다음 목표 포지션 결정.

        후보 intent 를 z 로 결정 후, 전환 방향에 따라 필터 적용:
          → LONG/SHORT (open) : open filter + spread-aware cost filter + open bn cooldown
          → FLAT (close)      : close filter + close bn cooldown
        필터 실패 시 현 intent 유지.
        """
        # 후보 intent 결정
        if z <= -self._p.open_k:
            candidate = Intent.LONG
        elif z >= self._p.open_k:
            candidate = Intent.SHORT
        elif abs(z) <= self._p.close_k:
            candidate = Intent.FLAT
        else:
            self._stats.holds_hysteresis += 1
            return current

        if candidate == current:
            return current

        # 방향별 필터
        if candidate == Intent.FLAT:
            if std_bps < self._p.min_close_std_bps:
                self._stats.skips_low_std += 1
                return current
            if dev_bps < self._p.min_close_dev_bps:
                self._stats.skips_low_dev_bps += 1
                return current
            if bn_close_cooldown:
                self._stats.skips_binance_cooldown += 1
                return current
            return candidate

        # 진입 또는 flip — open 필터
        if std_bps < self._p.min_open_std_bps:
            self._stats.skips_low_std += 1
            return current
        if dev_bps < self._p.min_open_dev_bps:
            self._stats.skips_low_dev_bps += 1
            return current

        # 스프레드 인식 비용 필터
        if self._p.spread_aware_filter:
            expected_edge_bps = dev_bps * self._p.beta_fl_assumption
            required_cost_bps = exec_spread_bps + 2.0 * self._p.fee_bps_cost
            if expected_edge_bps < required_cost_bps * self._p.spread_edge_safety:
                self._stats.skips_spread_cost += 1
                return current

        # Binance 가격 변화 직후 cooldown (open)
        if bn_open_cooldown:
            self._stats.skips_binance_cooldown += 1
            return current

        return candidate

    def _target_qty(self, intent: Intent, exec_mid: float) -> float:
        if intent == Intent.FLAT or exec_mid <= 0:
            return 0.0
        unit = self._p.max_position_size / exec_mid
        return unit if intent == Intent.LONG else -unit

    def _signal_inputs(
        self,
        history: SnapshotHistory,
        exec_sym: str,
        bn_sym: str,
        exec_mid: float,
    ) -> tuple[float | None, float | None, float | None]:
        if self._p.signal_mode == "leadlag":
            return self._leadlag_signal_inputs(history, exec_sym, bn_sym)
        return self._basis_signal_inputs(history, exec_sym, bn_sym, exec_mid)

    def _basis_signal_inputs(
        self,
        history: SnapshotHistory,
        exec_sym: str,
        bn_sym: str,
        exec_mid: float,
    ) -> tuple[float | None, float | None, float | None]:
        diff = history.cross_mid_diff_array(
            self._p.execution_exchange, exec_sym,
            EXCHANGE_BINANCE, bn_sym,
            duration_sec=self._p.window_sec,
        )
        if diff.size < self._p.warmup_samples:
            return None, None, None
        mean = float(diff.mean())
        std = float(diff.std())
        basis_now = float(diff[-1])
        std_bps = (std / exec_mid) * 10000.0
        dev_bps = (abs(basis_now - mean) / exec_mid) * 10000.0
        z = (basis_now - mean) / std if std > 1e-12 else 0.0
        return z, dev_bps, std_bps

    def _leadlag_signal_inputs(
        self,
        history: SnapshotHistory,
        exec_sym: str,
        bn_sym: str,
    ) -> tuple[float | None, float | None, float | None]:
        exec_mid = history.mid_array(self._p.execution_exchange, exec_sym, self._p.window_sec)
        bn_mid = history.mid_array(EXCHANGE_BINANCE, bn_sym, self._p.window_sec)
        n = min(exec_mid.size, bn_mid.size)
        if n == 0:
            return None, None, None
        exec_mid = exec_mid[-n:]
        bn_mid = bn_mid[-n:]
        lag_steps = max(1, int(self._p.signal_horizon_sec / 0.05))
        needed = max(self._p.warmup_samples, lag_steps + 2)
        if n < needed:
            return None, None, None
        exec_ret_bps = (exec_mid[lag_steps:] / exec_mid[:-lag_steps] - 1.0) * 10000.0
        bn_ret_bps = (bn_mid[lag_steps:] / bn_mid[:-lag_steps] - 1.0) * 10000.0
        residual_bps = bn_ret_bps - exec_ret_bps
        mean = float(residual_bps.mean())
        std = float(residual_bps.std())
        now = float(residual_bps[-1])
        raw_z = (now - mean) / std if std > 1e-12 else 0.0
        # Binance가 execution venue보다 먼저 움직였으면 Gate catch-up 방향으로 진입.
        return -raw_z, abs(now - mean), std

    @staticmethod
    def _actual_qty(user_state: UserState, exec_sym: str) -> float:
        pos = user_state.get_position(exec_sym)
        if pos is None:
            return 0.0
        return float(pos.position_amount)

"""체결 시뮬레이터 — 100ms 지연 + LIMIT IOC 가정.

주문 제출 시점 t 에 보류, 시뮬 시각이 t + 100ms 에 도달하면 그 순간의
LatestTickerCache 에서 해당 심볼의 반대편 호가를 확인:
  BUY  LIMIT IOC @ limit_price → ask_100ms ≤ limit_price 이면 ask_100ms 에 체결
  SELL LIMIT IOC @ limit_price → bid_100ms ≥ limit_price 이면 bid_100ms 에 체결
체결 실패(miss) 는 별도 카운트.

체결 시 UserState 의 포지션/잔고를 직접 업데이트하고 PnLTracker 에 기록.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from decimal import Decimal

import structlog

from strategy_flipster.backtest.pnl_tracker import PnlTracker
from strategy_flipster.market_data.latest_cache import LatestTickerCache
from strategy_flipster.market_data.symbol import EXCHANGE_FLIPSTER
from strategy_flipster.types import (
    OrderRequest,
    OrderSide,
    Position,
    PositionSide,
    MarginType,
)
from strategy_flipster.user_data.state import UserState

logger = structlog.get_logger(__name__)


@dataclass
class _Pending:
    request: OrderRequest
    submit_ns: int
    fill_at_ns: int


@dataclass
class FillSimulatorStats:
    submitted: int = 0
    filled: int = 0
    missed: int = 0
    filled_notional_usd: float = 0.0
    missed_notional_usd: float = 0.0
    fill_price_improve_usd: float = 0.0
    fill_price_improve_bps_x_notional: float = 0.0
    miss_adverse_bps_x_notional: float = 0.0


class FillSimulator:
    """LIMIT IOC + 100ms 지연 체결 시뮬레이터"""

    def __init__(
        self,
        user_state: UserState,
        pnl: PnlTracker,
        execution_exchange: str = EXCHANGE_FLIPSTER,
        fee_bps: float = 0.45,
        latency_ns: int = 100_000_000,  # 100ms
    ) -> None:
        self._user_state: UserState = user_state
        self._pnl: PnlTracker = pnl
        self._execution_exchange: str = execution_exchange
        self._fee_bps: float = fee_bps
        self._latency_ns: int = latency_ns
        self._pending: list[_Pending] = []
        self._stats: FillSimulatorStats = FillSimulatorStats()

    @property
    def stats(self) -> FillSimulatorStats:
        return self._stats

    def submit(self, orders: list[OrderRequest], submit_ns: int) -> None:
        for o in orders:
            self._pending.append(_Pending(
                request=o,
                submit_ns=submit_ns,
                fill_at_ns=submit_ns + self._latency_ns,
            ))
            self._stats.submitted += 1

    def process(self, now_ns: int, latest: LatestTickerCache) -> None:
        """now_ns 시점까지 도달한 pending 주문을 체결 시도.

        처리한 항목은 리스트에서 제거. 미도달 건은 유지.
        """
        if not self._pending:
            return
        remaining: list[_Pending] = []
        for p in self._pending:
            if p.fill_at_ns > now_ns:
                remaining.append(p)
                continue
            self._try_fill(p, now_ns, latest)
        self._pending = remaining

    def _try_fill(
        self,
        pending: _Pending,
        now_ns: int,
        latest: LatestTickerCache,
    ) -> None:
        req = pending.request
        ticker = latest.get(self._execution_exchange, req.symbol)
        if ticker is None:
            self._stats.missed += 1
            return
        limit_price = float(req.price) if req.price is not None else 0.0
        qty = req.quantity if req.quantity is not None else Decimal("0")
        qty_f = float(qty)
        req_notional = qty_f * limit_price
        if qty <= 0 or limit_price <= 0:
            self._stats.missed += 1
            return

        if req.side == OrderSide.BUY:
            # LIMIT BUY → ask 가 limit_price 이하면 체결
            if ticker.ask_price <= limit_price and ticker.ask_price > 0:
                fill_price = ticker.ask_price
            else:
                self._stats.missed += 1
                self._stats.missed_notional_usd += req_notional
                if ticker.ask_price > 0:
                    adverse_bps = max(0.0, (ticker.ask_price - limit_price) / limit_price * 10000.0)
                    self._stats.miss_adverse_bps_x_notional += adverse_bps * req_notional
                return
        else:  # SELL
            if ticker.bid_price >= limit_price and ticker.bid_price > 0:
                fill_price = ticker.bid_price
            else:
                self._stats.missed += 1
                self._stats.missed_notional_usd += req_notional
                if ticker.bid_price > 0:
                    adverse_bps = max(0.0, (limit_price - ticker.bid_price) / limit_price * 10000.0)
                    self._stats.miss_adverse_bps_x_notional += adverse_bps * req_notional
                return

        # 체결 처리
        self._stats.filled += 1
        fill_notional = qty_f * fill_price
        self._stats.filled_notional_usd += fill_notional
        fee = qty_f * fill_price * self._fee_bps * 1e-4
        improve_usd = max(0.0, (limit_price - fill_price) * qty_f) if req.side == OrderSide.BUY else max(0.0, (fill_price - limit_price) * qty_f)
        improve_bps = (improve_usd / req_notional * 10000.0) if req_notional > 0 else 0.0
        self._stats.fill_price_improve_usd += improve_usd
        self._stats.fill_price_improve_bps_x_notional += improve_bps * fill_notional
        self._pnl.on_fill(
            symbol=req.symbol,
            side=req.side,
            qty=qty_f,
            price=fill_price,
            fee=fee,
            ts_ns=now_ns,
        )
        # UserState 포지션 갱신 (simple net position)
        self._update_position(req.symbol, req.side, qty_f, fill_price, now_ns)

    def _update_position(
        self,
        symbol: str,
        side: OrderSide,
        qty: float,
        price: float,
        ts_ns: int,
    ) -> None:
        current = self._user_state.positions.get(symbol)
        cur_amount = float(current.position_amount) if current is not None else 0.0
        delta = qty if side == OrderSide.BUY else -qty
        new_amount = cur_amount + delta
        if abs(new_amount) < 1e-12:
            self._user_state.remove_position(symbol)
            return
        pos_side = PositionSide.LONG if new_amount > 0 else PositionSide.SHORT
        self._user_state.update_position(Position(
            symbol=symbol,
            leverage=1,
            margin_type=MarginType.CROSS,
            position_side=pos_side,
            position_amount=Decimal(str(new_amount)),
            entry_price=Decimal(str(price)),
            mark_price=Decimal(str(price)),
            unrealized_pnl=Decimal("0"),
        ))

"""주문 라이프사이클 관리 — Strategy → Execution 중간 계층"""

from __future__ import annotations

import time
from decimal import Decimal

import structlog

from strategy_flipster.backtest.pnl_tracker import PnlTracker
from strategy_flipster.error import AppException
from strategy_flipster.execution.rest_client import FlipsterExecutionClient
from strategy_flipster.market_data.latest_cache import LatestTickerCache
from strategy_flipster.market_data.symbol import EXCHANGE_FLIPSTER
from strategy_flipster.types import (
    MarginType,
    OrderRequest,
    OrderResponse,
    OrderSide,
    Position,
    PositionSide,
)
from strategy_flipster.user_data.state import UserState

logger = structlog.get_logger(__name__)


class OrderManager:
    """전략의 OrderRequest를 받아 실행.

    - dry_run 모드: 실제 제출 없이 가상 체결 (latest_cache 기반),
      UserState/PnLTracker 갱신해서 전략이 자신의 체결을 인지할 수 있게 함
    - 실매매 모드: Flipster REST 호출, 결과는 Flipster WS private 로 UserState 갱신
    """

    def __init__(
        self,
        client: FlipsterExecutionClient,
        state: UserState,
        dry_run: bool = False,
        dry_run_latest: LatestTickerCache | None = None,
        dry_run_pnl: PnlTracker | None = None,
        dry_run_fee_bps: float = 0.45,
    ) -> None:
        self._client: FlipsterExecutionClient = client
        self._state: UserState = state
        self._dry_run: bool = dry_run
        self._dry_run_latest: LatestTickerCache | None = dry_run_latest
        self._dry_run_pnl: PnlTracker | None = dry_run_pnl
        self._dry_run_fee_bps: float = dry_run_fee_bps
        self._total_submitted: int = 0
        self._total_errors: int = 0

    @property
    def total_submitted(self) -> int:
        return self._total_submitted

    @property
    def total_errors(self) -> int:
        return self._total_errors

    async def submit_order(self, request: OrderRequest) -> OrderResponse | None:
        """주문 제출. dry_run 이면 가상 체결 + UserState 갱신."""
        if self._dry_run:
            return self._simulate_fill(request)

        try:
            response = await self._client.place_order(request)
            self._total_submitted += 1
            return response
        except AppException as e:
            self._total_errors += 1
            logger.error(
                "order_submit_failed",
                symbol=request.symbol,
                side=request.side.value,
                error=e.error.message,
                status_code=e.error.status_code,
            )
            return None

    async def submit_orders(self, requests: list[OrderRequest]) -> list[OrderResponse | None]:
        """복수 주문 순차 제출"""
        results: list[OrderResponse | None] = []
        for req in requests:
            result = await self.submit_order(req)
            results.append(result)
        return results

    def _simulate_fill(self, request: OrderRequest) -> OrderResponse | None:
        """Dry-run 가상 체결. latest_cache 에서 반대편 호가 기준으로 즉시 체결,
        UserState 포지션과 PnlTracker 갱신. 체결가가 없으면 miss 로 기록."""
        self._total_submitted += 1
        ticker = None
        if self._dry_run_latest is not None:
            ticker = self._dry_run_latest.get(EXCHANGE_FLIPSTER, request.symbol)

        qty = float(request.quantity) if request.quantity is not None else 0.0
        limit_price = float(request.price) if request.price is not None else 0.0
        if ticker is None or qty <= 0 or limit_price <= 0:
            logger.info(
                "order_dry_run_miss",
                symbol=request.symbol, side=request.side.value,
                reason="no_ticker_or_bad_price",
            )
            return None

        if request.side == OrderSide.BUY:
            fill_price = ticker.ask_price
            if fill_price <= 0 or fill_price > limit_price:
                logger.info(
                    "order_dry_run_miss",
                    symbol=request.symbol, side="BUY",
                    ask=fill_price, limit=limit_price,
                )
                return None
        else:
            fill_price = ticker.bid_price
            if fill_price <= 0 or fill_price < limit_price:
                logger.info(
                    "order_dry_run_miss",
                    symbol=request.symbol, side="SELL",
                    bid=fill_price, limit=limit_price,
                )
                return None

        # 체결 처리 — UserState 갱신
        fee = qty * fill_price * self._dry_run_fee_bps * 1e-4
        self._apply_dry_run_fill(request.symbol, request.side, qty, fill_price)
        if self._dry_run_pnl is not None:
            self._dry_run_pnl.on_fill(
                symbol=request.symbol,
                side=request.side,
                qty=qty,
                price=fill_price,
                fee=fee,
                ts_ns=time.time_ns(),
            )
        logger.info(
            "order_dry_run_fill",
            symbol=request.symbol,
            side=request.side.value,
            qty=round(qty, 8),
            price=fill_price,
            fee=round(fee, 6),
        )
        return None

    def _apply_dry_run_fill(
        self,
        symbol: str,
        side: OrderSide,
        qty: float,
        price: float,
    ) -> None:
        current = self._state.positions.get(symbol)
        cur_amount = float(current.position_amount) if current is not None else 0.0
        delta = qty if side == OrderSide.BUY else -qty
        new_amount = cur_amount + delta
        if abs(new_amount) < 1e-12:
            self._state.remove_position(symbol)
            return
        pos_side = PositionSide.LONG if new_amount > 0 else PositionSide.SHORT
        self._state.update_position(Position(
            symbol=symbol,
            leverage=1,
            margin_type=MarginType.CROSS,
            position_side=pos_side,
            position_amount=Decimal(str(new_amount)),
            entry_price=Decimal(str(price)),
            mark_price=Decimal(str(price)),
            unrealized_pnl=Decimal("0"),
        ))

    async def cancel_all(self, symbol: str) -> int:
        """심볼의 모든 대기 주문 취소. 취소된 주문 수 반환."""
        try:
            pending = await self._client.get_pending_orders(symbol)
            cancelled = 0
            for order in pending:
                try:
                    await self._client.cancel_order(symbol, order.order_id)
                    cancelled += 1
                except AppException as e:
                    logger.warning(
                        "cancel_failed",
                        symbol=symbol,
                        order_id=order.order_id,
                        error=e.error.message,
                    )
            return cancelled
        except AppException as e:
            logger.error("cancel_all_failed", symbol=symbol, error=e.error.message)
            return 0

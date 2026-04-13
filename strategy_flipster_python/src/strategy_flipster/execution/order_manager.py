"""주문 라이프사이클 관리 — Strategy → Execution 중간 계층"""

from __future__ import annotations

import structlog

from strategy_flipster.error import AppException
from strategy_flipster.execution.rest_client import FlipsterExecutionClient
from strategy_flipster.types import OrderRequest, OrderResponse
from strategy_flipster.user_data.state import UserState

logger = structlog.get_logger(__name__)


class OrderManager:
    """전략의 OrderRequest를 받아 실행.

    - 사전 검증 (dry-run 모드 지원)
    - 주문 제출 + 에러 처리
    - 실행 로그
    """

    def __init__(
        self,
        client: FlipsterExecutionClient,
        state: UserState,
        dry_run: bool = False,
    ) -> None:
        self._client: FlipsterExecutionClient = client
        self._state: UserState = state
        self._dry_run: bool = dry_run
        self._total_submitted: int = 0
        self._total_errors: int = 0

    @property
    def total_submitted(self) -> int:
        return self._total_submitted

    @property
    def total_errors(self) -> int:
        return self._total_errors

    async def submit_order(self, request: OrderRequest) -> OrderResponse | None:
        """주문 제출. dry_run이면 로그만 남기고 None 반환."""
        if self._dry_run:
            logger.info(
                "order_dry_run",
                symbol=request.symbol,
                side=request.side.value,
                type=request.order_type.value,
                qty=str(request.quantity),
                price=str(request.price),
            )
            return None

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

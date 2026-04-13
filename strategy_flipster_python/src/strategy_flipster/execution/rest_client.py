"""Flipster 주문 실행 REST 클라이언트"""

from __future__ import annotations

import json
from decimal import Decimal

import httpx
import structlog

from strategy_flipster.config import FlipsterApiConfig
from strategy_flipster.error import AppError, AppException, ErrorKind
from strategy_flipster.execution.auth import make_auth_headers
from strategy_flipster.types import (
    MarginType,
    OrderRequest,
    OrderResponse,
    OrderSide,
    OrderStatus,
    OrderType,
    TimeInForce,
)

logger = structlog.get_logger(__name__)


class FlipsterExecutionClient:
    """Flipster Trade API 클라이언트"""

    def __init__(self, config: FlipsterApiConfig) -> None:
        self._config: FlipsterApiConfig = config
        self._client: httpx.AsyncClient | None = None

    async def start(self) -> None:
        self._client = httpx.AsyncClient(
            base_url=self._config.base_url,
            timeout=10.0,
        )

    async def stop(self) -> None:
        if self._client is not None:
            await self._client.aclose()
            self._client = None

    # ── 인증 ──

    def _auth_headers(
        self, method: str, path: str, body: str | None = None,
    ) -> dict[str, str]:
        return make_auth_headers(
            self._config.api_key,
            self._config.api_secret,
            method,
            path,
            body,
        )

    async def _request(
        self,
        method: str,
        path: str,
        body: dict | None = None,
    ) -> dict:
        if self._client is None:
            raise AppException(AppError(kind=ErrorKind.NETWORK, message="클라이언트 미시작"))

        body_str: str | None = None
        if body is not None:
            body_str = json.dumps(body)

        headers = self._auth_headers(method, path, body_str)
        headers["Content-Type"] = "application/json"

        if method == "GET":
            resp = await self._client.get(path, headers=headers)
        elif method == "POST":
            resp = await self._client.post(path, headers=headers, content=body_str)
        elif method == "PUT":
            resp = await self._client.put(path, headers=headers, content=body_str)
        elif method == "DELETE":
            resp = await self._client.request("DELETE", path, headers=headers, content=body_str)
        else:
            raise AppException(AppError(kind=ErrorKind.API, message=f"지원하지 않는 메서드: {method}"))

        if resp.status_code != 200:
            raise AppException(AppError(
                kind=ErrorKind.API,
                message=f"{method} {path}: {resp.status_code} {resp.text}",
                status_code=resp.status_code,
            ))

        return resp.json()

    # ── 주문 ──

    async def place_order(self, req: OrderRequest) -> OrderResponse:
        """주문 제출"""
        body: dict = {
            "symbol": req.symbol,
            "side": req.side.value,
            "type": req.order_type.value,
        }
        if req.quantity is not None:
            body["quantity"] = str(req.quantity)
        if req.amount is not None:
            body["amount"] = str(req.amount)
        if req.price is not None:
            body["price"] = str(req.price)
        if req.reduce_only:
            body["reduceOnly"] = True
        if req.time_in_force is not None:
            body["timeInForce"] = req.time_in_force.value
        if req.max_slippage_price is not None:
            body["maxSlippagePrice"] = str(req.max_slippage_price)

        data = await self._request("POST", "/api/v1/trade/order", body)
        order_data = data.get("order", data)

        logger.info(
            "order_placed",
            symbol=req.symbol,
            side=req.side.value,
            type=req.order_type.value,
            order_id=order_data.get("orderId"),
            status=order_data.get("status"),
        )

        return self._parse_order_response(order_data)

    async def cancel_order(self, symbol: str, order_id: str) -> None:
        """대기 주문 취소"""
        body = {"symbol": symbol, "orderId": order_id}
        await self._request("DELETE", "/api/v1/trade/order", body)
        logger.info("order_cancelled", symbol=symbol, order_id=order_id)

    async def set_tp_sl(
        self,
        symbol: str,
        order_id: str,
        take_profit: Decimal | None = None,
        stop_loss: Decimal | None = None,
    ) -> OrderResponse:
        """TP/SL 설정"""
        body: dict = {"symbol": symbol, "orderId": order_id}
        if take_profit is not None:
            body["newTakeProfitPrice"] = str(take_profit)
        if stop_loss is not None:
            body["newStopLossPrice"] = str(stop_loss)

        data = await self._request("PUT", "/api/v1/trade/order", body)
        order_data = data.get("order", data)
        return self._parse_order_response(order_data)

    async def get_pending_orders(self, symbol: str | None = None) -> list[OrderResponse]:
        """대기 주문 목록 조회"""
        path = "/api/v1/trade/order"
        if symbol:
            path += f"?symbol={symbol}"
        data = await self._request("GET", path)
        orders = data.get("orders", [])
        return [self._parse_order_response(o) for o in orders]

    # ── 레버리지/마진 ──

    async def set_leverage(
        self,
        symbol: str,
        leverage: int,
        margin_type: MarginType,
    ) -> None:
        """레버리지 및 마진 모드 설정"""
        body = {
            "symbol": symbol,
            "leverage": leverage,
            "marginType": margin_type.value,
        }
        await self._request("POST", "/api/v1/trade/leverage", body)
        logger.info(
            "leverage_set",
            symbol=symbol,
            leverage=leverage,
            margin_type=margin_type.value,
        )

    async def adjust_margin(
        self,
        symbol: str,
        amount: Decimal,
        add: bool = True,
    ) -> None:
        """Isolated 마진 추가/축소"""
        body = {
            "symbol": symbol,
            "amount": str(amount),
            "type": 1 if add else 2,
        }
        await self._request("POST", "/api/v1/trade/margin", body)

    # ── 심볼 / 시장 (인증 필요) ──

    async def get_tradable_symbols(self) -> dict[str, list[str]]:
        """거래 가능 심볼 목록"""
        data = await self._request("GET", "/api/v1/trade/symbol")
        return {
            "spot": data.get("spot", []),
            "perpetual": data.get("perpetualSwap", []),
        }

    # ── 시장 (공개, 인증 불필요) ──

    async def _public_get(self, path: str) -> dict | list:
        if self._client is None:
            raise AppException(AppError(kind=ErrorKind.NETWORK, message="클라이언트 미시작"))
        resp = await self._client.get(path)
        if resp.status_code != 200:
            raise AppException(AppError(
                kind=ErrorKind.API,
                message=f"GET {path}: {resp.status_code} {resp.text}",
                status_code=resp.status_code,
            ))
        return resp.json()

    async def get_contract_info(self, symbol: str) -> dict:
        """계약 스펙 (tickSize, unitOrderQty, notionalMinOrderAmount 등)"""
        data = await self._public_get(f"/api/v1/market/contract?symbol={symbol}")
        if isinstance(data, list):
            if not data:
                raise AppException(AppError(kind=ErrorKind.API, message=f"contract 응답 비어있음: {symbol}"))
            return data[0]
        return data

    async def get_ticker(self, symbol: str) -> dict:
        """현재 ticker (bidPrice, askPrice, lastPrice, markPrice 등)"""
        data = await self._public_get(f"/api/v1/market/ticker?symbol={symbol}")
        if isinstance(data, list):
            if not data:
                raise AppException(AppError(kind=ErrorKind.API, message=f"ticker 응답 비어있음: {symbol}"))
            return data[0]
        return data

    # ── 파싱 ──

    @staticmethod
    def _parse_order_response(raw: dict) -> OrderResponse:
        side_str = raw.get("side", "BUY")
        side = OrderSide.BUY if side_str == "BUY" else OrderSide.SELL

        type_str = raw.get("orderType", "MARKET")
        order_type_map = {"MARKET": OrderType.MARKET, "LIMIT": OrderType.LIMIT, "STOP_MARKET": OrderType.STOP_MARKET}
        order_type = order_type_map.get(type_str, OrderType.MARKET)

        status_str = raw.get("status", "PENDING_NEW")
        status_map = {
            "PENDING_NEW": OrderStatus.PENDING_NEW,
            "NEW": OrderStatus.NEW,
            "PARTIALLY_FILLED": OrderStatus.PARTIALLY_FILLED,
            "FILLED": OrderStatus.FILLED,
            "CANCELED": OrderStatus.CANCELED,
            "REJECTED": OrderStatus.REJECTED,
        }
        status = status_map.get(status_str, OrderStatus.PENDING_NEW)

        qty_str = raw.get("quantity")
        price_str = raw.get("price")
        leaves_str = raw.get("leavesQty")

        return OrderResponse(
            order_id=raw.get("orderId", ""),
            symbol=raw.get("symbol", ""),
            side=side,
            order_type=order_type,
            status=status,
            quantity=Decimal(qty_str) if qty_str else None,
            price=Decimal(price_str) if price_str else None,
            leaves_qty=Decimal(leaves_str) if leaves_str else None,
        )

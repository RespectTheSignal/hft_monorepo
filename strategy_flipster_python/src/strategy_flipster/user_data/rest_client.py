"""Flipster REST 클라이언트 — 계정/포지션 조회"""

from __future__ import annotations

from decimal import Decimal

import httpx
import structlog

from strategy_flipster.config import FlipsterApiConfig
from strategy_flipster.error import AppError, AppException, ErrorKind
from strategy_flipster.execution.auth import make_auth_headers
from strategy_flipster.types import (
    AccountInfo,
    Balance,
    MarginType,
    Position,
    PositionSide,
)

logger = structlog.get_logger(__name__)


class FlipsterUserRestClient:
    """Flipster REST API — 계정 정보 조회용"""

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

    def _auth_headers(self, method: str, path: str, body: str | None = None) -> dict[str, str]:
        return make_auth_headers(
            self._config.api_key,
            self._config.api_secret,
            method,
            path,
            body,
        )

    async def _get(self, path: str) -> dict | list:
        if self._client is None:
            raise AppException(AppError(kind=ErrorKind.NETWORK, message="클라이언트 미시작"))

        headers = self._auth_headers("GET", path)
        resp = await self._client.get(path, headers=headers)

        if resp.status_code != 200:
            raise AppException(AppError(
                kind=ErrorKind.API,
                message=f"GET {path} failed: {resp.status_code} {resp.text}",
                status_code=resp.status_code,
            ))

        return resp.json()

    async def get_account(self) -> AccountInfo:
        data = await self._get("/api/v1/account")
        if not isinstance(data, dict):
            raise AppException(AppError(kind=ErrorKind.API, message="account 응답 형식 오류"))
        return AccountInfo(
            total_wallet_balance=Decimal(data["totalWalletBalance"]),
            total_unrealized_pnl=Decimal(data["totalUnrealizedPnl"]),
            total_margin_balance=Decimal(data["totalMarginBalance"]),
            available_balance=Decimal(data["availableBalance"]),
        )

    async def get_positions(self, symbol: str | None = None) -> list[Position]:
        path = "/api/v1/account/position"
        if symbol:
            path += f"?symbol={symbol}"
        data = await self._get(path)
        if not isinstance(data, list):
            raise AppException(AppError(kind=ErrorKind.API, message="position 응답 형식 오류"))
        return [self._parse_position(p) for p in data]

    async def get_balances(self, asset: str | None = None) -> list[Balance]:
        path = "/api/v1/account/balance"
        if asset:
            path += f"?asset={asset}"
        data = await self._get(path)
        if not isinstance(data, list):
            raise AppException(AppError(kind=ErrorKind.API, message="balance 응답 형식 오류"))
        return [
            Balance(
                asset=b["asset"],
                balance=Decimal(b["balance"]),
                available_balance=Decimal(b["availableBalance"]),
            )
            for b in data
        ]

    @staticmethod
    def _parse_position(raw: dict) -> Position:
        pos_side_str = raw.get("positionSide")
        if pos_side_str == "LONG":
            pos_side = PositionSide.LONG
        elif pos_side_str == "SHORT":
            pos_side = PositionSide.SHORT
        else:
            pos_side = PositionSide.NONE

        margin_str = raw.get("marginType", "CROSS")
        margin_type = MarginType.ISOLATED if margin_str == "ISOLATED" else MarginType.CROSS

        liq_price_str = raw.get("liquidationPrice")
        liq_price = Decimal(liq_price_str) if liq_price_str else None

        return Position(
            symbol=raw["symbol"],
            leverage=int(raw.get("leverage", 1)),
            margin_type=margin_type,
            position_side=pos_side,
            position_amount=Decimal(raw.get("positionAmount") or "0"),
            entry_price=Decimal(raw.get("entryPrice") or "0"),
            mark_price=Decimal(raw.get("markPrice") or "0"),
            unrealized_pnl=Decimal(raw.get("unrealizedPnl") or "0"),
            liquidation_price=liq_price,
        )

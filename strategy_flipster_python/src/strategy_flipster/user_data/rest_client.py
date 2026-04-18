"""Flipster REST 클라이언트 — 계정/포지션 조회 (GET 전용, retry 적용)"""

from __future__ import annotations

import asyncio
from decimal import Decimal

import httpx
import structlog

from strategy_flipster.config import FlipsterApiConfig
from strategy_flipster.error import AppError, AppException, ErrorKind
from strategy_flipster.execution.auth import make_auth_headers
from strategy_flipster.types import AccountInfo, Balance, Position
from strategy_flipster.user_data.position_parser import parse_position_row

logger = structlog.get_logger(__name__)

_RETRY_EXC = (httpx.TimeoutException, httpx.ConnectError, httpx.ReadError, httpx.RemoteProtocolError)
_RETRY_STATUS = frozenset({500, 502, 503, 504})


class FlipsterUserRestClient:
    """Flipster REST API — 계정 정보 조회용 (GET만, 항상 retry)"""

    def __init__(
        self,
        config: FlipsterApiConfig,
        max_retries: int = 3,
        backoff_base: float = 0.1,
    ) -> None:
        self._config: FlipsterApiConfig = config
        self._client: httpx.AsyncClient | None = None
        self._max_retries: int = max_retries
        self._backoff_base: float = backoff_base

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

        max_attempts = self._max_retries + 1
        attempt = 0
        while True:
            attempt += 1
            headers = self._auth_headers("GET", path)
            try:
                resp = await self._client.get(path, headers=headers)
            except _RETRY_EXC as e:
                if attempt >= max_attempts:
                    raise AppException(AppError(
                        kind=ErrorKind.NETWORK,
                        message=f"GET {path}: {type(e).__name__} {e} (after {attempt} attempts)",
                    )) from e
                delay = self._backoff_base * (3 ** (attempt - 1))
                logger.warning("user_rest_retry_network", path=path, attempt=attempt, exc=type(e).__name__, delay=delay)
                await asyncio.sleep(delay)
                continue

            if resp.status_code == 200:
                return resp.json()
            if resp.status_code in _RETRY_STATUS and attempt < max_attempts:
                delay = self._backoff_base * (3 ** (attempt - 1))
                logger.warning("user_rest_retry_5xx", path=path, status=resp.status_code, attempt=attempt, delay=delay)
                await asyncio.sleep(delay)
                continue

            raise AppException(AppError(
                kind=ErrorKind.API,
                message=f"GET {path} failed: {resp.status_code} {resp.text}",
                status_code=resp.status_code,
            ))

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
        return parse_position_row(raw)

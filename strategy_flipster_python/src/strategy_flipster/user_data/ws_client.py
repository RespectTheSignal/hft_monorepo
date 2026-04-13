"""Flipster WebSocket private 토픽 클라이언트.

account, account.position, account.balance, account.margin 구독.
"""

from __future__ import annotations

import asyncio
import json
from decimal import Decimal
from typing import Callable

import structlog
import websockets

from strategy_flipster.config import FlipsterApiConfig
from strategy_flipster.execution.auth import make_ws_auth_headers
from strategy_flipster.types import (
    AccountInfo,
    Balance,
    MarginType,
    Position,
    PositionSide,
)
from strategy_flipster.user_data.state import UserState

logger = structlog.get_logger(__name__)

# 구독할 private 토픽
PRIVATE_TOPICS: list[str] = [
    "account",
    "account.position",
    "account.balance",
    "account.margin",
]


class FlipsterUserWsClient:
    """Flipster WS private 스트림 — UserState 실시간 갱신"""

    def __init__(
        self,
        config: FlipsterApiConfig,
        state: UserState,
        on_update: Callable[[], None] | None = None,
    ) -> None:
        self._config: FlipsterApiConfig = config
        self._state: UserState = state
        self._on_update: Callable[[], None] | None = on_update
        self._ws: websockets.WebSocketClientProtocol | None = None  # type: ignore[assignment]
        self._running: bool = False
        self._reconnect_delay: float = 1.0
        self._max_reconnect_delay: float = 30.0

    async def start(self) -> None:
        """WS 연결 + 구독 + 수신 루프 시작"""
        self._running = True
        while self._running:
            try:
                await self._connect_and_run()
            except Exception:
                if not self._running:
                    break
                logger.exception(
                    "flipster_ws_error",
                    reconnect_delay=self._reconnect_delay,
                )
                await asyncio.sleep(self._reconnect_delay)
                self._reconnect_delay = min(
                    self._reconnect_delay * 2,
                    self._max_reconnect_delay,
                )

    async def stop(self) -> None:
        self._running = False
        if self._ws is not None:
            await self._ws.close()
            self._ws = None

    async def _connect_and_run(self) -> None:
        headers = make_ws_auth_headers(
            self._config.api_key,
            self._config.api_secret,
        )

        self._ws = await websockets.connect(  # type: ignore[assignment]
            self._config.ws_url,
            additional_headers=headers,
        )
        logger.info("flipster_ws_connected")
        self._reconnect_delay = 1.0  # 연결 성공 시 리셋

        # 구독 요청
        subscribe_msg = json.dumps({
            "op": "subscribe",
            "args": PRIVATE_TOPICS,
        })
        await self._ws.send(subscribe_msg)
        logger.info("flipster_ws_subscribed", topics=PRIVATE_TOPICS)

        # 수신 루프
        async for raw_msg in self._ws:
            if not self._running:
                break
            self._handle_message(raw_msg)

    def _handle_message(self, raw_msg: str | bytes) -> None:
        try:
            msg = json.loads(raw_msg)
        except json.JSONDecodeError:
            logger.warning("flipster_ws_invalid_json", raw=str(raw_msg)[:200])
            return

        topic = msg.get("topic", "")
        data_list = msg.get("data", [])

        for data_item in data_list:
            rows = data_item.get("rows", [])
            for row in rows:
                self._apply_update(topic, row)

        if self._on_update is not None:
            self._on_update()

    def _apply_update(self, topic: str, row: dict) -> None:
        if topic == "account":
            self._state.update_account(AccountInfo(
                total_wallet_balance=Decimal(row.get("totalWalletBalance", "0")),
                total_unrealized_pnl=Decimal(row.get("totalUnrealizedPnl", "0")),
                total_margin_balance=Decimal(row.get("totalMarginBalance", "0")),
                available_balance=Decimal(row.get("availableBalance", "0")),
            ))

        elif topic == "account.position":
            pos_side_str = row.get("positionSide")
            if pos_side_str == "LONG":
                pos_side = PositionSide.LONG
            elif pos_side_str == "SHORT":
                pos_side = PositionSide.SHORT
            else:
                pos_side = PositionSide.NONE

            margin_str = row.get("marginType", "CROSS")
            margin_type = MarginType.ISOLATED if margin_str == "ISOLATED" else MarginType.CROSS

            liq_str = row.get("liquidationPrice")
            liq_price = Decimal(liq_str) if liq_str else None

            position = Position(
                symbol=row["symbol"],
                leverage=int(row.get("leverage", 1)),
                margin_type=margin_type,
                position_side=pos_side,
                position_amount=Decimal(row.get("positionAmount") or "0"),
                entry_price=Decimal(row.get("entryPrice") or "0"),
                mark_price=Decimal(row.get("markPrice") or "0"),
                unrealized_pnl=Decimal(row.get("unrealizedPnl") or "0"),
                liquidation_price=liq_price,
            )
            self._state.update_position(position)

        elif topic == "account.balance":
            self._state.update_balance(Balance(
                asset=row["asset"],
                balance=Decimal(row.get("balance", "0")),
                available_balance=Decimal(row.get("availableBalance", "0")),
            ))

        elif topic == "account.margin":
            # margin 업데이트는 account 토픽의 상세 버전
            # 필요 시 별도 MarginInfo 타입으로 확장 가능
            pass

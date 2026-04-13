"""UserState — 사용자 계정 상태 관리.

asyncio 단일 스레드에서 사용. Rust 번역 시 Arc<RwLock<UserState>>.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field

from strategy_flipster.types import AccountInfo, Balance, Position


@dataclass
class UserState:
    """사용자 계정 상태 — WS 업데이트로 실시간 갱신"""

    positions: dict[str, Position] = field(default_factory=dict)
    balances: dict[str, Balance] = field(default_factory=dict)
    account: AccountInfo | None = None
    last_update_ns: int = 0

    def update_position(self, position: Position) -> None:
        self.positions[position.symbol] = position
        self.last_update_ns = time.time_ns()

    def remove_position(self, symbol: str) -> None:
        self.positions.pop(symbol, None)
        self.last_update_ns = time.time_ns()

    def update_balance(self, balance: Balance) -> None:
        self.balances[balance.asset] = balance
        self.last_update_ns = time.time_ns()

    def update_account(self, account: AccountInfo) -> None:
        self.account = account
        self.last_update_ns = time.time_ns()

    def get_position(self, symbol: str) -> Position | None:
        return self.positions.get(symbol)

    def get_balance(self, asset: str) -> Balance | None:
        return self.balances.get(asset)

    def has_position(self, symbol: str) -> bool:
        pos = self.positions.get(symbol)
        if pos is None:
            return False
        return pos.position_amount != 0

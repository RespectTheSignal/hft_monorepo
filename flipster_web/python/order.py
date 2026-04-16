from __future__ import annotations

import time
import uuid
from enum import Enum
from dataclasses import dataclass, field
from typing import Optional


class Side(str, Enum):
    LONG = "Long"
    SHORT = "Short"


class MarginType(str, Enum):
    ISOLATED = "Isolated"
    CROSS = "Cross"


class OrderType(str, Enum):
    MARKET = "ORDER_TYPE_MARKET"
    LIMIT = "ORDER_TYPE_LIMIT"


@dataclass
class OrderParams:
    side: Side
    amount_usd: float
    order_type: OrderType = OrderType.MARKET
    price: Optional[float] = None  # required for limit orders
    leverage: int = 1
    margin_type: MarginType = MarginType.ISOLATED

    def __post_init__(self):
        if self.order_type == OrderType.LIMIT and self.price is None:
            raise ValueError("price is required for limit orders")

    def to_body(self, ref_price: float) -> dict:
        """Build the JSON body for the Flipster API.

        ref_price: current market price, used as the price field for market
        orders and to calculate size.
        """
        now_ns = str(time.time_ns())
        price = self.price if self.price is not None else ref_price
        return {
            "side": self.side.value,
            "requestId": str(uuid.uuid4()),
            "timestamp": now_ns,
            "refServerTimestamp": now_ns,
            "refClientTimestamp": now_ns,
            "leverage": self.leverage,
            "price": str(price),
            "amount": str(self.amount_usd),
            "attribution": "SEARCH_PERPETUAL",
            "marginType": self.margin_type.value,
            "orderType": self.order_type.value,
        }

    @staticmethod
    def close_body(price: float) -> dict:
        """Build the JSON body for closing a position (PUT size=0)."""
        now_ns = str(time.time_ns())
        return {
            "requestId": str(uuid.uuid4()),
            "timestamp": now_ns,
            "refServerTimestamp": now_ns,
            "refClientTimestamp": now_ns,
            "size": "0",
            "price": str(price),
            "attribution": "SEARCH_PERPETUAL",
            "orderType": "ORDER_TYPE_MARKET",
        }

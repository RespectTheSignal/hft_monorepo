from __future__ import annotations

import time
from enum import Enum
from dataclasses import dataclass
from typing import Optional


class Side(str, Enum):
    LONG = "long"
    SHORT = "short"


class OrderType(str, Enum):
    MARKET = "market"
    LIMIT = "limit"


class TimeInForce(str, Enum):
    GTC = "gtc"  # good till cancel
    IOC = "ioc"  # immediate or cancel
    POC = "poc"  # pending or cancel (post-only)


@dataclass
class OrderParams:
    """Gate.io futures order parameters.

    Gate uses signed size: positive = long, negative = short.
    The ``side`` field controls the sign automatically.
    """

    side: Side
    size: int  # number of contracts (unsigned, sign derived from side)
    order_type: OrderType = OrderType.MARKET
    price: Optional[float] = None  # required for limit; "0" sent for market
    tif: TimeInForce = TimeInForce.GTC
    reduce_only: bool = False
    text: Optional[str] = None  # custom label, e.g. "t-1234567890"

    def __post_init__(self):
        if self.order_type == OrderType.LIMIT and self.price is None:
            raise ValueError("price is required for limit orders")

    def to_body(self, contract: str) -> dict:
        """Build the JSON body for the Gate.io futures order API.

        contract: e.g. "BTC_USDT"
        """
        signed_size = self.size if self.side == Side.LONG else -self.size
        price_str = str(self.price) if self.price is not None else "0"
        text = self.text or f"t-{int(time.time() * 1000)}"
        return {
            "contract": contract,
            "size": str(signed_size),
            "price": price_str,
            "order_type": self.order_type.value,
            "tif": self.tif.value,
            "text": text,
            "reduce_only": self.reduce_only,
        }

    @staticmethod
    def close_body(contract: str, size: int, price: Optional[float] = None) -> dict:
        """Build a reduce-only order to close ``size`` contracts.

        size: positive = close short (buy), negative = close long (sell).
        price: limit price, or None/0 for market.
        """
        return {
            "contract": contract,
            "size": str(size),
            "price": str(price) if price else "0",
            "order_type": "market",
            "tif": "ioc",
            "text": f"t-{int(time.time() * 1000)}",
            "reduce_only": True,
        }

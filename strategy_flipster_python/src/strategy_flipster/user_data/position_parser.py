"""Flipster position row parser."""

from __future__ import annotations

from decimal import Decimal

from strategy_flipster.types import MarginType, Position, PositionSide


def parse_position_row(raw: dict) -> Position:
    pos_side_str = raw.get("positionSide")
    if pos_side_str == "LONG":
        pos_side = PositionSide.LONG
        sign = Decimal("1")
    elif pos_side_str == "SHORT":
        pos_side = PositionSide.SHORT
        sign = Decimal("-1")
    else:
        pos_side = PositionSide.NONE
        sign = Decimal("0")

    margin_str = raw.get("marginType", "CROSS")
    margin_type = MarginType.ISOLATED if margin_str == "ISOLATED" else MarginType.CROSS

    liq_price_str = raw.get("liquidationPrice")
    liq_price = Decimal(liq_price_str) if liq_price_str else None

    entry_price = Decimal(raw.get("entryPrice") or "0")
    mark_price = Decimal(raw.get("markPrice") or "0")
    unrealized_pnl = Decimal(raw.get("unrealizedPnl") or "0")

    qty_str = raw.get("positionQty")
    notional_str = raw.get("positionAmount")

    if qty_str not in (None, ""):
        qty = abs(Decimal(qty_str))
    elif notional_str not in (None, ""):
        notional = abs(Decimal(notional_str))
        price = mark_price if mark_price > 0 else entry_price
        qty = (notional / price) if price > 0 else Decimal("0")
    else:
        qty = Decimal("0")

    signed_qty = qty * sign if sign != 0 else Decimal("0")

    return Position(
        symbol=raw["symbol"],
        leverage=int(raw.get("leverage", 1)),
        margin_type=margin_type,
        position_side=pos_side,
        position_amount=signed_qty,
        entry_price=entry_price,
        mark_price=mark_price,
        unrealized_pnl=unrealized_pnl,
        liquidation_price=liq_price,
    )

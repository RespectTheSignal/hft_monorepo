"""핵심 데이터 타입 — Rust struct 대응 (@dataclass frozen+slots)"""

from __future__ import annotations

import enum
import struct
import time
from dataclasses import dataclass
from decimal import Decimal


# ──────────────────────────────────────────────
# Enums (→ Rust enum)
# ──────────────────────────────────────────────

class Exchange(enum.Enum):
    FLIPSTER = "flipster"
    BINANCE = "binance"


class OrderSide(enum.Enum):
    BUY = "BUY"
    SELL = "SELL"


class OrderType(enum.Enum):
    MARKET = "MARKET"
    LIMIT = "LIMIT"
    STOP_MARKET = "STOP_MARKET"


class OrderStatus(enum.Enum):
    PENDING_NEW = "PENDING_NEW"
    NEW = "NEW"
    PARTIALLY_FILLED = "PARTIALLY_FILLED"
    FILLED = "FILLED"
    CANCELED = "CANCELED"
    REJECTED = "REJECTED"


class TimeInForce(enum.Enum):
    GTC = "GTC"
    IOC = "IOC"
    FOK = "FOK"


class MarginType(enum.Enum):
    CROSS = "CROSS"
    ISOLATED = "ISOLATED"


class PositionSide(enum.Enum):
    LONG = "LONG"
    SHORT = "SHORT"
    NONE = "NONE"


# ──────────────────────────────────────────────
# 시장 데이터
# ──────────────────────────────────────────────

@dataclass(frozen=True, slots=True)
class BookTicker:
    """정규화된 호가 데이터 — 모든 거래소 공통"""

    exchange: str
    symbol: str
    bid_price: float
    ask_price: float
    bid_size: float       # Flipster: 0.0 (미제공)
    ask_size: float       # Flipster: 0.0
    last_price: float     # Binance: 0.0
    mark_price: float     # Binance: 0.0
    index_price: float    # Binance: 0.0
    event_time_ms: int    # 서버 이벤트 시각 (ms 통일)
    recv_ts_ns: int       # 전략 수신 시각 (ns)


@dataclass(frozen=True, slots=True)
class MarketStats:
    """주기적 시장 데이터 — REST ticker 폴링으로 갱신.

    bookticker ZMQ 푸시와 별개. funding/OI/volume 등 느린 지표 포함.
    """

    symbol: str
    bid_price: float
    ask_price: float
    last_price: float
    mark_price: float
    index_price: float
    funding_rate: float
    next_funding_time_ns: int   # ns (Flipster 응답 단위)
    open_interest: float
    volume_24h: float
    turnover_24h: float
    price_change_24h: float
    updated_ns: int


@dataclass(frozen=True, slots=True)
class Trade:
    """정규화된 체결 데이터"""

    exchange: str
    symbol: str
    price: float
    size: float
    trade_id: int
    create_time_ms: int
    server_time_ms: int
    recv_ts_ns: int


# ──────────────────────────────────────────────
# 주문
# ──────────────────────────────────────────────

@dataclass(frozen=True, slots=True)
class OrderRequest:
    """주문 요청 — Strategy → OrderManager"""

    symbol: str
    side: OrderSide
    order_type: OrderType
    quantity: Decimal | None = None
    amount: Decimal | None = None
    price: Decimal | None = None
    reduce_only: bool = False
    time_in_force: TimeInForce | None = None
    max_slippage_price: Decimal | None = None


@dataclass(frozen=True, slots=True)
class OrderResponse:
    """주문 응답 — Flipster REST"""

    order_id: str
    symbol: str
    side: OrderSide
    order_type: OrderType
    status: OrderStatus
    quantity: Decimal | None = None
    price: Decimal | None = None
    leaves_qty: Decimal | None = None


# ──────────────────────────────────────────────
# 계정/포지션
# ──────────────────────────────────────────────

@dataclass(frozen=True, slots=True)
class Position:
    """포지션 정보"""

    symbol: str
    leverage: int
    margin_type: MarginType
    position_side: PositionSide
    position_amount: Decimal
    entry_price: Decimal
    mark_price: Decimal
    unrealized_pnl: Decimal
    liquidation_price: Decimal | None = None


@dataclass(frozen=True, slots=True)
class Balance:
    """자산 잔고"""

    asset: str
    balance: Decimal
    available_balance: Decimal


@dataclass(frozen=True, slots=True)
class AccountInfo:
    """계정 요약"""

    total_wallet_balance: Decimal
    total_unrealized_pnl: Decimal
    total_margin_balance: Decimal
    available_balance: Decimal


# ──────────────────────────────────────────────
# Wire format 파서 (프리컴파일 struct)
# ──────────────────────────────────────────────

# Binance BookTicker: exchange(16) + symbol(32) + 4×f64 + 5×i64 = 120B
BINANCE_BT_STRUCT: struct.Struct = struct.Struct("<16s32s4d5q")
BINANCE_BT_SIZE: int = BINANCE_BT_STRUCT.size  # 120

# Flipster BookTicker: symbol(32) + 5×f64 + 4×i64 = 104B
FLIPSTER_BT_STRUCT: struct.Struct = struct.Struct("<32s5d4q")
FLIPSTER_BT_SIZE: int = FLIPSTER_BT_STRUCT.size  # 104

# Binance Trade: exchange(16) + symbol(32) + 2×f64 + 3×i64 + 1B + 7B padding + 4×i64 = 128B
BINANCE_TRADE_STRUCT: struct.Struct = struct.Struct("<16s32s2d3qb7x4q")
BINANCE_TRADE_SIZE: int = BINANCE_TRADE_STRUCT.size  # 128


def parse_binance_bookticker(payload: bytes | memoryview) -> BookTicker:
    """Binance 120-byte 바이너리 → BookTicker"""
    recv_ts = time.time_ns()
    (
        exchange_raw, symbol_raw,
        bid_price, ask_price, bid_size, ask_size,
        event_time, server_time, _pub_sent, _sub_recv, _sub_dump,
    ) = BINANCE_BT_STRUCT.unpack(payload)

    return BookTicker(
        exchange=exchange_raw.rstrip(b"\x00").decode("utf-8"),
        symbol=symbol_raw.rstrip(b"\x00").decode("utf-8"),
        bid_price=bid_price,
        ask_price=ask_price,
        bid_size=bid_size,
        ask_size=ask_size,
        last_price=0.0,
        mark_price=0.0,
        index_price=0.0,
        event_time_ms=event_time,
        recv_ts_ns=recv_ts,
    )


def parse_flipster_bookticker(payload: bytes | memoryview) -> BookTicker:
    """Flipster 104-byte 바이너리 → BookTicker"""
    recv_ts = time.time_ns()
    (
        symbol_raw,
        bid_price, ask_price, last_price, mark_price, index_price,
        server_ts_ns, _pub_recv, _pub_sent, _sub_recv,
    ) = FLIPSTER_BT_STRUCT.unpack(payload)

    return BookTicker(
        exchange="flipster",
        symbol=symbol_raw.rstrip(b"\x00").decode("utf-8"),
        bid_price=bid_price,
        ask_price=ask_price,
        bid_size=0.0,
        ask_size=0.0,
        last_price=last_price,
        mark_price=mark_price,
        index_price=index_price,
        event_time_ms=server_ts_ns // 1_000_000,  # ns → ms
        recv_ts_ns=recv_ts,
    )


def parse_binance_trade(payload: bytes | memoryview) -> Trade:
    """Binance 128-byte 바이너리 → Trade"""
    recv_ts = time.time_ns()
    (
        exchange_raw, symbol_raw,
        price, size,
        trade_id, create_time, create_time_ms,
        _is_internal,
        server_time, _pub_sent, _sub_recv, _sub_dump,
    ) = BINANCE_TRADE_STRUCT.unpack(payload)

    return Trade(
        exchange=exchange_raw.rstrip(b"\x00").decode("utf-8"),
        symbol=symbol_raw.rstrip(b"\x00").decode("utf-8"),
        price=price,
        size=size,
        trade_id=trade_id,
        create_time_ms=create_time_ms,
        server_time_ms=server_time,
        recv_ts_ns=recv_ts,
    )

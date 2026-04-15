"""Type stubs for hft_strategy_shm._core (pyo3-generated)."""
from typing import List, Optional

__version__: str

# Side / TIF / OrderType / OrderKind — raw u8 discriminants mirrored in Rust.
SIDE_BUY: int
SIDE_SELL: int
TIF_GTC: int
TIF_IOC: int
TIF_FOK: int
TIF_POSTONLY: int
TIF_DAY: int
ORD_TYPE_LIMIT: int
ORD_TYPE_MARKET: int
ORDER_KIND_PLACE: int
ORDER_KIND_CANCEL: int
ORDER_FRAME_SIZE: int
ORDER_FRAME_ALIGN: int
ORDER_KIND_PLACE_I: int
ORDER_KIND_CANCEL_I: int


def wall_clock_ns() -> int: ...


class PyOrderBuilder:
    exchange: str
    symbol: str
    kind: int
    side: int
    tif: int
    ord_type: int
    price_raw: int
    size_raw: int
    client_id: int
    ts_ns: int
    aux: List[int]  # length 5

    def __init__(self) -> None: ...
    def clear(self) -> None: ...


class PyQuoteSnapshot:
    seq: int
    exchange_id: int
    symbol_idx: int
    bid_price: int
    bid_size: int
    ask_price: int
    ask_size: int
    event_ns: int
    recv_ns: int
    pub_ns: int


class PyTradeFrame:
    seq: int
    exchange_id: int
    symbol_idx: int
    price: int
    size: int
    trade_id: int
    event_ns: int
    recv_ns: int
    pub_ns: int
    flags: int


class PyStrategyClient:
    vm_id: int

    @classmethod
    def open_dev_shm(
        cls,
        path: str,
        *,
        quote_slot_count: int,
        trade_ring_capacity: int,
        symtab_capacity: int,
        order_ring_capacity: int,
        n_max: int,
        vm_id: int,
    ) -> "PyStrategyClient": ...

    @classmethod
    def open_hugetlbfs(
        cls,
        path: str,
        *,
        quote_slot_count: int,
        trade_ring_capacity: int,
        symtab_capacity: int,
        order_ring_capacity: int,
        n_max: int,
        vm_id: int,
    ) -> "PyStrategyClient": ...

    @classmethod
    def open_pci_bar(
        cls,
        path: str,
        *,
        quote_slot_count: int,
        trade_ring_capacity: int,
        symtab_capacity: int,
        order_ring_capacity: int,
        n_max: int,
        vm_id: int,
    ) -> "PyStrategyClient": ...

    @classmethod
    def open_dev_shm_with_retry(
        cls,
        path: str,
        *,
        quote_slot_count: int,
        trade_ring_capacity: int,
        symtab_capacity: int,
        order_ring_capacity: int,
        n_max: int,
        vm_id: int,
        timeout_ms: int = 10_000,
        step_ms: int = 100,
    ) -> "PyStrategyClient": ...

    def heartbeat_age_ns(self) -> Optional[int]: ...
    def intern_symbol(self, exchange: str, symbol: str) -> int: ...
    def read_quote(self, symbol_idx: int) -> Optional[PyQuoteSnapshot]: ...
    def try_consume_trade(self) -> Optional[PyTradeFrame]: ...
    def publish_order(self, builder: PyOrderBuilder) -> bool: ...

    def publish_simple(
        self,
        exchange: str,
        symbol: str,
        *,
        kind: int,
        side: int,
        tif: int,
        ord_type: int,
        price_raw: int,
        size_raw: int,
        client_id: int,
        ts_ns: int,
    ) -> bool: ...

    @staticmethod
    def exchange_from_u8(v: int) -> str: ...

    @staticmethod
    def exchange_to_u8_str(s: str) -> int: ...

"""hft_strategy_shm — Python binding for the Rust SharedRegion (v2) strategy facade.

이 모듈은 `hft-strategy-shm` (Rust) 을 pyo3 로 래핑한 `_core` 를 re-export 한다.
전략 VM 이 Publisher 가 올려둔 SharedRegion 에 붙어 quote / trade 를 읽고
자기 vm_id 전용 order ring 으로 주문을 publish 한다.

# 사용법
```python
import hft_strategy_shm as shm

client = shm.PyStrategyClient.open_pci_bar(
    "/sys/bus/pci/devices/0000:00:04.0/resource2",
    quote_slot_count=10_000,
    trade_ring_capacity=1 << 20,
    symtab_capacity=16_384,
    order_ring_capacity=16_384,
    n_max=4,
    vm_id=2,
)

# Hot path: OrderBuilder 1회 생성 후 field 만 갈아끼워 재사용.
b = shm.PyOrderBuilder()
b.exchange = "gate"
b.symbol = "BTC_USDT"
b.side = shm.SIDE_BUY
b.tif = shm.TIF_IOC
b.ord_type = shm.ORD_TYPE_LIMIT
b.price_raw = 50_000_00000
b.size_raw = 10
b.client_id = 42
b.ts_ns = shm.wall_clock_ns()

ok = client.publish_order(b)
if not ok:
    # ring full or symbol intern 실패.
    pass
```
"""

from ._core import (  # noqa: F401
    # 클래스
    PyStrategyClient,
    PyOrderBuilder,
    PyQuoteSnapshot,
    PyTradeFrame,
    # 함수
    wall_clock_ns,
    # 상수 — Side
    SIDE_BUY,
    SIDE_SELL,
    # 상수 — TIF
    TIF_GTC,
    TIF_IOC,
    TIF_FOK,
    TIF_POSTONLY,
    TIF_DAY,
    # 상수 — OrderType
    ORD_TYPE_LIMIT,
    ORD_TYPE_MARKET,
    # 상수 — OrderKind
    ORDER_KIND_PLACE,
    ORDER_KIND_CANCEL,
    # 상수 — Place meta level
    PLACE_LEVEL_OPEN,
    PLACE_LEVEL_CLOSE,
    # 정합성 assert 용
    ORDER_FRAME_SIZE,
    ORDER_FRAME_ALIGN,
    __version__,
)

# 정합성 재확인 — import 타이밍에 구조체 크기/정렬 wire 불변식을 pin.
# Rust 쪽 OrderFrame `#[repr(C, align(64))]` 가 깨지면 여기서 바로 실패.
assert ORDER_FRAME_SIZE == 128, (
    f"OrderFrame size {ORDER_FRAME_SIZE} != 128 — wire layout 변경 가능성. "
    "ctypes 버전 재빌드 필요."
)
assert ORDER_FRAME_ALIGN == 64, (
    f"OrderFrame align {ORDER_FRAME_ALIGN} != 64 — cache-line 정렬 깨졌음."
)

__all__ = [
    "PyStrategyClient",
    "PyOrderBuilder",
    "PyQuoteSnapshot",
    "PyTradeFrame",
    "wall_clock_ns",
    "SIDE_BUY",
    "SIDE_SELL",
    "TIF_GTC",
    "TIF_IOC",
    "TIF_FOK",
    "TIF_POSTONLY",
    "TIF_DAY",
    "ORD_TYPE_LIMIT",
    "ORD_TYPE_MARKET",
    "ORDER_KIND_PLACE",
    "ORDER_KIND_CANCEL",
    "PLACE_LEVEL_OPEN",
    "PLACE_LEVEL_CLOSE",
    "ORDER_FRAME_SIZE",
    "ORDER_FRAME_ALIGN",
    "__version__",
]

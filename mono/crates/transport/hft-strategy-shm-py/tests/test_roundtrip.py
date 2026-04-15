"""End-to-end round trip — publisher (Rust) 를 Python 테스트 안에서 부팅할 수 있다는
전제가 깔리지 않으므로, 본 테스트는 순수 hft_strategy_shm 단독에서 구조체 invariant
와 builder 동작만 검증한다.

실제 SharedRegion 연결 테스트는 `crates/transport/hft-strategy-shm/tests/*` 의 Rust
통합 테스트에서 커버. 본 파이썬 테스트는 API shape 만 확인.

실행:
    pip install maturin pytest
    cd crates/transport/hft-strategy-shm-py
    maturin develop --release
    pytest tests/
"""
import pytest

import hft_strategy_shm as shm


def test_wire_constants():
    assert shm.ORDER_FRAME_SIZE == 128
    assert shm.ORDER_FRAME_ALIGN == 64


def test_order_builder_defaults():
    b = shm.PyOrderBuilder()
    assert b.kind == shm.ORDER_KIND_PLACE
    assert b.side == shm.SIDE_BUY
    assert b.tif == shm.TIF_GTC
    assert b.ord_type == shm.ORD_TYPE_LIMIT
    assert b.price_raw == 0
    assert b.size_raw == 0
    assert b.client_id == 0
    assert b.ts_ns == 0


def test_order_builder_field_roundtrip():
    b = shm.PyOrderBuilder()
    b.exchange = "gate"
    b.symbol = "BTC_USDT"
    b.side = shm.SIDE_SELL
    b.tif = shm.TIF_IOC
    b.ord_type = shm.ORD_TYPE_MARKET
    b.price_raw = 0
    b.size_raw = 5
    b.client_id = 1234
    b.ts_ns = 123_456_789
    b.aux = [1, 2, 3, 4, 5]
    assert b.exchange == "gate"
    assert b.symbol == "BTC_USDT"
    assert b.side == shm.SIDE_SELL
    assert b.tif == shm.TIF_IOC
    assert b.ord_type == shm.ORD_TYPE_MARKET
    assert list(b.aux) == [1, 2, 3, 4, 5]


def test_order_builder_clear():
    b = shm.PyOrderBuilder()
    b.price_raw = 999
    b.client_id = 777
    b.clear()
    assert b.price_raw == 0
    assert b.client_id == 0
    assert b.kind == shm.ORDER_KIND_PLACE


def test_wall_clock_ns_is_monotonic_ish():
    t1 = shm.wall_clock_ns()
    t2 = shm.wall_clock_ns()
    # 벽시계이므로 엄격한 monotonic 은 보장 못 하지만 같은 tick 이상은 기대.
    assert t1 > 0 and t2 > 0
    assert t2 >= t1 - 1_000_000  # 1ms 이상 뒤로 점프하진 않는다.


def test_exchange_static_helpers():
    # known
    assert shm.PyStrategyClient.exchange_to_u8_str("gate") >= 0
    assert shm.PyStrategyClient.exchange_to_u8_str("binance") >= 0
    # round trip
    u = shm.PyStrategyClient.exchange_to_u8_str("gate")
    assert shm.PyStrategyClient.exchange_from_u8(u) == "gate"

    with pytest.raises(ValueError):
        shm.PyStrategyClient.exchange_to_u8_str("ftx")
    with pytest.raises(ValueError):
        shm.PyStrategyClient.exchange_from_u8(255)

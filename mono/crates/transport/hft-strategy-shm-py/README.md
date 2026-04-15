# hft-strategy-shm-py

Python binding for `hft-strategy-shm` (Rust) — Strategy VM 이 Publisher 가 올려둔
SharedRegion 에 Python 에서 직접 붙어 quote/trade 를 읽고 자기 vm_id 전용 order
ring 으로 주문을 publish.

## 빌드

```bash
# maturin 0.15 이상
pip install maturin

# 개발 설치 (현재 venv 에 import 가능하게)
cd crates/transport/hft-strategy-shm-py
maturin develop --release

# 혹은 wheel 빌드 (CI/배포)
maturin build --release -o ./wheels
```

`_core.cpython-*.so` 가 `python/hft_strategy_shm/` 에 떨어진다. import 타이밍에
`ORDER_FRAME_SIZE == 128`, `ORDER_FRAME_ALIGN == 64` assertion 으로 wire
불변식을 자동 검증.

## 사용법

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

# Pre-intern for hot path — symbol_idx 캐시.
btc_idx = client.intern_symbol("gate", "BTC_USDT")

# MD 소비.
snap = client.read_quote(btc_idx)
if snap:
    mid_raw = (snap.bid_price + snap.ask_price) // 2

while True:
    t = client.try_consume_trade()
    if t is None:
        break
    print(f"trade sym={t.symbol_idx} px={t.price} sz={t.size}")

# 주문 발행 — Builder 1회 생성 후 field 만 갈아끼우며 재사용.
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
    # ring full or symbol intern 실패 — 상위가 판단.
    pass
```

## 백엔드 선택

| 메서드 | 백엔드 | 용도 |
|--------|--------|------|
| `open_dev_shm` | `/dev/shm/..` tmpfs | 단일 호스트 dev / 벤치 |
| `open_hugetlbfs` | `/dev/hugepages/..` | 단일 호스트 prod (2MB/1GB page) |
| `open_pci_bar` | `/sys/.../resource2` | guest VM (ivshmem) prod |
| `open_dev_shm_with_retry` | `/dev/shm/..` + retry | publisher boot race 대비 |

## 스레드 모델

- `PyStrategyClient` 는 `#[pyclass(unsendable)]` — 단일 Python 스레드에서만 사용.
- `publish_order` 는 짧은 atomic 연산만 수행해 GIL 해제 오버헤드 > 본 작업.
- `open_dev_shm_with_retry` 는 `allow_threads` 로 sleep 루프 동안 GIL 릴리즈.

## ctypes 호환성

Python 측이 `ctypes.Structure` 로 직접 OrderFrame 을 쓰고 싶다면:

```python
import ctypes
import hft_strategy_shm as shm

assert shm.ORDER_FRAME_SIZE == ctypes.sizeof(MyOrderFrame)  # 사용자 정의 struct
assert shm.ORDER_FRAME_ALIGN == 64
```

단, 일반적 권장은 `PyOrderBuilder` 재사용이다 — Rust 측이 offset/align 관리.

## 설치 시 체크리스트

- Rust 1.80+
- Python 3.9+
- Linux (tmpfs / hugetlbfs / ivshmem 사용 환경)
- publisher 가 먼저 SharedRegion 을 초기화했거나 `open_dev_shm_with_retry` 사용

# ADR 0005 — Price Regime 확립과 Wire Format 정비

Status: **Proposed** (2026-04-17)
Supersedes: 없음. 보완: ADR-0003 (topology), ADR-0004 (result path), SHM_DESIGN.md
Owner: jeong

## Context

Phase 2 E 까지 두 개의 주문 전송 경로가 완성됐다:

| 경로 | 가격 표현 | 제약 |
|------|-----------|------|
| **SHM** (`OrderFrame`) | `price: i64` | 정수만 허용. `integer_price()` 가 소수점 reject. |
| **ZMQ** (`OrderRequestWire`) | `price: f64` (LE 8B) | 실수 자유. executor 가 문자열로 format. |

여기서 3가지 문제가 발생한다:

1. **정수 가격 제약**: Gate/Binance USDT-Perpetual 의 호가 단위가 `0.1`, `0.01` 인
   심볼이 다수. SHM 경로는 이런 소수 가격을 보낼 수 없다.
2. **quantize 부재**: executor 5종 모두 가격을 받아서 문자열로 변환만 할 뿐,
   `ContractMeta.order_price_round` 기반 tick-size 양자화를 수행하지 않는다.
   잘못된 가격이 거래소 API 까지 도달하면 400 reject.
3. **cancel wire 취약성**: SHM cancel 은 `aux[0..5]` 에 exchange_order_id 를
   null-padded ASCII 로 담는다. 길이 제한 = 40B (5×u64). Gate 의 주문 ID 는 숫자
   → 충분하지만, 다른 거래소가 UUID 를 쓰면 초과 가능.

## Decision

### D1. 가격 표현: Scaled Integer (×10^8 고정소수)

**SHM `OrderFrame.price: i64` 을 "raw 정수" 에서 "×10^8 고정소수" 로 재해석한다.**

- 변환 규칙: `wire_price = (f64_price * 1e8).round() as i64`
- 역변환: `f64_price = wire_price as f64 / 1e8`
- 0 → Market 주문 (기존과 동일)
- 정밀도: 소수점 이하 8자리까지 오차 없이 표현. 현재 모든 거래소의 USDT-Perp
  tick_size 는 10^-4 이하이므로 충분.

**이유:**
- `OrderFrame` 레이아웃 (128B, `#[repr(C)]`) 자체는 변경하지 않는다.
  `price: i64` 필드를 유지하되 *해석만 바꾼다*. 따라서 ivshmem shared region
  reboot 없이 rolling upgrade 가능하다.
- ZMQ 경로의 `OrderRequestWire.price: f64` 은 그대로 유지. adapter 에서
  scaled integer ↔ f64 변환만 추가.
- Python `hft-strategy-shm-py` 에서 `price_raw` 로 노출하던 값이 이제
  `scaled_price` 로 명확해진다.

**Rolling upgrade 프로토콜:**
1. 먼저 reader (order-gateway) 를 업데이트: `price / 1e8` 로 해석.
2. 그 다음 writer (strategy/Python) 를 업데이트: `price * 1e8` 로 기록.
3. 전환 기간 동안 gateway 는 `if price > 1e6 then scaled else raw` 휴리스틱으로
   판별할 수 있다 (USDT-Perp 가격은 raw 로도 1e6 을 넘기 어려움).

### D2. Quantize 책임: Adapter Layer (order_adapter.rs)

**quantize 는 strategy → egress 변환 시점 (order_adapter) 에서 수행한다.**

- `order_request_to_order_frame()` 와 `order_request_to_wire()` 에
  `Option<&ContractMeta>` 파라미터를 추가한다.
- meta 가 있으면:
  - `price = round_to_tick(price, meta.order_price_round)`
  - `size = round_to_tick(size, meta.order_size_round)`
  - `size < meta.min_order_size` → reject (`OrderAdaptError::BelowMinSize`)
- meta 가 없으면 (테스트, dev 모드): 현재처럼 pass-through.

**이유:**
- Executor 는 거래소별 HTTP 포맷팅만 담당. tick_size 검증까지 넣으면
  executor 인터페이스가 `ContractMeta` 에 의존하게 돼서 관심사 분리가 깨진다.
- Strategy 코드에서 quantize 하면 V6/V7/V8 세 변형 전부에 중복 로직이 들어간다.
- Adapter 는 이미 SHM/ZMQ 분기점이므로 여기서 한 번만 처리하면 양쪽 다 커버.

**`round_to_tick` 구현:**
```
fn round_to_tick(value: f64, tick: f64) -> f64 {
    if tick <= 0.0 || !tick.is_finite() { return value; }
    (value / tick).round() * tick
}
```

### D3. Cancel Wire: 현행 유지, 길이 guard 추가

- 현재 `aux[0..5]` = 40B null-padded ASCII → Gate/Binance/Bybit/Bitget/OKX
  모든 거래소의 실제 order ID 길이는 20자 미만.
- **변경 없음.** 단, `exchange_order_id` 가 40B 를 초과하면 adapter 에서
  `OrderAdaptError::ExchangeOrderIdTooLong` 으로 reject 하는 guard 를 추가한다.
- 향후 UUID-based ID 를 쓰는 거래소가 추가되면 aux 해석을 확장하거나
  ZMQ cancel path 로 fallback.

### D4. `slot.seq` 탈결합 — 보류

- 현재 `OrderFrame.seq: u64` 은 SPSC ring 의 monotonic writer sequence 로,
  empty slot 판별 (seq==0) + ordering + torn read guard 를 겸한다.
- 이를 `ring_header.write_idx` + payload 내 `version` 으로 분리하면 ring 설계가
  깔끔해지지만, shared region layout 변경 (ADR-0003 §Decision 5: 전체 reboot)
  을 유발한다.
- **현 단계에서는 변경하지 않는다.** SHM result ring 도입 시 (ADR-0004 보류 조건
  충족) 같이 검토.

### D5. PyO3 v2 API — Price 인터페이스 정비

- `PyOrderBuilder` 에 `set_price_f64(price: f64)` 메서드 추가.
  내부에서 `(price * 1e8).round() as i64` 변환.
- 기존 `set_price(raw: i64)` 는 deprecated 으로 유지 (호환).
- `PyQuoteSnapshot` / `PyTradeFrame` 의 가격은 이미 f64 → 변경 없음.

## Alternatives Considered

| 대안 | 장점 | 기각 이유 |
|------|------|-----------|
| `OrderFrame.price` 를 `f64` 로 변경 | 변환 불필요 | repr(C) layout 변경 = shared region reboot + Python ctypes 전면 수정. |
| 심볼별 동적 precision (×10^N, N from meta) | 정밀도 최적화 | reader 가 N 을 알아야 해서 wire 에 precision 필드 필요 → layout 변경. |
| Executor 에서 quantize | 거래소별 정밀 제어 | 5개 executor 에 중복. adapter 에서 하면 1곳. |
| Strategy 에서 quantize | 빠른 reject | V6/V7/V8 × quantize 중복. 관심사 혼합. |

## Consequences

- `order_adapter.rs` 에 `ContractMeta` 의존 + `round_to_tick` + scaled integer 변환 추가.
- `integer_price()` 는 `scaled_price()` 로 rename, 소수 가격도 허용.
- Executor 코드 변경 없음 (이미 f64 를 받아 문자열 변환).
- Python 측 `hft-strategy-shm-py` 에 `set_price_f64()` 추가, 기존 API deprecated.
- SHM shared region layout 변경 없음 → 무중단 rolling upgrade 가능.
- check.md "price regime" 항목을 구현 후 `[x]` 전환.

## Implementation Steps (Codex 프롬프트 참조)

1. `order_adapter.rs`: `scaled_price()` 함수 (×1e8), `round_to_tick()`, `ContractMeta` 파라미터 추가.
2. `order_adapter.rs`: `order_request_to_order_frame()` + `order_request_to_wire()` 에 quantize 적용.
3. Gateway `shm_sub.rs` / `lib.rs`: SHM 경로에서 `price / 1e8` 역변환 추가.
4. `hft-strategy-shm-py`: `set_price_f64()` + `PRICE_SCALE = 1e8` 상수.
5. 테스트: 소수 가격 roundtrip (0.1, 0.01, 0.0001), tick quantize, min_size reject, cancel id guard.
6. 3-step build gate: `cargo check → test → clippy` 녹색.

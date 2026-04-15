# hft-types — SPEC

## 역할
레포 전체의 **값 타입 허브**. 다른 모든 crate 가 이걸 참조한다. 동작 로직은 가지지 않는다.

## 공개 API (Phase 1 완료 시점)

### Enum
- `ExchangeId { Binance, Gate, Bybit, Bitget, Okx }`
  - `fn as_str(&self) -> &'static str` — 로그·토픽에서 사용
  - `fn all() -> &'static [ExchangeId]` — 거래소 순회용
  - `Display`, `FromStr`, `serde` (rename_all = "lowercase")
- `DataRole { Primary, Secondary }` — Gate 처럼 한 거래소에서 두 feed 를 돌릴 때 구분
- `MarketEvent { BookTicker(BookTicker), Trade(Trade), WebBookTicker(BookTicker) }`
  - `fn topic_kind(&self) -> &'static str` — `hft-protocol::topics` 에 바로 넘김

### Struct
- `Symbol(Arc<str>)` — clone 이 cheap 해야 함. 생성은 `Symbol::from("BTC_USDT")`
- `BookTicker { symbol, bid_price, ask_price, bid_size, ask_size, event_time_ms, server_time_ms }`
- `Trade { symbol, price, size, trade_id, create_time_ms, server_time_ms, is_buyer_maker }`
- `OrderSide { Buy, Sell }`

### Allocator
- `#[global_allocator] static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;`
  - 서비스 바이너리는 `use hft_types::ALLOCATOR` 한 줄로 공유

## 계약 (다른 crate 와의)

- `hft-protocol` 은 이 crate 의 struct 를 wire format 으로 encode/decode
- `hft-exchange-api` 는 `MarketEvent` 를 emit 한다
- `services/*` 는 `ExchangeId`, `DataRole` 로 config 분기
- **이 crate 는 다른 crate 를 참조하지 않는다** (순환 방지). `serde` 만 의존

## Phase 1 TODO

1. `Symbol` 을 `String` → `Arc<str>` 로 교체. 생성자 `from(&str)`, `from(String)` 제공. `AsRef<str>`, `Deref<Target=str>`, `Hash`, `Eq`.
2. `BookTicker`, `Trade` 에 `#[repr(C, align(64))]`. 필드 순서는 `hft-protocol::wire` 의 바이트 레이아웃과 일치해야 함 (양쪽 이 SPEC 과 `wire.rs` 에 offset 명시).
3. `ExchangeId` 에 `as_str`, `all()` 구현. `match` exhaustive 로 강제.
4. `MarketEvent::topic_kind` 구현 — 문자열은 `hft-protocol::topics::MSG_*` 상수 재수출.
5. `ALLOCATOR` 전역 등록.
6. Unit tests:
   - `size_of::<BookTicker>()` / `size_of::<Trade>()` 가 wire size 와 맞는지 (필드 합 + padding)
   - serde round-trip
   - `ExchangeId::all()` 에 variant 전부 있는지 (macro 로 강제)

## Anti-jitter 체크
- `Clone` 이 cheap 해야 함 (`Symbol` 이 `Arc<str>` 인 이유)
- hot path 가 이 struct 를 serialize 할 때 heap alloc 이 있으면 안 됨. `#[repr(C)]` + 고정 크기라 encode 는 bytewise memcpy

## 완료 조건
- [ ] `cargo test -p hft-types` 통과
- [ ] `size_of::<BookTicker>() + overhead = wire::BOOK_TICKER_SIZE` 확인 (문서에 계산 주석)
- [ ] `Symbol::clone()` 이 ref count 증가만 하는지 확인 (dhat)

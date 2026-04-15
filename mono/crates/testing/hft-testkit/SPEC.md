# hft-testkit — SPEC

## 역할
통합 테스트를 위한 mock 과 harness. 다른 crate 가 단위 테스트 레벨에서도 이 crate 를 dev-dep 으로 사용.

## 공개 API

```rust
// Mock Feed
pub struct MockFeed {
    pub id: ExchangeId,
    pub role: DataRole,
    pub clock: Arc<MockClock>,
    pub script: Vec<ScheduledEvent>,    // 순서대로 emit 할 이벤트
}

pub struct ScheduledEvent {
    pub at_ms: i64,                     // clock.now_ms() 기준
    pub event: MarketEvent,
}

#[async_trait]
impl ExchangeFeed for MockFeed { /* 스크립트 replay */ }

// Warm-up
pub struct WarmUp { /* ... */ }
impl WarmUp {
    pub async fn run(emit: Emitter, clock: &MockClock, count: usize);
}

// Harness
#[macro_export]
macro_rules! pipeline_harness {
    ($body:expr) => { /* spawn worker+aggregator+sub+strategy in-process */ }
}

// Assertions
pub fn assert_p99_under(hdr: &Histogram<u64>, nanos: u64);
pub fn assert_p99_9_under(hdr: &Histogram<u64>, nanos: u64);

// Mock sinks
pub struct MockSink { pub sent: Arc<Mutex<Vec<(Vec<u8>, Vec<u8>)>>> }
impl EventSink for MockSink { /* ... */ }
```

## 계약
- `MockFeed` 는 script replay 중에도 `cancel` token 응답.
- `WarmUp::run` 은 realistic 한 distribution 으로 이벤트 주입 (스파이크 시뮬레이션).
- `pipeline_harness!` 매크로는 publisher + subscriber + strategy 를 한 프로세스에 띄움. ZMQ 는 `inproc://test-*` 사용.
- 테스트는 `MockClock` 으로 시간을 deterministic 하게 제어.

## Phase 1 TODO

1. `MockFeed` 구현 — `stream` 안에서 script 순회하며 `clock.advance_ms` + `emit` 호출.
2. `MockClock` 은 `hft-time` 재수출.
3. `WarmUp::run` — 1만 건 BookTicker 를 랜덤 간격 (jitter 포함) 으로 emit.
4. `pipeline_harness!` 매크로 — publisher/subscriber crate 의 main 을 library-fied 한 API 호출.
5. `assert_p99_under` / `assert_p99_9_under` — HDR histogram 받아 threshold 비교.
6. `MockSink` / `MockSource` — `hft-ipc` 의 trait 구현.

## 계약의 영향
이 crate 가 안정되면:
- 모든 crate 의 단위 테스트가 이 crate 하나만 dev-dep 으로 추가하면 됨
- publisher SPEC 의 "완료 조건" (100K events/s 주입) 이 이 crate 로 재현 가능

## 완료 조건
- [ ] `MockFeed` 로 `ExchangeFeed` 계약 테스트 통과
- [ ] `pipeline_harness!` 로 단일 테스트에서 E2E 시나리오 구성 가능
- [ ] `WarmUp` + `assert_p99_9_under(histogram, 2_000_000)` (2ms) 통과

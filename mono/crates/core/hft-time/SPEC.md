# hft-time — SPEC

## 역할
시간 읽기와 stage 별 지연 스탬프. **이 crate 경유 없이 `SystemTime`/`Instant` 를 직접 호출하는 코드는 review 에서 reject.**

## 공개 API

### Trait
```rust
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;        // wall clock epoch millis (QuestDB 타임스탬프, 토픽 로그)
    fn now_nanos(&self) -> u64;     // monotonic (latency 측정 전용)
}
```

### 구현체
- `SystemClock` — 프로덕션
- `MockClock` — 테스트. `AtomicI64` 로 now 제어, `advance(ms)` / `advance_nanos(ns)`

### Stamps
```rust
pub enum Stage {
    ExchangeServer, WsReceived, Serialized, Pushed,
    Published, Subscribed, Consumed,
}

pub struct LatencyStamps {
    pub exchange_server_ms: i64,
    pub ws_received_ms: i64,
    pub serialized_ms: i64,
    pub pushed_ms: i64,
    pub published_ms: i64,
    pub subscribed_ms: i64,
    pub consumed_ms: i64,
}

impl LatencyStamps {
    pub fn set(&mut self, stage: Stage, ms: i64);
    pub fn get(&self, stage: Stage) -> i64;
    pub fn diff_ms(&self, a: Stage, b: Stage) -> i64;   // b - a, 0-handling 주의
    pub fn total_ms(&self) -> Option<i64>;              // Consumed - ExchangeServer
}
```

## 계약
- `hft-protocol::wire` 가 `LatencyStamps` 의 메모리 레이아웃을 고정 오프셋으로 가져감.
  이 crate 는 `#[repr(C)]` 를 지켜야 하고, 필드 순서 변경 시 `wire.rs` 의 상수도 같이 변경.
- 모든 서비스는 `Arc<dyn Clock>` 을 DI. 테스트에서 MockClock 주입 가능해야 함.
- `hft-telemetry` 가 `now_nanos` 를 span timing 에 사용 (간접적으로 `tracing` 이 찍는 시각).

## Phase 1 TODO

1. `MockClock` 구현 — `AtomicI64` ms + `AtomicU64` nanos. `advance_ms(delta)`, `advance_nanos(delta)`.
2. `SystemClock::now_nanos` 현재 구현 (`Instant::now()` 기반) 을 유지하되, thread-safe 와 monotonic 둘 다 테스트로 보장.
3. `Stage` enum + `LatencyStamps` 의 `set/get/diff_ms` 구현.
4. `#[repr(C)]` 확정 + `size_of` assertion 테스트 (`wire.rs` 의 `LATENCY_STAMPS_SIZE` 상수와 일치).
5. `fn since_ms(&self, epoch: i64) -> i64` 헬퍼 — subscriber 쪽에서 `now - stage_ms` 계산용.

## Anti-jitter 체크
- `now_ms` 는 syscall 1회 (`clock_gettime`) → 5~20ns. 허용.
- `now_nanos` 는 `Instant::now() - START` → 5~10ns. 허용.
- `LatencyStamps` 는 stack 또는 wire payload 안에서만 전달. 별도 heap alloc 없음.
- `Arc<dyn Clock>` 의 vtable 1회 간접호출은 p99.9 에 영향 무시 가능 (∼1ns).

## 완료 조건
- [ ] `MockClock::advance_ms(5); assert_eq!(stamps.diff_ms(A,B), 5)` 테스트 통과
- [ ] `SystemClock::now_nanos()` 가 1M 호출 동안 monotonic 검증
- [ ] `size_of::<LatencyStamps>()` = 56 (i64 × 7)

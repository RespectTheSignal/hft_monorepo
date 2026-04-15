# hft-ipc — SPEC

## 역할
ZMQ, shm 위에 깔리는 **통합 transport 파사드**. 서비스 코드는 가능하면 `hft-ipc` 만 보고 `hft-zmq`/`hft-shm` 는 간접 사용.

Phase 1 에서는 얇게만. ZMQ path 를 그대로 노출하고 shm 은 구현 되는 대로 뒤에서 교체 가능한 구조로.

## 공개 API

```rust
pub trait EventSink: Send + Sync {
    fn try_send(&self, topic: &[u8], payload: &[u8]) -> SendOutcome;
}

pub trait EventSource: Send + Sync {
    fn try_recv(&mut self) -> Option<(Vec<u8>, Vec<u8>)>;
    fn drain(&mut self, cap: usize, out: &mut Vec<(Vec<u8>, Vec<u8>)>) -> usize;
}

pub struct ZmqSink(PushSocket);         // hft-zmq 래핑
pub struct ZmqSource(PullSocket);
pub struct ZmqPub(PubSocket);
pub struct ZmqSub(SubSocket);

impl EventSink for ZmqSink { /* ... */ }
impl EventSource for ZmqSource { /* ... */ }

// shm variant 는 Phase 2+ 에서
```

## 계약

- 서비스 바이너리는 `Box<dyn EventSink>` / `Box<dyn EventSource>` 로 주입받음 → 테스트에서 `MockSink` 로 교체.
- `try_send` 는 WouldBlock 시 drop (hft-zmq 규약과 동일).
- `drain` 은 cap 만큼 꺼내서 out 에 append. out 은 caller 가 준 재사용 버퍼.

## Phase 1 TODO

1. `EventSink` / `EventSource` trait 정의
2. `ZmqSink`, `ZmqSource`, `ZmqPub`, `ZmqSub` 구현 (hft-zmq 얇게 래핑)
3. `mock.rs` — `MockSink` / `MockSource` (test 전용, `cfg(test)` 또는 `testkit` feature)
4. 서비스 main 에서 `Box<dyn EventSink>` 로 받는 helper 함수 `fn connect_push_sink(ctx, cfg) -> Box<dyn EventSink>`

## Anti-jitter 체크
- trait object 호출 1회 간접 (vtable). 허용.
- `Vec<(Vec<u8>, Vec<u8>)>` 는 caller 가 재사용 → alloc 없음.

## 완료 조건
- [ ] `ZmqSink` 가 `EventSink` 로 써도 hft-zmq 직접 쓰는 것과 같은 성능 (벤치로 확인, diff < 5%)
- [ ] `MockSink` 로 단위 테스트에서 publisher 로직 검증 가능

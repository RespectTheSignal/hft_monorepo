# hft-zmq — SPEC

## 역할
ZMQ 래퍼. **전 서비스가 ZMQ 를 쓰는 유일한 경로.** HWM/LINGER/IMMEDIATE 설정은 여기서만.

## 공개 API

```rust
pub struct Context(zmq::Context);
impl Context {
    pub fn new() -> Self;
    pub fn push(&self, endpoint: &str, cfg: &ZmqConfig) -> anyhow::Result<PushSocket>;
    pub fn pull(&self, endpoint: &str, cfg: &ZmqConfig) -> anyhow::Result<PullSocket>;
    pub fn pub_(&self, endpoint: &str, cfg: &ZmqConfig) -> anyhow::Result<PubSocket>;
    pub fn sub(&self, endpoint: &str, topics: &[&[u8]], cfg: &ZmqConfig) -> anyhow::Result<SubSocket>;
}

pub enum SendOutcome { Sent, WouldBlock, Error(zmq::Error) }

pub struct PushSocket { /* ... */ }
impl PushSocket {
    /// topic 은 multipart 1st frame. non-blocking.
    pub fn send_dontwait(&self, topic: &[u8], payload: &[u8]) -> SendOutcome;
}

pub struct PubSocket { /* 동일 */ }

pub struct PullSocket { /* ... */ }
impl PullSocket {
    /// ms 단위 poll. timeout 넘으면 None.
    pub fn recv_timeout(&mut self, ms: i32) -> anyhow::Result<Option<(Vec<u8>, Vec<u8>)>>;
    /// 배치 drain. cap 만큼 꺼낼 때까지 (또는 WouldBlock).
    pub fn drain_batch(&mut self, cap: usize, out: &mut Vec<(Vec<u8>, Vec<u8>)>) -> usize;
}

pub struct SubSocket { /* recv_timeout, drain_batch 동일 */ }
```

## 설정 불변
- `ZMQ_SNDHWM`, `ZMQ_RCVHWM` = `cfg.hwm`
- `ZMQ_LINGER` = `cfg.linger_ms`
- `ZMQ_IMMEDIATE` = 1
- `ZMQ_TCP_KEEPALIVE` = 1, idle 30, intvl 10, cnt 3
- PULL 측은 `ZMQ_RCVTIMEO` 를 `recv_timeout` 에 맞춰 매 호출 set (또는 `zmq::poll` 사용)

## 계약
- `send_dontwait` 는 **절대 블로킹하지 않음**. HWM 초과 → `WouldBlock` 리턴.
  caller 는 이 경우 drop 하고 `hft_telemetry::zmq_dropped_inc` 호출. 절대 재시도 루프 금지 (hot path 버그).
- `drain_batch` 는 cap 만큼 한 번에 꺼냄. subscriber 의 주 소비 패턴.
- tokio 와 직접 통합 안 함. PULL 쪽은 dedicated std thread 에 박고 `std::sync::mpsc` 로 ringbuf 에 전달 → tokio task 가 ringbuf 에서 drain. (또는 `tokio::io::unix::AsyncFd(socket.get_fd())` — Phase 1 에서는 전자 권장, 안정적)

## 전송 매체
- inproc: 같은 프로세스 내부 (worker ↔ aggregator)
- ipc: 같은 머신의 다른 프로세스 (publisher ↔ storage-svc)
- tcp: 다른 머신 (aggregator ↔ 원격 subscriber)

endpoint 문자열은 `ZmqConfig` 에서 받음. 이 crate 는 검증 안 함.

## Phase 1 TODO

1. `Context` + 4개 socket wrapper 구현.
2. `send_dontwait` — `zmq::SNDMORE | zmq::DONTWAIT` 로 multipart send, WouldBlock 매핑.
3. `drain_batch` — `recv` 를 `DONTWAIT` 으로 반복, WouldBlock 이면 종료.
4. subscription: `SubSocket::new` 에서 topics 를 `set_subscribe` 로 각각 등록.
5. fd 노출 API (`fn raw_fd(&self) -> RawFd`) — latency-probe / future async bridge 용.
6. 내부 metric 호출: send 성공 / WouldBlock / Error 카운트.
7. stress test: inproc PUSH-PULL 로 100K msg/s, drop 0, p99.9 < 100μs.

## Anti-jitter 체크
- `send_dontwait` 는 blocking 분기 제거됨. 재확인.
- `drain_batch` 의 `cap` 을 너무 크게 잡으면 한 번의 drain 이 길어져 다른 task 를 굶길 수 있음. cap=512 권장 (config).
- `zmq::Context` 는 프로세스당 1개. 여러 개 만들면 io thread 가 중복 → jitter.
- `Socket::bind` / `connect` 는 startup 에 1회.

## 완료 조건
- [ ] inproc 100K msg/s 스트레스 테스트 drop 0, p99.9 send < 100μs
- [ ] ipc PUSH → 다른 프로세스 PULL drain_batch(512) 정상 동작
- [ ] HWM 초과 시 `WouldBlock` 리턴 + metric 증가 동시 발생 테스트

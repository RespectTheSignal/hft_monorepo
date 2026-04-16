//! hft-ipc — ZMQ 위에 얇게 깔린 통합 transport 파사드.
//!
//! ## 설계
//!
//! - 서비스 코드는 가능한 한 `hft-ipc` 의 trait 만 의존하고, 내부 구현체는
//!   `hft-zmq` (또는 Phase 2 의 `hft-shm`) 로 교체되도록 설계.
//! - `EventSink` / `EventSource` 는 object-safe 하며, `Box<dyn EventSink>`
//!   주입으로 테스트 대체 (mock) 가 가능.
//! - hot path 에서 allocation 을 최소화하기 위해 `drain` 은 caller 가
//!   제공한 `Vec` 을 재사용한다 — worker 는 한 번 할당한 batch buffer 를
//!   `cap=1024` 정도로 유지.
//!
//! ## 계약
//!
//! - `EventSink::try_send` 는 non-blocking. HWM 초과 시 `WouldBlock` 리턴
//!   (hft-zmq 와 동일 규약).
//! - `EventSource::try_recv` 는 timeout 0 기반 — 이벤트 없으면 `None`.
//! - `EventSource::drain` 은 batched — `cap` 건까지 pull 하며 실제 pull 한 건수 반환.
//! - `ZmqSink`/`ZmqSource` 는 내부 ZMQ 소켓 소유 — drop 시 `LINGER` 설정에 따라 flush.
//!
//! ## Phase 1 TODO
//!
//! 1. `EventSink` / `EventSource` trait ✅
//! 2. `ZmqPushSink` / `ZmqPubSink` / `ZmqPullSource` / `ZmqSubSource` 구현 ✅
//! 3. `MockSink` / `MockSource` — `hft-testkit` 에서 이 trait 을 직접 구현한 타입이
//!    이미 있으므로 여기서 중복 제공하지 않음 (cfg feature 분리 대신).
//! 4. helper: `connect_push_sink(ctx, cfg, endpoint)` 등. ✅
//!
//! ## Anti-jitter
//!
//! - trait object 1회 vtable — 허용 (hot path 는 PushSocket::send_dontwait 직호출도 가능).
//! - `drain_into` 는 Vec 을 외부에서 재사용해 allocation 0회.
//! - WouldBlock 은 drop 으로 처리 — hft-zmq 의 counter 가 이미 inc 되었으므로 재카운트 금지.

#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]

use hft_config::ZmqConfig;
use hft_zmq::{
    Context, PubSocket, PullSocket, PushSocket, SendOutcome, SubSocket, TopicPayload, ZmqResult,
};
use parking_lot::Mutex;

// 재수출 — 사용자가 이 crate 만 의존해도 공통 타입 접근 가능.
pub use hft_zmq::{SendOutcome as IpcSendOutcome, TopicPayload as IpcTopicPayload};

// ────────────────────────────────────────────────────────────────────────────
// Trait
// ────────────────────────────────────────────────────────────────────────────

/// 이벤트 송신 쪽 추상. `try_send` 는 non-blocking.
///
/// 구현체는 내부적으로 `&self` 로 호출되도록 interior mutability 를 써도 되고,
/// PUSH/PUB 처럼 zmq 자체가 thread-safe 한 경우는 그대로 공유해도 된다.
pub trait EventSink: Send + Sync {
    /// 멀티파트(topic, payload) non-blocking send.
    fn try_send(&self, topic: &[u8], payload: &[u8]) -> SendOutcome;

    /// 구현별 디버그 라벨.
    fn label(&self) -> &'static str {
        "ipc-sink"
    }
}

/// 이벤트 수신 쪽 추상.
///
/// `&mut self` 인 이유: PULL/SUB 소켓은 recv 가 상태를 바꿀 수 있음 (multipart
/// 중간 상태 등). mutex 로 감싸면 &self 로도 가능하지만 hot path 에서 잠금
/// 비용을 피하려고 `&mut`.
pub trait EventSource: Send {
    /// ms 단위 timeout 으로 1건 recv. timeout 이면 `Ok(None)`.
    fn recv_timeout(&mut self, timeout_ms: i32) -> ZmqResult<Option<TopicPayload>>;

    /// 이벤트 없으면 None, 있으면 1건. (timeout 0 기반)
    fn try_recv(&mut self) -> ZmqResult<Option<TopicPayload>> {
        self.recv_timeout(0)
    }

    /// 최대 `cap` 건을 `out` 에 append. 실제 pull 한 건수 반환.
    fn drain_into(&mut self, cap: usize, out: &mut Vec<TopicPayload>) -> usize;

    fn label(&self) -> &'static str {
        "ipc-source"
    }
}

// ────────────────────────────────────────────────────────────────────────────
// ZMQ-backed sink/source — PUSH / PUB / PULL / SUB 각 1종
// ────────────────────────────────────────────────────────────────────────────

/// PUSH 소켓 sink. PUSH 는 zmq 내부적으로 thread-safe 하지 않아서
/// hft-zmq 의 `send_dontwait(&self, ...)` 는 `&self` 로 호출 가능하지만
/// 같은 소켓을 여러 스레드에서 concurrent 호출하는 건 zmq-rs 가 금함.
/// 따라서 실제로 multi-worker 가 같은 endpoint 로 push 할 땐 각자 개별
/// PUSH 소켓을 들게 하고, 여기서는 `Mutex` 로 방어만 해두는 수준으로.
pub struct ZmqPushSink {
    inner: Mutex<PushSocket>,
}

impl ZmqPushSink {
    pub fn new(sock: PushSocket) -> Self {
        Self { inner: Mutex::new(sock) }
    }

    /// 내부 lock 해제 후 소유권 반환 (close 등).
    pub fn into_inner(self) -> PushSocket {
        self.inner.into_inner()
    }
}

impl EventSink for ZmqPushSink {
    #[inline]
    fn try_send(&self, topic: &[u8], payload: &[u8]) -> SendOutcome {
        self.inner.lock().send_dontwait(topic, payload)
    }

    fn label(&self) -> &'static str {
        "zmq-push-sink"
    }
}

/// PUB 소켓 sink. aggregator → 외부 subscribers.
pub struct ZmqPubSink {
    inner: Mutex<PubSocket>,
}

impl ZmqPubSink {
    pub fn new(sock: PubSocket) -> Self {
        Self { inner: Mutex::new(sock) }
    }

    pub fn into_inner(self) -> PubSocket {
        self.inner.into_inner()
    }
}

impl EventSink for ZmqPubSink {
    #[inline]
    fn try_send(&self, topic: &[u8], payload: &[u8]) -> SendOutcome {
        self.inner.lock().send_dontwait(topic, payload)
    }

    fn label(&self) -> &'static str {
        "zmq-pub-sink"
    }
}

/// PULL source — aggregator 가 worker 로부터 수신.
pub struct ZmqPullSource {
    inner: PullSocket,
}

impl ZmqPullSource {
    pub fn new(sock: PullSocket) -> Self {
        Self { inner: sock }
    }

    pub fn into_inner(self) -> PullSocket {
        self.inner
    }
}

impl EventSource for ZmqPullSource {
    fn recv_timeout(&mut self, timeout_ms: i32) -> ZmqResult<Option<TopicPayload>> {
        self.inner.recv_timeout(timeout_ms)
    }

    fn drain_into(&mut self, cap: usize, out: &mut Vec<TopicPayload>) -> usize {
        self.inner.drain_batch(cap, out)
    }

    fn label(&self) -> &'static str {
        "zmq-pull-source"
    }
}

/// SUB source — subscriber 쪽 전용.
pub struct ZmqSubSource {
    inner: SubSocket,
}

impl ZmqSubSource {
    pub fn new(sock: SubSocket) -> Self {
        Self { inner: sock }
    }

    pub fn subscribe(&self, topic: &[u8]) -> ZmqResult<()> {
        self.inner.subscribe(topic)
    }

    pub fn unsubscribe(&self, topic: &[u8]) -> ZmqResult<()> {
        self.inner.unsubscribe(topic)
    }

    pub fn into_inner(self) -> SubSocket {
        self.inner
    }
}

impl EventSource for ZmqSubSource {
    fn recv_timeout(&mut self, timeout_ms: i32) -> ZmqResult<Option<TopicPayload>> {
        self.inner.recv_timeout(timeout_ms)
    }

    fn drain_into(&mut self, cap: usize, out: &mut Vec<TopicPayload>) -> usize {
        self.inner.drain_batch(cap, out)
    }

    fn label(&self) -> &'static str {
        "zmq-sub-source"
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers — 서비스 main 에서 쓰는 팩토리
// ────────────────────────────────────────────────────────────────────────────

/// PUSH 를 `connect` 모드로 열어 Boxed sink 로 반환.
pub fn connect_push_sink(
    ctx: &Context,
    endpoint: &str,
    cfg: &ZmqConfig,
) -> ZmqResult<Box<dyn EventSink>> {
    let sock = ctx.push(endpoint, cfg)?;
    Ok(Box::new(ZmqPushSink::new(sock)))
}

/// PUSH 를 `bind` 모드로 열어 sink 로 반환 (reverse topology).
pub fn bind_push_sink(
    ctx: &Context,
    endpoint: &str,
    cfg: &ZmqConfig,
) -> ZmqResult<Box<dyn EventSink>> {
    let sock = ctx.push_bind(endpoint, cfg)?;
    Ok(Box::new(ZmqPushSink::new(sock)))
}

/// PULL 을 `bind` 로 열어 Boxed source 로 반환. aggregator 의 기본 수신 패턴.
pub fn bind_pull_source(
    ctx: &Context,
    endpoint: &str,
    cfg: &ZmqConfig,
) -> ZmqResult<Box<dyn EventSource>> {
    let sock = ctx.pull(endpoint, cfg)?;
    Ok(Box::new(ZmqPullSource::new(sock)))
}

/// PULL 을 `connect` 로 여는 변형.
pub fn connect_pull_source(
    ctx: &Context,
    endpoint: &str,
    cfg: &ZmqConfig,
) -> ZmqResult<Box<dyn EventSource>> {
    let sock = ctx.pull_connect(endpoint, cfg)?;
    Ok(Box::new(ZmqPullSource::new(sock)))
}

/// PUB 소켓을 `bind` 로 열어 sink 로 반환. aggregator → 외부 경로.
pub fn bind_pub_sink(
    ctx: &Context,
    endpoint: &str,
    cfg: &ZmqConfig,
) -> ZmqResult<Box<dyn EventSink>> {
    let sock = ctx.pub_(endpoint, cfg)?;
    Ok(Box::new(ZmqPubSink::new(sock)))
}

/// SUB 를 `connect` 로 열어 source 로 반환. 토픽 prefix 리스트 구독.
pub fn connect_sub_source(
    ctx: &Context,
    endpoint: &str,
    topics: &[&[u8]],
    cfg: &ZmqConfig,
) -> ZmqResult<Box<dyn EventSource>> {
    let sock = ctx.sub(endpoint, topics, cfg)?;
    Ok(Box::new(ZmqSubSource::new(sock)))
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    // inproc 엔드포인트가 테스트 간 충돌하지 않도록 고유화.
    static EP_COUNTER: AtomicU32 = AtomicU32::new(0);
    fn next_ep(tag: &str) -> String {
        let n = EP_COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("inproc://hft-ipc-test-{}-{}", tag, n)
    }

    fn test_cfg() -> ZmqConfig {
        ZmqConfig {
            hwm: 1024,
            linger_ms: 0,
            drain_batch_cap: 64,
            push_endpoint: String::new(),
            pub_endpoint: String::new(),
            sub_endpoint: String::new(),
            order_ingress_bind: None,
        }
    }

    #[test]
    fn push_pull_roundtrip_via_trait() {
        let ctx = Context::new();
        let ep = next_ep("pp");
        let cfg = test_cfg();

        let mut src = bind_pull_source(&ctx, &ep, &cfg).unwrap();
        let sink = connect_push_sink(&ctx, &ep, &cfg).unwrap();

        // 첫 send 는 아직 connection 이 fully established 되기 전이라 실패할 수도 있어 루프.
        let mut sent = false;
        for _ in 0..50 {
            match sink.try_send(b"topic.a", b"hello") {
                SendOutcome::Sent => {
                    sent = true;
                    break;
                }
                SendOutcome::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                SendOutcome::Error(e) => panic!("send failed: {e:?}"),
            }
        }
        assert!(sent, "send should eventually succeed");

        // recv.
        let got = src.recv_timeout(200).unwrap();
        let (topic, payload) = got.expect("message should arrive");
        assert_eq!(topic, b"topic.a");
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn drain_into_accumulates_multiple() {
        let ctx = Context::new();
        let ep = next_ep("drain");
        let cfg = test_cfg();

        let mut src = bind_pull_source(&ctx, &ep, &cfg).unwrap();
        let sink = connect_push_sink(&ctx, &ep, &cfg).unwrap();

        // 5개 send, 충분히 재시도.
        let mut total = 0;
        for i in 0..5 {
            for _ in 0..50 {
                match sink.try_send(b"t", format!("m{i}").as_bytes()) {
                    SendOutcome::Sent => {
                        total += 1;
                        break;
                    }
                    SendOutcome::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    SendOutcome::Error(e) => panic!("send: {e:?}"),
                }
            }
        }
        assert_eq!(total, 5);

        let mut buf: Vec<TopicPayload> = Vec::new();
        // 완전히 들어오길 잠깐 기다림.
        for _ in 0..20 {
            src.drain_into(64, &mut buf);
            if buf.len() >= 5 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(buf.len() >= 5, "expected 5, got {}", buf.len());
    }

    #[test]
    fn trait_objects_are_object_safe() {
        // 컴파일 타임 어서션: trait object 로 저장/주입 가능해야 한다.
        fn takes(_s: Box<dyn EventSink>, _r: Box<dyn EventSource>) {}

        let ctx = Context::new();
        let cfg = test_cfg();
        let ep = next_ep("obj");
        let src = bind_pull_source(&ctx, &ep, &cfg).unwrap();
        let sink = connect_push_sink(&ctx, &ep, &cfg).unwrap();
        takes(sink, src);
    }

    #[test]
    fn recv_timeout_returns_none_when_empty() {
        let ctx = Context::new();
        let cfg = test_cfg();
        let ep = next_ep("empty");
        let mut src = bind_pull_source(&ctx, &ep, &cfg).unwrap();
        let got = src.recv_timeout(5).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn pub_sub_roundtrip_with_topic_filter() {
        let ctx = Context::new();
        let cfg = test_cfg();
        let ep = next_ep("pubsub");

        let pub_sink = bind_pub_sink(&ctx, &ep, &cfg).unwrap();
        let mut sub_src = connect_sub_source(&ctx, &ep, &[b"ok."], &cfg).unwrap();

        // PUB/SUB 는 slow joiner 이슈가 있어서 초기 메시지가 drop 될 수 있음.
        // 몇 ms 대기 후 여러 번 송신.
        std::thread::sleep(std::time::Duration::from_millis(100));

        let mut any_ok = false;
        for _ in 0..30 {
            // skip 프리픽스 메시지.
            let _ = pub_sink.try_send(b"skip.xxx", b"ignored");
            let _ = pub_sink.try_send(b"ok.channel", b"payload");
            if let Some((t, p)) = sub_src.recv_timeout(50).unwrap_or(None) {
                assert_eq!(t, b"ok.channel");
                assert_eq!(p, b"payload");
                any_ok = true;
                break;
            }
        }
        assert!(any_ok, "expected to receive at least one ok.* message");
    }

    #[test]
    fn labels_are_descriptive() {
        let ctx = Context::new();
        let cfg = test_cfg();
        let ep = next_ep("label");
        let src = bind_pull_source(&ctx, &ep, &cfg).unwrap();
        let sink = connect_push_sink(&ctx, &ep, &cfg).unwrap();
        assert!(src.label().contains("pull"));
        assert!(sink.label().contains("push"));
    }
}

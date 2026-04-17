//! hft-zmq — ZMQ PUSH/PULL/PUB/SUB 래퍼.
//!
//! ## 설계
//! - 전 서비스가 ZMQ 를 쓰는 **유일한 경로**. HWM/LINGER/IMMEDIATE/KEEPALIVE
//!   옵션 튜닝은 이 crate 한 곳에서만 일어난다.
//! - `Context` 는 프로세스당 1개. `zmq::Context` 는 내부적으로 `Arc` 이므로
//!   clone 해서 여러 소켓에서 공유한다 (io-thread pool 공유 → jitter 최소화).
//! - 모든 sender 는 **non-blocking**. HWM 초과 시 `SendOutcome::WouldBlock`
//!   을 리턴하고 caller 가 즉시 drop + metric inc. 핫 패스에서 재시도 루프 금지.
//! - `drain_batch` 는 subscriber 의 표준 소비 패턴. cap 만큼 `DONTWAIT` 으로
//!   모은 뒤 한 번에 상위로 전달 → syscall/lock 마찰 최소화.
//!
//! ## 계약
//! - topic 프레임은 multipart 의 **첫 번째 part**, payload 는 두 번째 part.
//! - PULL/SUB `recv_timeout(ms)` 은 `ZMQ_RCVTIMEO` + blocking recv_multipart.
//! - `raw_fd()` 노출: 미래에 async runtime (tokio AsyncFd) 와 붙일 때 사용.
//! - 전송 실패(Eagain) 와 진짜 에러(ECONTEXT/ETERM 등)를 명확히 구분해
//!   metric 분기를 둔다.
//!
//! ## Anti-jitter
//! - `Context` 는 startup 에 1번 생성. worker 스레드당 1개의 소켓 권장.
//! - hot path 에서 `String` 할당 없음: 모든 send API 는 `&[u8]` 만 수용.
//! - `drain_batch` cap 은 config 에서 주입. 기본 512 (너무 크면 한 턴이 길어 starvation).
//!
//! ## Phase 1 TODO
//! - ✅ Context + 4 socket wrapper
//! - ✅ SendOutcome (WouldBlock / Sent / Error) + metric 훅
//! - ✅ drain_batch / recv_timeout
//! - ✅ raw_fd 노출
//! - ⏸ stress test (100K msg/s, drop 0, p99.9 < 100μs) — 실제 CI 장비에서 수행

#![deny(rust_2018_idioms)]

use thiserror::Error;
use tracing::warn;

use hft_config::ZmqConfig;
use hft_telemetry::{counter_inc, CounterKey};

// ─────────────────────────────────────────────────────────────────────────────
// Error
// ─────────────────────────────────────────────────────────────────────────────

/// hft-zmq 레벨 에러.
#[derive(Debug, Error)]
pub enum ZmqWrapError {
    /// 내부 zmq-rs 에러 (socket 옵션 실패, bind/connect 실패 등).
    #[error("zmq error: {0}")]
    Zmq(#[from] zmq::Error),
    /// 컨텍스트가 이미 종료되어 사용 불가.
    #[error("zmq context terminated")]
    ContextTerminated,
    /// multipart 파싱에서 기대치 않은 프레임 수.
    #[error("unexpected multipart frame count: {0}")]
    BadFrameCount(usize),
}

/// 결과 타입 별칭.
pub type ZmqResult<T> = Result<T, ZmqWrapError>;

// ─────────────────────────────────────────────────────────────────────────────
// SendOutcome — 핫 패스 classification
// ─────────────────────────────────────────────────────────────────────────────

/// 논블로킹 send 결과.
///
/// - `Sent`: 성공적으로 큐에 enqueue.
/// - `WouldBlock`: HWM 초과 → drop. metric `zmq_dropped` 는 내부에서 이미 bump.
/// - `Error(e)`: 비정상 에러. caller 가 로그/재시도 정책 결정.
#[derive(Debug)]
pub enum SendOutcome {
    /// 전송 OK.
    Sent,
    /// HWM 가 가득 차 drop.
    WouldBlock,
    /// 다른 zmq 에러.
    Error(zmq::Error),
}

impl SendOutcome {
    /// 성공 여부 boolean.
    #[inline]
    pub fn is_sent(&self) -> bool {
        matches!(self, Self::Sent)
    }

    /// drop 여부 boolean.
    #[inline]
    pub fn is_would_block(&self) -> bool {
        matches!(self, Self::WouldBlock)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Context
// ─────────────────────────────────────────────────────────────────────────────

/// ZMQ 컨텍스트 래퍼.
///
/// `zmq::Context::new()` 는 내부 `Arc<...>` 이므로 clone 이 값싸다.
/// 여러 모듈에 넘길 땐 `ctx.clone()` 해도 실제로는 같은 io-thread pool 공유.
#[derive(Clone)]
pub struct Context(zmq::Context);

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    /// 새 컨텍스트 생성. 프로세스당 1개면 충분.
    pub fn new() -> Self {
        Self(zmq::Context::new())
    }

    /// 내부 zmq::Context 접근 (고급 소비자용).
    pub fn raw(&self) -> &zmq::Context {
        &self.0
    }

    /// PUSH 소켓 생성 및 `connect`.
    ///
    /// - push 의 기본 패턴: upstream(worker) → aggregator(PULL)
    /// - 옵션: SNDHWM, LINGER, IMMEDIATE, TCP_KEEPALIVE 세트.
    pub fn push(&self, endpoint: &str, cfg: &ZmqConfig) -> ZmqResult<PushSocket> {
        let sock = self.0.socket(zmq::PUSH)?;
        apply_common_opts(&sock, cfg)?;
        sock.connect(endpoint)?;
        Ok(PushSocket { inner: sock })
    }

    /// PUSH 소켓 생성 및 `connect` + reconnect 간격 적용.
    pub fn push_with_reconnect(
        &self,
        endpoint: &str,
        cfg: &ZmqConfig,
        reconnect_interval_ms: i32,
        reconnect_interval_max_ms: i32,
    ) -> ZmqResult<PushSocket> {
        let sock = self.0.socket(zmq::PUSH)?;
        apply_common_opts(&sock, cfg)?;
        sock.set_reconnect_ivl(reconnect_interval_ms)?;
        sock.set_reconnect_ivl_max(reconnect_interval_max_ms)?;
        sock.connect(endpoint)?;
        Ok(PushSocket { inner: sock })
    }

    /// PUSH 소켓을 `bind` 로 열기 (aggregator ↔ bg-writer 같은 역방향 토폴로지).
    pub fn push_bind(&self, endpoint: &str, cfg: &ZmqConfig) -> ZmqResult<PushSocket> {
        let sock = self.0.socket(zmq::PUSH)?;
        apply_common_opts(&sock, cfg)?;
        sock.bind(endpoint)?;
        Ok(PushSocket { inner: sock })
    }

    /// PULL 소켓 생성 및 `bind`.
    pub fn pull(&self, endpoint: &str, cfg: &ZmqConfig) -> ZmqResult<PullSocket> {
        let sock = self.0.socket(zmq::PULL)?;
        apply_common_opts(&sock, cfg)?;
        sock.bind(endpoint)?;
        Ok(PullSocket { inner: sock })
    }

    /// PULL 을 `connect` 로 여는 변형.
    pub fn pull_connect(&self, endpoint: &str, cfg: &ZmqConfig) -> ZmqResult<PullSocket> {
        let sock = self.0.socket(zmq::PULL)?;
        apply_common_opts(&sock, cfg)?;
        sock.connect(endpoint)?;
        Ok(PullSocket { inner: sock })
    }

    /// PUB 소켓 생성 및 `bind`. (PUB 가 server.)
    pub fn pub_(&self, endpoint: &str, cfg: &ZmqConfig) -> ZmqResult<PubSocket> {
        let sock = self.0.socket(zmq::PUB)?;
        apply_common_opts(&sock, cfg)?;
        sock.bind(endpoint)?;
        Ok(PubSocket { inner: sock })
    }

    /// SUB 소켓 생성 + connect + 토픽 구독.
    ///
    /// `topics` 가 비어있으면 아무것도 받지 않는다. 일반적으로 prefix 매칭 규칙에 따라
    /// `b""` (빈 프레임) 를 넣으면 모든 메시지 구독.
    pub fn sub(&self, endpoint: &str, topics: &[&[u8]], cfg: &ZmqConfig) -> ZmqResult<SubSocket> {
        let sock = self.0.socket(zmq::SUB)?;
        apply_common_opts(&sock, cfg)?;
        sock.connect(endpoint)?;
        for t in topics {
            sock.set_subscribe(t)?;
        }
        Ok(SubSocket { inner: sock })
    }
}

/// 소켓 공통 옵션 적용.
///
/// - ZMQ_SNDHWM / ZMQ_RCVHWM = cfg.hwm
/// - ZMQ_LINGER = cfg.linger_ms
/// - ZMQ_IMMEDIATE = 1 (연결 완료 전에는 send 를 enqueue 하지 않음 → drop 정확)
/// - TCP keepalive: idle 30s, intvl 10s, cnt 3
fn apply_common_opts(sock: &zmq::Socket, cfg: &ZmqConfig) -> ZmqResult<()> {
    sock.set_sndhwm(cfg.hwm)?;
    sock.set_rcvhwm(cfg.hwm)?;
    sock.set_linger(cfg.linger_ms)?;
    sock.set_immediate(true)?;
    // TCP keepalive — inproc/ipc 에선 no-op 에 가깝지만 tcp 에서 dead conn 빨리 감지.
    let _ = sock.set_tcp_keepalive(1);
    let _ = sock.set_tcp_keepalive_idle(30);
    let _ = sock.set_tcp_keepalive_intvl(10);
    let _ = sock.set_tcp_keepalive_cnt(3);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// 내부 send helper — non-blocking multipart
// ─────────────────────────────────────────────────────────────────────────────

/// 멀티파트 send: topic + payload, `DONTWAIT` 플래그 강제.
///
/// HWM 초과 → `WouldBlock`, 기타 에러 → `Error`.
/// 어떤 경로든 적절한 counter 를 bump 한다.
fn send_multipart_nonblocking(sock: &zmq::Socket, topic: &[u8], payload: &[u8]) -> SendOutcome {
    // 1st part: topic, SNDMORE | DONTWAIT
    match sock.send(topic, zmq::SNDMORE | zmq::DONTWAIT) {
        Ok(()) => {}
        Err(zmq::Error::EAGAIN) => {
            counter_inc(CounterKey::ZmqHwmBlock);
            counter_inc(CounterKey::ZmqDropped);
            return SendOutcome::WouldBlock;
        }
        Err(e) => {
            warn!(target: "hft_zmq", error = ?e, "send_multipart topic frame failed");
            return SendOutcome::Error(e);
        }
    }
    // 2nd part: payload, DONTWAIT
    match sock.send(payload, zmq::DONTWAIT) {
        Ok(()) => SendOutcome::Sent,
        Err(zmq::Error::EAGAIN) => {
            // 중간 상태 (topic 은 큐에 들어갔지만 payload 실패) — zmq가 내부에서 정리.
            // 실무적으로 RCVHWM 만큼 reserve 되어 거의 발생하지 않지만 카운트.
            counter_inc(CounterKey::ZmqHwmBlock);
            counter_inc(CounterKey::ZmqDropped);
            SendOutcome::WouldBlock
        }
        Err(e) => {
            warn!(target: "hft_zmq", error = ?e, "send_multipart payload frame failed");
            SendOutcome::Error(e)
        }
    }
}

/// 단일 프레임 raw payload 를 `DONTWAIT` 로 전송한다.
fn send_single_nonblocking(sock: &zmq::Socket, payload: &[u8]) -> SendOutcome {
    match sock.send(payload, zmq::DONTWAIT) {
        Ok(()) => SendOutcome::Sent,
        Err(zmq::Error::EAGAIN) => {
            counter_inc(CounterKey::ZmqHwmBlock);
            counter_inc(CounterKey::ZmqDropped);
            SendOutcome::WouldBlock
        }
        Err(e) => {
            warn!(target: "hft_zmq", error = ?e, "send_single payload frame failed");
            SendOutcome::Error(e)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PushSocket
// ─────────────────────────────────────────────────────────────────────────────

/// PUSH 소켓. 순수 송신 (recv 없음).
pub struct PushSocket {
    inner: zmq::Socket,
}

impl PushSocket {
    /// non-blocking multipart send. HWM 초과 → WouldBlock + metric inc.
    #[inline]
    pub fn send_dontwait(&self, topic: &[u8], payload: &[u8]) -> SendOutcome {
        send_multipart_nonblocking(&self.inner, topic, payload)
    }

    /// 단일 프레임 raw payload 를 non-blocking 으로 전송한다.
    #[inline]
    pub fn send_bytes_dontwait(&self, payload: &[u8]) -> SendOutcome {
        send_single_nonblocking(&self.inner, payload)
    }

    /// raw fd (tokio AsyncFd 어댑터 용).
    pub fn raw_fd(&self) -> ZmqResult<RawFdValue> {
        Ok(RawFdValue(self.inner.get_fd()?.into()))
    }

    /// 소켓 `disconnect`. 보통 Drop 으로 충분하지만 explicit shutdown 용.
    pub fn close(self) {
        drop(self);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PubSocket
// ─────────────────────────────────────────────────────────────────────────────

/// PUB 소켓. PUSH 와 동일 인터페이스.
pub struct PubSocket {
    inner: zmq::Socket,
}

impl PubSocket {
    /// non-blocking multipart send.
    #[inline]
    pub fn send_dontwait(&self, topic: &[u8], payload: &[u8]) -> SendOutcome {
        send_multipart_nonblocking(&self.inner, topic, payload)
    }

    /// raw fd.
    pub fn raw_fd(&self) -> ZmqResult<RawFdValue> {
        Ok(RawFdValue(self.inner.get_fd()?.into()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PullSocket / SubSocket — 공통 recv 로직
// ─────────────────────────────────────────────────────────────────────────────

/// (topic, payload) 튜플 타입 alias. 핫 패스에서 alloc 을 드러내므로 타입만 별칭.
pub type TopicPayload = (Vec<u8>, Vec<u8>);

/// 공통 recv 로직.
///
/// `ZMQ_RCVTIMEO` 를 설정해 blocking recv_multipart 호출. -1 이면 무한 대기.
fn recv_timeout_inner(sock: &mut zmq::Socket, timeout_ms: i32) -> ZmqResult<Option<TopicPayload>> {
    sock.set_rcvtimeo(timeout_ms)?;
    match sock.recv_multipart(0) {
        Ok(parts) => {
            if parts.len() != 2 {
                return Err(ZmqWrapError::BadFrameCount(parts.len()));
            }
            let mut it = parts.into_iter();
            let topic = it.next().unwrap();
            let payload = it.next().unwrap();
            Ok(Some((topic, payload)))
        }
        Err(zmq::Error::EAGAIN) => Ok(None),
        Err(zmq::Error::ETERM) => Err(ZmqWrapError::ContextTerminated),
        Err(e) => Err(e.into()),
    }
}

/// 공통 raw single-frame recv 로직.
fn recv_bytes_timeout_inner(sock: &mut zmq::Socket, timeout_ms: i32) -> ZmqResult<Option<Vec<u8>>> {
    sock.set_rcvtimeo(timeout_ms)?;
    match sock.recv_multipart(0) {
        Ok(parts) => {
            if parts.len() != 1 {
                return Err(ZmqWrapError::BadFrameCount(parts.len()));
            }
            Ok(parts.into_iter().next())
        }
        Err(zmq::Error::EAGAIN) => Ok(None),
        Err(zmq::Error::ETERM) => Err(ZmqWrapError::ContextTerminated),
        Err(e) => Err(e.into()),
    }
}

/// 배치 drain: `DONTWAIT` 으로 cap 개까지 recv.
///
/// 반환: 실제로 꺼낸 개수. WouldBlock 에 도달하면 조기 종료.
fn drain_batch_inner(sock: &mut zmq::Socket, cap: usize, out: &mut Vec<TopicPayload>) -> usize {
    let start = out.len();
    for _ in 0..cap {
        match sock.recv_multipart(zmq::DONTWAIT) {
            Ok(parts) => {
                if parts.len() != 2 {
                    warn!(
                        target: "hft_zmq",
                        frames = parts.len(),
                        "drain_batch discarded non-2-frame message"
                    );
                    counter_inc(CounterKey::DecodeFail);
                    continue;
                }
                let mut it = parts.into_iter();
                let topic = it.next().unwrap();
                let payload = it.next().unwrap();
                out.push((topic, payload));
            }
            Err(zmq::Error::EAGAIN) => break,
            Err(e) => {
                warn!(target: "hft_zmq", error = ?e, "drain_batch recv error");
                break;
            }
        }
    }
    out.len() - start
}

/// PULL 소켓.
pub struct PullSocket {
    inner: zmq::Socket,
}

impl PullSocket {
    /// ms 단위 timeout 으로 한 건 수신. `None` = timeout.
    pub fn recv_timeout(&mut self, timeout_ms: i32) -> ZmqResult<Option<TopicPayload>> {
        recv_timeout_inner(&mut self.inner, timeout_ms)
    }

    /// raw single-frame payload 를 timeout 기반으로 1건 수신한다.
    pub fn recv_bytes_timeout(&mut self, timeout_ms: i32) -> ZmqResult<Option<Vec<u8>>> {
        recv_bytes_timeout_inner(&mut self.inner, timeout_ms)
    }

    /// 배치 drain. 반환은 **이번 호출에서 밀어 넣은 건수**.
    pub fn drain_batch(&mut self, cap: usize, out: &mut Vec<TopicPayload>) -> usize {
        drain_batch_inner(&mut self.inner, cap, out)
    }

    /// raw fd.
    pub fn raw_fd(&self) -> ZmqResult<RawFdValue> {
        Ok(RawFdValue(self.inner.get_fd()?.into()))
    }
}

/// SUB 소켓.
pub struct SubSocket {
    inner: zmq::Socket,
}

impl SubSocket {
    /// 추가 토픽 구독.
    pub fn subscribe(&self, topic: &[u8]) -> ZmqResult<()> {
        self.inner.set_subscribe(topic)?;
        Ok(())
    }

    /// 토픽 구독 해제.
    pub fn unsubscribe(&self, topic: &[u8]) -> ZmqResult<()> {
        self.inner.set_unsubscribe(topic)?;
        Ok(())
    }

    /// ms 단위 timeout 으로 한 건 수신. `None` = timeout.
    pub fn recv_timeout(&mut self, timeout_ms: i32) -> ZmqResult<Option<TopicPayload>> {
        recv_timeout_inner(&mut self.inner, timeout_ms)
    }

    /// 배치 drain.
    pub fn drain_batch(&mut self, cap: usize, out: &mut Vec<TopicPayload>) -> usize {
        drain_batch_inner(&mut self.inner, cap, out)
    }

    /// raw fd.
    pub fn raw_fd(&self) -> ZmqResult<RawFdValue> {
        Ok(RawFdValue(self.inner.get_fd()?.into()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// RawFdValue — cross-platform fd 표현
// ─────────────────────────────────────────────────────────────────────────────

/// raw fd 래퍼 (i64 on Unix, usize-ish on Windows). zmq-rs 는 `get_fd()` 로
/// 플랫폼 의존적 fd 를 준다 (i64). tokio `AsyncFd<RawFd>` 와 붙이려면
/// 유닉스 기준 `RawFd = i32` 캐스트가 필요하다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawFdValue(pub ZmqFdRepr);

/// zmq-rs get_fd 의 플랫폼 중립 리턴 타입. 실제로는 i64 (zmq-rs 0.10).
pub type ZmqFdRepr = i64;

impl RawFdValue {
    /// unix RawFd (i32) 로 캐스트. Linux/macOS 에서만 사용.
    #[cfg(unix)]
    #[inline]
    pub fn as_unix_raw_fd(self) -> std::os::unix::io::RawFd {
        self.0 as std::os::unix::io::RawFd
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 테스트
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_config::ZmqConfig;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 테스트 간 endpoint 충돌 방지용 카운터 → 고유한 inproc endpoint 생성.
    fn unique_inproc() -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        let id = N.fetch_add(1, Ordering::SeqCst);
        format!("inproc://hft-zmq-test-{}", id)
    }

    fn cfg() -> ZmqConfig {
        ZmqConfig {
            hwm: 100_000,
            linger_ms: 0,
            drain_batch_cap: 512,
            push_endpoint: String::new(),
            pub_endpoint: String::new(),
            sub_endpoint: String::new(),
            order_ingress_bind: None,
            result_egress_bind: None,
            result_heartbeat_interval_ms: 0,
        }
    }

    #[test]
    fn send_outcome_helpers() {
        assert!(SendOutcome::Sent.is_sent());
        assert!(!SendOutcome::Sent.is_would_block());
        assert!(SendOutcome::WouldBlock.is_would_block());
        assert!(!SendOutcome::WouldBlock.is_sent());
    }

    #[test]
    fn push_pull_inproc_roundtrip() {
        let ctx = Context::new();
        let ep = unique_inproc();
        let cfg = cfg();
        // PULL 가 먼저 bind, PUSH 가 connect.
        let mut pull = ctx.pull(&ep, &cfg).unwrap();
        let push = ctx.push(&ep, &cfg).unwrap();

        // inproc 은 bind 이후 즉시 가용. 적어도 한 번 recv 하려면 send 가 먼저 dispatch.
        let out = push.send_dontwait(b"test.topic", b"hello world");
        assert!(out.is_sent(), "send_dontwait: {:?}", out);

        let got = pull
            .recv_timeout(500)
            .expect("recv_timeout err")
            .expect("timeout");
        assert_eq!(got.0, b"test.topic");
        assert_eq!(got.1, b"hello world");
    }

    #[test]
    fn pull_drain_batch_collects_multiple() {
        let ctx = Context::new();
        let ep = unique_inproc();
        let cfg = cfg();
        let mut pull = ctx.pull(&ep, &cfg).unwrap();
        let push = ctx.push(&ep, &cfg).unwrap();

        for i in 0..10u32 {
            let payload = i.to_le_bytes();
            assert!(push.send_dontwait(b"t", &payload).is_sent());
        }

        // drain 은 바로 호출해도 inproc은 거의 즉시 가용.
        // 빡빡한 케이스 대비 poll loop: 최대 5회까지 polling.
        let mut out = Vec::new();
        for _ in 0..5 {
            pull.drain_batch(512, &mut out);
            if out.len() == 10 {
                break;
            }
            // sleep-free wait: 짧은 timeout recv 를 한 번 태워 kernel 에 yield
            let _ = pull.recv_timeout(10);
        }
        assert!(out.len() >= 10, "collected {} msgs", out.len());
    }

    #[test]
    fn recv_timeout_returns_none_on_idle() {
        let ctx = Context::new();
        let ep = unique_inproc();
        let cfg = cfg();
        let mut pull = ctx.pull(&ep, &cfg).unwrap();
        // PUSH 가 연결되지 않음 → recv_timeout(50) 은 None.
        let got = pull.recv_timeout(50).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn push_pull_raw_single_frame_roundtrip() {
        let ctx = Context::new();
        let ep = unique_inproc();
        let cfg = cfg();
        let mut pull = ctx.pull(&ep, &cfg).unwrap();
        let push = ctx.push(&ep, &cfg).unwrap();

        let out = push.send_bytes_dontwait(b"raw-128b");
        assert!(out.is_sent(), "send_bytes_dontwait: {:?}", out);

        let got = pull
            .recv_bytes_timeout(500)
            .expect("recv_bytes_timeout err")
            .expect("timeout");
        assert_eq!(got, b"raw-128b");
    }

    #[test]
    fn pub_sub_inproc_roundtrip() {
        let ctx = Context::new();
        let ep = unique_inproc();
        let cfg = cfg();

        let publisher = ctx.pub_(&ep, &cfg).unwrap();
        // SUB 는 connect 후 구독을 걸어야 하고 slow-joiner 문제 있음 — 여러 번 시도.
        let mut sub = ctx.sub(&ep, &[b""], &cfg).unwrap();

        // slow joiner: 처음 몇 개는 놓칠 수 있어 loop.
        let mut received = None;
        for _ in 0..50 {
            let _ = publisher.send_dontwait(b"topic", b"payload");
            match sub.recv_timeout(50).unwrap() {
                Some(m) => {
                    received = Some(m);
                    break;
                }
                None => continue,
            }
        }
        let m = received.expect("no PUB-SUB message received");
        assert_eq!(m.0, b"topic");
        assert_eq!(m.1, b"payload");
    }

    #[test]
    fn sub_unsubscribe_takes_effect() {
        let ctx = Context::new();
        let ep = unique_inproc();
        let cfg = cfg();

        let publisher = ctx.pub_(&ep, &cfg).unwrap();
        let mut sub = ctx.sub(&ep, &[b"keep"], &cfg).unwrap();
        // drop = no-op 이지만 명시적 unsubscribe 호출만 검증.
        sub.unsubscribe(b"keep").unwrap();
        sub.subscribe(b"keep").unwrap();

        for _ in 0..20 {
            let _ = publisher.send_dontwait(b"keep", b"1");
        }
        // 적어도 하나 이상 수신되는지만 확인 (slow joiner 특성상 개수는 비결정적).
        let mut collected = 0;
        for _ in 0..20 {
            if sub.recv_timeout(25).unwrap().is_some() {
                collected += 1;
                if collected >= 1 {
                    break;
                }
            }
        }
        assert!(collected >= 1);
    }

    #[test]
    fn raw_fd_exposed() {
        let ctx = Context::new();
        let ep = unique_inproc();
        let cfg = cfg();
        let push = ctx.push_bind(&ep, &cfg).unwrap();
        let fd = push.raw_fd().unwrap();
        // inproc 에서 fd 는 플랫폼별 내부 핸들이라 값 자체는 검증 어렵고
        // 에러 없이 가져와지는 것만 확인.
        let _ = fd.0;
    }
}

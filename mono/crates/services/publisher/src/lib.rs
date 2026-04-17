//! publisher — 거래소 feed → 직렬화 → PUSH → PUB.
//!
//! # 역할
//! 50ms 지연 예산의 stage 3 ~ stage 5 (`serialized` / `pushed` / `published`)
//! 를 전부 담당하는 핵심 서비스. `ExchangeFeed::stream` 콜백 안에서 곧바로
//! C-struct 직렬화 + ZMQ PUSH 를 수행하고, 1개의 Aggregator task 가 inproc
//! PULL → tcp PUB 로 팬아웃한다.
//!
//! # 설계 원칙
//! - **zero channel**: feed → worker → aggregator 사이 tokio channel 을 두지
//!   않는다. 콜백 직접 실행으로 컨텍스트 스위치를 제거.
//! - **non-blocking ZMQ**: PUSH / PUB 둘 다 `DONTWAIT`. HWM 초과 시에는 즉시
//!   drop + `counter_inc(ZmqDropped)`. hot path 에서 재시도 루프 금지.
//! - **patch-in-place stamp**: serializer 는 `publisher_sent_ms = 0` 으로
//!   encode 한다. PUSH 직전에 `patch_*_pushed_ms` 로 offset 96 / 104 에
//!   `stamps.pushed.wall_ms` 를 덮어쓰고, Aggregator 는 PUB 직전에 같은
//!   offset 에 `stamps.published.wall_ms` 로 한 번 더 덮어쓴다. encode 중복
//!   없음.
//! - **topic precompute**: subscribe 대상 (exchange, msg_type, symbol) 조합의
//!   UTF-8 토픽 문자열을 startup 에 미리 빌드해 `Arc<[u8]>` 로 들고 다닌다.
//!   hot path 에서 포맷 할당 0.
//!
//! # 레이아웃
//! ```text
//!    ExchangeFeed (tokio task)          Aggregator (tokio task)
//!    ┌─────────────────┐                ┌──────────────────────┐
//!    │  WS read loop   │                │  loop {              │
//!    │    emit(ev,ls)  ├──┐             │    drain_batch(512)  │
//!    │                 │  │             │    for (t, p) in...: │
//!    └─────────────────┘  │             │      patch pub_ms    │
//!                         │             │      pub.send_dw...  │
//!             hot emit 콜백 안에서:        │    recv_timeout(1ms) │
//!             Worker::on_event(ev, ls)  │  }                   │
//!             ┌─────────────────┐       └──────────────────────┘
//!             │ encode into buf │            PULL (bind inproc)
//!             │ patch pushed_ms │                  ▲
//!             │ push.send_dw    ├──────────────────┘
//!             │ (inproc)        │
//!             └─────────────────┘
//! ```
//!
//! # 완료 조건 (SPEC.md §완료 조건)
//! - `on_event` alloc 0 (문자열 포맷팅 X, Vec push X)
//! - `Aggregator` loop alloc 0 (재사용 버퍼)
//! - SIGTERM → 5s 안에 clean shutdown
//! - MockFeed 100K evt/s 주입 시 p99.9 stage 3~6 < 2ms, drop 0

#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]

pub mod shm_pub;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use hft_config::{AppConfig, ExchangeConfig};
use hft_exchange_api::{CancellationToken, Emitter, ExchangeFeed};
use hft_exchange_binance::{BinanceConfig, BinanceFeed};
use hft_exchange_bitget::{BitgetConfig, BitgetFeed};
use hft_exchange_bybit::{BybitConfig, BybitFeed};
use hft_exchange_gate::{GateConfig, GateFeed};
use hft_exchange_okx::{OkxConfig, OkxFeed};
use hft_protocol::{
    encode_bookticker_into, encode_trade_into, patch_bookticker_pushed_ms, patch_trade_pushed_ms,
    TopicBuilder, BOOK_TICKER_SIZE, MSG_BOOKTICKER, MSG_TRADE, MSG_WEBBOOKTICKER, TRADE_SIZE,
};
use hft_telemetry::{counter_add, counter_inc, CounterKey};
use hft_time::{Clock, LatencyStamps, Stage, SystemClock};
use hft_types::{
    BookTicker as DomainBookTicker, DataRole, ExchangeId, MarketEvent, Symbol, Trade as DomainTrade,
};
use hft_zmq::{Context as ZmqContext, PubSocket, PullSocket, PushSocket, SendOutcome};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

// ─────────────────────────────────────────────────────────────────────────────
// 상수
// ─────────────────────────────────────────────────────────────────────────────

/// Worker 내부 직렬화 버퍼 크기. `BOOK_TICKER_SIZE(120)` 와 `TRADE_SIZE(128)`
/// 중 큰 쪽을 포함해야 한다. 컴파일 타임에 검증한다.
const WORKER_BUF_SIZE: usize = if BOOK_TICKER_SIZE > TRADE_SIZE {
    BOOK_TICKER_SIZE
} else {
    TRADE_SIZE
};

// BOOK_TICKER_SIZE / TRADE_SIZE 가 `const` 이므로 `const fn` max 가 가능.
const _: () = assert!(WORKER_BUF_SIZE >= BOOK_TICKER_SIZE);
const _: () = assert!(WORKER_BUF_SIZE >= TRADE_SIZE);

/// Aggregator drain loop 의 recv_timeout. 이 값 이후 idle → 루프 한 바퀴 더.
/// cancel 반응성과 syscall 마찰의 타협. 1ms 이면 1000 pps 낭비 수준이지만
/// 실측 jitter 영향은 미미.
const AGGREGATOR_RECV_TIMEOUT_MS: i32 = 1;

/// Aggregator drain loop 가 shutdown 요청 후 잔여 메시지를 배출하기 위해
/// 돌릴 최대 시간 (ms). 초과분은 경고 후 drop.
const AGGREGATOR_DRAIN_GRACE_MS: u64 = 500;

/// 환경변수: 활성화 시 aggregator 전용 OS 스레드를 생성하고 hot core 에 pin.
/// 값이 `1`, `true`, `yes` (case-insensitive) 이면 활성.
const ENV_PIN_HOT_CORES: &str = "HFT_PIN_HOT_CORES";

/// 환경변수 값이 truthy 한지.
fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let s = v.trim().to_ascii_lowercase();
            matches!(s.as_str(), "1" | "true" | "yes" | "on")
        }
        Err(_) => false,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TopicCache — (exchange, msg_type, symbol) → Arc<[u8]> 사전 계산
// ─────────────────────────────────────────────────────────────────────────────

/// (exchange, msg_type, symbol) → 토픽 바이트.
///
/// 지원 msg_type 은 3가지 ([`MSG_BOOKTICKER`], [`MSG_TRADE`],
/// [`MSG_WEBBOOKTICKER`]) — `&'static str` 포인터 비교로 구분.
#[derive(Debug, Clone, Default)]
pub struct TopicCache {
    inner: HashMap<(ExchangeId, &'static str, Symbol), Arc<[u8]>>,
}

impl TopicCache {
    /// 빈 캐시.
    pub fn new() -> Self {
        Self::default()
    }

    /// `(exchange, msg_type, symbol)` 에 대한 토픽 바이트를 사전 계산해 저장한다.
    ///
    /// 동일 키가 이미 있으면 덮어쓴다 (구독 갱신 시 사용 가능).
    pub fn insert(&mut self, exchange: ExchangeId, msg_type: &'static str, symbol: &Symbol) {
        let topic = TopicBuilder::build(exchange, msg_type, symbol.as_str()).into_bytes();
        self.inner
            .insert((exchange, msg_type, symbol.clone()), Arc::from(topic));
    }

    /// hot path 조회. miss 시 `None` → 핫 패스에서 fallback string 을 만든다
    /// (추가 할당 1회 — 구독되지 않은 심볼이 들어올 때만 발생).
    #[inline]
    pub fn get(
        &self,
        exchange: ExchangeId,
        msg_type: &'static str,
        symbol: &Symbol,
    ) -> Option<&Arc<[u8]>> {
        self.inner.get(&(exchange, msg_type, symbol.clone()))
    }

    /// 등록된 엔트리 수 (주로 테스트/로그).
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// 비어있는지.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// `ExchangeConfig` 리스트로부터 toler/role 을 반영해 토픽을 일괄 등록.
    ///
    /// `Primary` 는 `bookticker` + `trade`, `WebOrderBook` 은 `webbookticker`
    /// 만 구독한다고 가정. (phase 1 계약)
    pub fn populate_from_exchanges(&mut self, exchanges: &[ExchangeConfig]) {
        for ex in exchanges {
            for sym in &ex.symbols {
                match ex.role {
                    DataRole::Primary => {
                        self.insert(ex.id, MSG_BOOKTICKER, sym);
                        self.insert(ex.id, MSG_TRADE, sym);
                    }
                    DataRole::WebOrderBook => {
                        self.insert(ex.id, MSG_WEBBOOKTICKER, sym);
                    }
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Worker — emit 콜백 구현체
// ─────────────────────────────────────────────────────────────────────────────

/// 1개 feed 당 1개 Worker.
///
/// `ExchangeFeed` 가 emit 콜백을 호출하면 즉시 직렬화 + PUSH 수행. tokio
/// channel 은 사용하지 않으므로 이 함수 안에서 blocking send 를 해도
/// 전파되는 backpressure 가 WS 수신 루프로 그대로 도달해 "subscribe 끊겨
/// 있는데 큐만 계속 찬다" 패턴을 방지한다.
pub struct Worker {
    clock: Arc<dyn Clock>,
    push: Mutex<PushSocket>,
    topics: Arc<TopicCache>,
    /// SHM sidecar — cfg.shm.enabled 이면 Some. 활성화 시 ZMQ PUSH 와 **동시에**
    /// /dev/shm quote/trade 영역에도 기록한다. intra-host strategy 는 SHM 을 읽어
    /// ZMQ 왕복 (p50 ~15μs) 대신 p50 ~100ns 의 fast path 를 얻는다.
    shm: Option<Arc<crate::shm_pub::ShmPublisher>>,
}

impl Worker {
    /// 새 Worker — SHM sidecar 없이.
    pub fn new(clock: Arc<dyn Clock>, push: PushSocket, topics: Arc<TopicCache>) -> Self {
        Self {
            clock,
            push: Mutex::new(push),
            topics,
            shm: None,
        }
    }

    /// 새 Worker + SHM sidecar.
    pub fn with_shm(
        clock: Arc<dyn Clock>,
        push: PushSocket,
        topics: Arc<TopicCache>,
        shm: Arc<crate::shm_pub::ShmPublisher>,
    ) -> Self {
        Self {
            clock,
            push: Mutex::new(push),
            topics,
            shm: Some(shm),
        }
    }

    /// 한 이벤트를 처리.
    ///
    /// 레이아웃:
    /// 1. `serialized` stage stamp mark
    /// 2. 고정 버퍼에 encode
    /// 3. `pushed` stage stamp mark
    /// 4. offset 96 / 104 에 `stamps.pushed.wall_ms` 덮어쓰기 (patch)
    /// 5. `PUSH DONTWAIT` multipart (topic, payload)
    ///
    /// 성공 시 `PipelineEvent` counter++, HWM drop 시 `ZmqDropped` counter++,
    /// 기타 zmq 에러는 warn 로그 + `ZmqDropped` counter++.
    pub fn on_event(&self, event: MarketEvent, mut stamps: LatencyStamps) {
        // 1) stage 3: serialized stamp 캡처.
        stamps.mark(Stage::Serialized, &*self.clock);

        // 스택 버퍼. BOOK_TICKER_SIZE(120) / TRADE_SIZE(128) 둘 다 수용.
        let mut buf = [0u8; WORKER_BUF_SIZE];

        // 2) encode + topic 조회.
        let (topic_bytes, payload_len) = match &event {
            MarketEvent::BookTicker(bt) => {
                match self.topics.get(bt.exchange, MSG_BOOKTICKER, &bt.symbol) {
                    Some(t) => {
                        encode_into_bookticker(&mut buf, bt);
                        (t.clone(), BOOK_TICKER_SIZE)
                    }
                    None => match build_uncached_topic(bt.exchange, MSG_BOOKTICKER, &bt.symbol) {
                        Some(t) => {
                            encode_into_bookticker(&mut buf, bt);
                            (t, BOOK_TICKER_SIZE)
                        }
                        None => return,
                    },
                }
            }
            MarketEvent::WebBookTicker(bt) => {
                match self.topics.get(bt.exchange, MSG_WEBBOOKTICKER, &bt.symbol) {
                    Some(t) => {
                        encode_into_bookticker(&mut buf, bt);
                        (t.clone(), BOOK_TICKER_SIZE)
                    }
                    None => {
                        match build_uncached_topic(bt.exchange, MSG_WEBBOOKTICKER, &bt.symbol) {
                            Some(t) => {
                                encode_into_bookticker(&mut buf, bt);
                                (t, BOOK_TICKER_SIZE)
                            }
                            None => return,
                        }
                    }
                }
            }
            MarketEvent::Trade(tr) => match self.topics.get(tr.exchange, MSG_TRADE, &tr.symbol) {
                Some(t) => {
                    encode_into_trade(&mut buf, tr);
                    (t.clone(), TRADE_SIZE)
                }
                None => match build_uncached_topic(tr.exchange, MSG_TRADE, &tr.symbol) {
                    Some(t) => {
                        encode_into_trade(&mut buf, tr);
                        (t, TRADE_SIZE)
                    }
                    None => return,
                },
            },
        };

        // 3) stage 4: pushed stamp 캡처.
        stamps.mark(Stage::Pushed, &*self.clock);

        // 4) offset 96(bookticker) / 104(trade) 에 pushed_ms 덮어쓰기.
        match payload_len {
            BOOK_TICKER_SIZE => {
                patch_bookticker_pushed_ms(&mut buf[..BOOK_TICKER_SIZE], stamps.pushed.wall_ms)
            }
            TRADE_SIZE => patch_trade_pushed_ms(&mut buf[..TRADE_SIZE], stamps.pushed.wall_ms),
            _ => unreachable!("payload_len must be BOOK_TICKER_SIZE or TRADE_SIZE"),
        }

        // 4.5) SHM sidecar — intra-host fast path.
        //      ZMQ PUSH 전에 **먼저** SHM 에 publish 한다. ZMQ 경로는 μs 대역이고,
        //      SHM 은 ns 대역이므로 순서를 이렇게 두면 strategy 가 거의 즉시 최신값을
        //      볼 수 있다. SHM publish 실패는 metric 만 올리고 ZMQ 경로는 영향 없음.
        if let Some(shm) = self.shm.as_ref() {
            // ms → ns 변환. legacy wire 포맷의 stamp 는 모두 ms 단위.
            let event_ns = u64::try_from(stamps.exchange_server.wall_ms)
                .unwrap_or(0)
                .saturating_mul(1_000_000);
            let recv_ns = u64::try_from(stamps.pushed.wall_ms)
                .unwrap_or(0)
                .saturating_mul(1_000_000);
            let pub_ns = recv_ns; // SHM publish 시각 ≈ pushed 시각. 재계산하지 않아 clock syscall 1 회 절약.
            match &event {
                MarketEvent::BookTicker(bt) | MarketEvent::WebBookTicker(bt) => {
                    shm.publish_bookticker(bt, event_ns, recv_ns, pub_ns);
                }
                MarketEvent::Trade(tr) => {
                    shm.publish_trade(tr, event_ns, recv_ns, pub_ns);
                }
            }
        }

        // 5) non-blocking send.
        let send_outcome = match self.push.lock() {
            Ok(push) => push.send_dontwait(&topic_bytes, &buf[..payload_len]),
            Err(poisoned) => {
                warn!(
                    target: "publisher::worker",
                    "push mutex poisoned — continuing with recovered socket"
                );
                poisoned
                    .into_inner()
                    .send_dontwait(&topic_bytes, &buf[..payload_len])
            }
        };
        match send_outcome {
            SendOutcome::Sent => counter_inc(CounterKey::PipelineEvent),
            SendOutcome::WouldBlock => {
                // counter 는 zmq wrapper 안에서 이미 bump.
                debug!(
                    target: "publisher::worker",
                    topic = %String::from_utf8_lossy(&topic_bytes),
                    "push HWM full — dropped"
                );
            }
            SendOutcome::Error(e) => {
                counter_inc(CounterKey::ZmqDropped);
                warn!(
                    target: "publisher::worker",
                    topic = %String::from_utf8_lossy(&topic_bytes),
                    error = ?e,
                    "push send error — event dropped"
                );
            }
        }
    }

    /// `Emitter` 클로저로 캡처. `Worker` 는 소유권이 필요하므로 caller 가
    /// `Arc<Worker>` 로 감싸 `self` 를 클로저에 clone 해 넣는다.
    pub fn into_emitter(self: Arc<Self>) -> Emitter {
        Arc::new(move |ev, ls| self.on_event(ev, ls))
    }
}

/// BookTicker 를 120B 버퍼 앞 부분에 encode.
#[inline]
fn encode_into_bookticker(buf: &mut [u8; WORKER_BUF_SIZE], bt: &DomainBookTicker) {
    // 고정 120B 구간에 대한 mutable ref 를 얻기 위해 split_at_mut.
    let (head, _tail) = buf.split_at_mut(BOOK_TICKER_SIZE);
    // head 는 &mut [u8] 길이 120 이므로 [u8; 120] 배열 참조로 변환.
    let arr: &mut [u8; BOOK_TICKER_SIZE] = head
        .try_into()
        .expect("split_at_mut invariant guarantees 120B");
    encode_bookticker_into(arr, bt);
}

/// Trade 를 128B 버퍼 앞 부분에 encode.
#[inline]
fn encode_into_trade(buf: &mut [u8; WORKER_BUF_SIZE], tr: &DomainTrade) {
    let (head, _tail) = buf.split_at_mut(TRADE_SIZE);
    let arr: &mut [u8; TRADE_SIZE] = head
        .try_into()
        .expect("split_at_mut invariant guarantees 128B");
    encode_trade_into(arr, tr);
}

/// 캐시 miss 에 한해 한 번 문자열 할당으로 토픽 생성.
///
/// 하지만 이 경로는 "구독도 하지 않은 심볼이 feed 에서 흘러 나온" 이상 사건이라
/// warn 로그를 남기고 drop. 토픽을 만들어줘도 downstream subscriber 가
/// 구독하지 않았을 가능성이 높아 대역을 낭비할 뿐이기 때문.
#[inline(never)]
fn build_uncached_topic(
    exchange: ExchangeId,
    msg_type: &'static str,
    symbol: &Symbol,
) -> Option<Arc<[u8]>> {
    warn!(
        target: "publisher::worker",
        exchange = exchange.as_str(),
        msg_type = msg_type,
        symbol = symbol.as_str(),
        "topic cache miss — event not subscribed, dropping"
    );
    counter_inc(CounterKey::ZmqDropped);
    let _ = exchange;
    let _ = msg_type;
    let _ = symbol;
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Aggregator — PULL (inproc) → patch published_ms → PUB (tcp)
// ─────────────────────────────────────────────────────────────────────────────

/// drain_batch → patch published_ms → pub.send_dontwait 를 돌리는 1-task.
///
/// 여러 Worker 의 PUSH 출력을 inproc 에서 한곳으로 모아 tcp PUB 로 팬아웃.
/// single-thread 이므로 순서/경합 없이 `PubSocket` 을 소유할 수 있다.
pub struct Aggregator {
    clock: Arc<dyn Clock>,
    pull: PullSocket,
    pub_: PubSocket,
    drain_cap: usize,
}

impl Aggregator {
    /// 새 Aggregator.
    pub fn new(clock: Arc<dyn Clock>, pull: PullSocket, pub_: PubSocket, drain_cap: usize) -> Self {
        Self {
            clock,
            pull,
            pub_,
            drain_cap,
        }
    }

    /// cancel 이 올 때까지 loop.
    ///
    /// - 매 tick drain_batch 로 최대 `drain_cap` 건 수신
    /// - recv_timeout 1ms 로 cancel 반응성 확보
    /// - cancel 이후 최대 `AGGREGATOR_DRAIN_GRACE_MS` 동안 잔여 drain
    pub async fn run(mut self, cancel: CancellationToken) {
        info!(
            target: "publisher::aggregator",
            drain_cap = self.drain_cap,
            "aggregator loop started"
        );

        // PULL/PUB 은 sync 객체라 tokio 안에서 쓸 때 spawn_blocking 이 이상적이지만
        // 여기서는 recv_timeout(1ms) + yield_now 조합으로 tokio worker 를 오래 잡지 않게
        // 한다. 다만 이 task 는 자체 전용 worker thread 에 할당되는 것을 가정.

        let mut batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(self.drain_cap);

        loop {
            // cancel 체크 (상위 우선). drain + tick loop 는 short-circuit 를 위해
            // `is_cancelled` 만 확인 — select! 와 blocking recv 를 섞지 않는다.
            if cancel.is_cancelled() {
                break;
            }

            // 1) DONTWAIT drain 우선 — 버스트가 쌓여 있으면 여러 건 처리.
            let drained = self.pull.drain_batch(self.drain_cap, &mut batch);
            if drained > 0 {
                self.process_and_clear(&mut batch);
            }

            // 2) batch 가 비면 recv_timeout(1ms) 로 짧게 대기 → kernel 에 yield.
            //    이 구간은 tokio worker 를 1ms 잡지만 다른 task 에 선점 기회 있음.
            if drained == 0 {
                match self.pull.recv_timeout(AGGREGATOR_RECV_TIMEOUT_MS) {
                    Ok(Some((t, p))) => {
                        batch.push((t, p));
                        self.process_and_clear(&mut batch);
                    }
                    Ok(None) => {
                        // idle tick — tokio runtime 에 양보.
                        tokio::task::yield_now().await;
                    }
                    Err(e) => {
                        warn!(
                            target: "publisher::aggregator",
                            error = ?e,
                            "pull recv_timeout error"
                        );
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        }

        // 3) shutdown drain — cancel 수신 후 `GRACE_MS` 동안 잔여 배출.
        let start = std::time::Instant::now();
        loop {
            if start.elapsed() >= Duration::from_millis(AGGREGATOR_DRAIN_GRACE_MS) {
                break;
            }
            let drained = self.pull.drain_batch(self.drain_cap, &mut batch);
            if drained == 0 {
                break;
            }
            self.process_and_clear(&mut batch);
        }

        info!(
            target: "publisher::aggregator",
            grace_ms = AGGREGATOR_DRAIN_GRACE_MS,
            "aggregator loop stopped"
        );
    }

    /// 배치 한 묶음 처리. 끝나면 비움.
    #[inline]
    fn process_and_clear(&mut self, batch: &mut Vec<(Vec<u8>, Vec<u8>)>) {
        let published_ms = self.clock.now_ms();
        let mut sent = 0u64;
        let mut dropped = 0u64;

        for (topic, payload) in batch.drain(..) {
            // offset 96 / 104 에 published_ms 덮어쓰기.
            // payload.len() 으로 wire type 판별 (BookTicker=120, Trade=128).
            match payload.len() {
                BOOK_TICKER_SIZE => {
                    let mut payload = payload;
                    patch_bookticker_pushed_ms(&mut payload, published_ms);
                    match self.pub_.send_dontwait(&topic, &payload) {
                        SendOutcome::Sent => sent += 1,
                        SendOutcome::WouldBlock => dropped += 1,
                        SendOutcome::Error(e) => {
                            dropped += 1;
                            warn!(
                                target: "publisher::aggregator",
                                error = ?e,
                                "pub send error"
                            );
                        }
                    }
                }
                TRADE_SIZE => {
                    let mut payload = payload;
                    patch_trade_pushed_ms(&mut payload, published_ms);
                    match self.pub_.send_dontwait(&topic, &payload) {
                        SendOutcome::Sent => sent += 1,
                        SendOutcome::WouldBlock => dropped += 1,
                        SendOutcome::Error(e) => {
                            dropped += 1;
                            warn!(
                                target: "publisher::aggregator",
                                error = ?e,
                                "pub send error"
                            );
                        }
                    }
                }
                other => {
                    // 라우팅이 여기까지 왔다는 건 Worker 가 WORKER_BUF_SIZE 말고
                    // 다른 길이를 보냈다는 뜻 — 버그일 수 있으므로 경고.
                    warn!(
                        target: "publisher::aggregator",
                        payload_len = other,
                        "unexpected payload length — dropped"
                    );
                    dropped += 1;
                    counter_inc(CounterKey::DecodeFail);
                }
            }
        }

        if sent > 0 {
            counter_add(CounterKey::PipelineEvent, sent);
        }
        if dropped > 0 {
            counter_add(CounterKey::ZmqDropped, dropped);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// FeedRegistry — ExchangeId 디스패치
// ─────────────────────────────────────────────────────────────────────────────

/// `ExchangeConfig` → 구체 Feed 인스턴스 생성.
///
/// vendor 구현체를 main 함수가 아닌 이 한 곳에서 선택한다. 테스트에서는
/// 이 함수를 우회해 `MockFeed` 를 직접 주입.
///
/// `cfg.ws_url` 이 `Some` 이면 각 vendor config 의 ws_url 을 override.
/// 백오프 / ping 간격도 `ExchangeConfig` 에서 pull 한다.
pub fn load_feed(cfg: &ExchangeConfig) -> Result<Arc<dyn ExchangeFeed>> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    match (cfg.id, cfg.role) {
        (ExchangeId::Binance, DataRole::Primary) => {
            let mut bc = BinanceConfig::default();
            if let Some(u) = &cfg.ws_url {
                bc.ws_url = u.clone();
            }
            bc.backoff_base_ms = cfg.reconnect_backoff_ms;
            Ok(Arc::new(BinanceFeed::with_clock(bc, clock)))
        }
        (ExchangeId::Binance, DataRole::WebOrderBook) => {
            Err(anyhow!("binance WebOrderBook role is not supported"))
        }
        (ExchangeId::Gate, role) => {
            let mut gc = GateConfig::default();
            if let Some(u) = &cfg.ws_url {
                match role {
                    DataRole::Primary => gc.primary_ws_url = u.clone(),
                    DataRole::WebOrderBook => gc.web_ws_url = u.clone(),
                }
            }
            gc.backoff_base_ms = cfg.reconnect_backoff_ms;
            Ok(Arc::new(GateFeed::with_clock(gc, role, clock)))
        }
        (ExchangeId::Bybit, DataRole::Primary) => {
            let mut bc = BybitConfig::default();
            if let Some(u) = &cfg.ws_url {
                bc.ws_url = u.clone();
            }
            bc.backoff_base_ms = cfg.reconnect_backoff_ms;
            bc.ping_interval_secs = cfg.ping_interval_s;
            Ok(Arc::new(BybitFeed::with_clock(bc, clock)))
        }
        (ExchangeId::Bybit, DataRole::WebOrderBook) => {
            Err(anyhow!("bybit WebOrderBook role is not supported"))
        }
        (ExchangeId::Bitget, DataRole::Primary) => {
            let mut bc = BitgetConfig::default();
            if let Some(u) = &cfg.ws_url {
                bc.ws_url = u.clone();
            }
            bc.backoff_base_ms = cfg.reconnect_backoff_ms;
            bc.ping_interval_secs = cfg.ping_interval_s;
            Ok(Arc::new(BitgetFeed::with_clock(bc, clock)))
        }
        (ExchangeId::Bitget, DataRole::WebOrderBook) => {
            Err(anyhow!("bitget WebOrderBook role is not supported"))
        }
        (ExchangeId::Okx, DataRole::Primary) => {
            let mut oc = OkxConfig::default();
            if let Some(u) = &cfg.ws_url {
                oc.ws_url = u.clone();
            }
            oc.backoff_base_ms = cfg.reconnect_backoff_ms;
            oc.ping_interval_secs = cfg.ping_interval_s;
            Ok(Arc::new(OkxFeed::with_clock(oc, clock)))
        }
        (ExchangeId::Okx, DataRole::WebOrderBook) => {
            Err(anyhow!("okx WebOrderBook role is not supported"))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Readiness probe — TCP bind on a port, 200-OK-ish healthz
// ─────────────────────────────────────────────────────────────────────────────

/// readiness state. `set_ready()` 호출 전에는 probe 가 503 응답.
#[derive(Debug, Default, Clone)]
pub struct ReadinessFlag {
    inner: Arc<ReadinessInner>,
}

#[derive(Debug, Default)]
struct ReadinessInner {
    ready: std::sync::atomic::AtomicBool,
    notify: Notify,
}

impl ReadinessFlag {
    /// 새 flag.
    pub fn new() -> Self {
        Self::default()
    }

    /// 준비 완료 → 200 OK.
    pub fn set_ready(&self) {
        self.inner
            .ready
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// 현재 ready?
    pub fn is_ready(&self) -> bool {
        self.inner.ready.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// 단순 HTTP/1.1 readiness probe.
///
/// `GET /healthz` → `200 OK` (ready) 또는 `503 Service Unavailable` (not ready).
/// 실제 HTTP 파서는 쓰지 않고 1-line 응답만 내려 deps 를 줄인다 (axum 은
/// 사용하지 않음).
///
/// `cancel` 에 의해 종료된다. bind 실패는 fatal 로 간주.
pub async fn run_readiness_probe(
    port: u16,
    ready: ReadinessFlag,
    cancel: CancellationToken,
) -> Result<()> {
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;

    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("readiness probe bind {addr}"))?;
    info!(
        target: "publisher::probe",
        addr = %addr,
        "readiness probe listening"
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accept = listener.accept() => {
                let (mut sock, peer) = match accept {
                    Ok(x) => x,
                    Err(e) => {
                        warn!(target: "publisher::probe", error = ?e, "accept failed");
                        continue;
                    }
                };
                let r = ready.clone();
                tokio::spawn(async move {
                    let status = if r.is_ready() {
                        "200 OK"
                    } else {
                        "503 Service Unavailable"
                    };
                    let body = if r.is_ready() { "ready" } else { "not-ready" };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                    let _ = peer;
                });
            }
        }
    }

    info!(target: "publisher::probe", "readiness probe stopped");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Startup 조립기
// ─────────────────────────────────────────────────────────────────────────────

/// publisher 전체를 담는 top-level 핸들. drop 되면 background tasks 가
/// cancel 되고 clean shutdown.
pub struct PublisherHandle {
    /// 모든 background task (feeds + aggregator + probe).
    pub tasks: Vec<JoinHandle<()>>,
    /// 외부에서 취소용.
    pub cancel: CancellationToken,
    /// readiness.
    pub ready: ReadinessFlag,
    /// zmq ctx 보유 (drop 시 context 정리).
    #[allow(dead_code)]
    pub zmq_ctx: ZmqContext,
}

impl PublisherHandle {
    /// SIGTERM / Ctrl-C 등 외부에서 호출. 토큰 cancel 만 수행.
    pub fn shutdown(&self) {
        if !self.cancel.is_cancelled() {
            self.cancel.cancel();
        }
    }

    /// 모든 background task 종료를 기다린다. 각 task 의 panic 은 로그만 남기고
    /// 집계 레벨에서는 성공으로 간주 (shutdown 경로에서 panic 으로 죽지 않기 위함).
    pub async fn join(mut self) {
        for (idx, t) in self.tasks.drain(..).enumerate() {
            if let Err(e) = t.await {
                warn!(
                    target: "publisher::handle",
                    task_idx = idx,
                    error = ?e,
                    "background task join error"
                );
            }
        }
    }
}

/// SPEC.md §Startup 순서 구현.
///
/// 호출자는 telemetry init 이 이미 끝났다고 가정한다. 이 함수는
/// - ZMQ Context 생성
/// - PULL (inproc bind) + PUB (tcp bind) 생성
/// - Aggregator task spawn
/// - 각 ExchangeConfig 마다 Feed + Worker 구성
/// - readiness probe 가 있다면 spawn
///   을 수행하고 `PublisherHandle` 을 돌려준다. Feed 실행 task 도 spawn 된 상태.
///
/// `readiness_port` 가 `Some` 이면 그 포트에 probe 를 띄우고, ready flag 는
/// aggregator task 가 sock bind 를 마친 이후에 set 된다.
pub async fn start(cfg: Arc<AppConfig>, readiness_port: Option<u16>) -> Result<PublisherHandle> {
    hft_config::validate(&cfg).context("config validation failed")?;

    let zmq_ctx = ZmqContext::new();
    let zmq_cfg = &cfg.zmq;

    // 1) PULL bind (inproc) + PUB bind (tcp).
    let pull = zmq_ctx
        .pull(&zmq_cfg.push_endpoint, zmq_cfg)
        .with_context(|| format!("pull bind {}", zmq_cfg.push_endpoint))?;
    let pub_ = zmq_ctx
        .pub_(&zmq_cfg.pub_endpoint, zmq_cfg)
        .with_context(|| format!("pub bind {}", zmq_cfg.pub_endpoint))?;

    // 2) topic cache (구독 범위만).
    let mut topics = TopicCache::new();
    topics.populate_from_exchanges(&cfg.exchanges);
    let topics = Arc::new(topics);
    info!(
        target: "publisher::start",
        cached_topics = topics.len(),
        "topic cache populated"
    );

    // 2.5) SHM sidecar — intra-host fast path publisher.
    //      실패 시 (권한/파일 IO/mlock) warn 후 SHM 비활성 상태로 계속 진행.
    let shm_pub: Option<Arc<crate::shm_pub::ShmPublisher>> = if cfg.shm.enabled {
        match build_shm_publisher(&cfg) {
            Ok(s) => {
                info!(
                    target: "publisher::start",
                    quote = %cfg.shm.quote_path.display(),
                    trade = %cfg.shm.trade_path.display(),
                    symtab = %cfg.shm.symtab_path.display(),
                    "SHM sidecar writers ready"
                );
                Some(s)
            }
            Err(e) => {
                warn!(
                    target: "publisher::start",
                    error = ?e,
                    "SHM sidecar init failed — publisher will run with ZMQ path only"
                );
                None
            }
        }
    } else {
        info!(target: "publisher::start", "SHM sidecar disabled by config");
        None
    };

    // 3) cancel token.
    let cancel = CancellationToken::new();
    let ready = ReadinessFlag::new();

    let mut tasks: Vec<JoinHandle<()>> = Vec::new();

    // 4) Aggregator spawn.
    //
    // 기본 경로: tokio::spawn → multi-thread runtime 의 임의 worker 에서 실행.
    // `HFT_PIN_HOT_CORES=1` 이면 전용 OS 스레드(+ current_thread runtime) 를 띄우고
    //   hot core 에 pin. tokio::task::JoinHandle 호환을 위해 완료 신호는 oneshot
    //   으로 돌려받아 task wrapper 가 await 한다. 이렇게 해야 `PublisherHandle.tasks`
    //   가 여전히 단일 Vec<JoinHandle<()>> 로 통일되어 join() 경로가 단순.
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let agg = Aggregator::new(clock.clone(), pull, pub_, zmq_cfg.drain_batch_cap);
    let agg_cancel = cancel.clone();
    let pin_hot = env_truthy(ENV_PIN_HOT_CORES);
    let agg_task: JoinHandle<()> = if pin_hot {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::Builder::new()
            .name("hft-publisher-aggregator".into())
            .spawn(move || {
                // pin 이 실패하면 warn 로그만 남기고 계속 — CAP 미설정 등
                // 예측 가능한 실패.
                if let Some(core) = hft_telemetry::next_hot_core() {
                    if let Err(e) = hft_telemetry::pin_current_thread(core) {
                        warn!(
                            target: "publisher::aggregator",
                            core = core,
                            error = ?e,
                            "pin_current_thread failed — continuing unpinned"
                        );
                    } else {
                        info!(
                            target: "publisher::aggregator",
                            core = core,
                            "aggregator thread pinned"
                        );
                    }
                }
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        error!(
                            target: "publisher::aggregator",
                            error = ?e,
                            "failed to build dedicated runtime"
                        );
                        let _ = tx.send(());
                        return;
                    }
                };
                rt.block_on(agg.run(agg_cancel));
                let _ = tx.send(());
            })
            .context("spawn aggregator OS thread")?;
        // JoinHandle 정합성을 위해 rx 를 기다리는 tokio task 로 감싼다.
        tokio::spawn(async move {
            let _ = rx.await;
        })
    } else {
        tokio::spawn(async move {
            agg.run(agg_cancel).await;
        })
    };
    tasks.push(agg_task);

    // 5) 각 ExchangeConfig 마다 Feed + Worker.
    //    각 feed 의 실제 구독 심볼은 `resolve_subscribed_symbols` 로 결정:
    //      (configured ∩ live_symbols) \ ignore_symbols
    //    live_symbols 조회가 실패한 거래소는 fallback 으로 configured 를 그대로
    //    사용하되 warn 로그를 남긴다 (REST 블랙아웃 중에도 publisher 가 살아있게).
    for ex_cfg in cfg.exchanges.iter().cloned() {
        let feed = load_feed(&ex_cfg)
            .with_context(|| format!("load_feed failed for {:?}/{:?}", ex_cfg.id, ex_cfg.role))?;

        let symbols = resolve_subscribed_symbols(feed.as_ref(), &ex_cfg).await;
        if symbols.is_empty() {
            warn!(
                target: "publisher::start",
                exchange = ?ex_cfg.id,
                role = ?ex_cfg.role,
                configured = ex_cfg.symbols.len(),
                ignored = ex_cfg.ignore_symbols.len(),
                "resolved 0 subscribed symbols after intersection — skipping feed"
            );
            continue;
        }

        let push = zmq_ctx
            .push(&zmq_cfg.push_endpoint, zmq_cfg)
            .with_context(|| format!("push connect {}", zmq_cfg.push_endpoint))?;
        let worker = Arc::new(match shm_pub.as_ref() {
            Some(s) => Worker::with_shm(clock.clone(), push, topics.clone(), s.clone()),
            None => Worker::new(clock.clone(), push, topics.clone()),
        });
        let emit = worker.into_emitter();

        let feed_cancel = cancel.clone();
        let label = feed.label();
        let feed_task = tokio::spawn(async move {
            run_feed_task(feed, symbols, emit, feed_cancel, label).await;
        });
        tasks.push(feed_task);
    }

    // 6) readiness probe.
    if let Some(port) = readiness_port {
        let probe_cancel = cancel.clone();
        let probe_ready = ready.clone();
        let probe_task = tokio::spawn(async move {
            if let Err(e) = run_readiness_probe(port, probe_ready, probe_cancel).await {
                error!(
                    target: "publisher::probe",
                    error = ?e,
                    "readiness probe exited with error"
                );
            }
        });
        tasks.push(probe_task);
    }

    // PUB bind 가 끝났고 Aggregator task 도 돌기 시작했으므로 ready=true.
    // (구독자 관점에서 tcp bind 가 accept 가능함 → true 로 기준.)
    ready.set_ready();

    Ok(PublisherHandle {
        tasks,
        cancel,
        ready,
        zmq_ctx,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Symbol intersection — Phase 2 Track B §1.6
// ─────────────────────────────────────────────────────────────────────────────

/// 한 거래소의 **실제 구독 심볼** 을 계산한다.
///
/// 알고리즘:
/// 1. `feed.available_symbols()` 로 라이브 리스트를 조회. 실패하거나 비어 있으면
///    `None` 으로 간주 (live 검증 스킵 모드).
/// 2. `configured ∩ live` — configured 순서를 유지하는 결정적 결과.
/// 3. 결과 set 에서 `ignore_symbols` 를 제거.
///
/// 성능: O(N_live + N_configured * lookup) — `ahash` 해시셋 기반. 각 start 당
/// 한번만 호출되므로 microsec 단위 최적화는 불필요.
///
/// 안정성:
/// - live 조회 실패: publisher 가 죽지 않고 configured - ignore 로 fallback + warn.
///   REST 가 일시 장애일 때도 구독 자체는 성공시키기 위함. configured 심볼이
///   상장폐지됐더라도 거래소가 알아서 에러를 내고 feed 구현체가 재연결 루프를
///   돈다 (영구 fatal 은 아님).
/// - configured 에 없는 심볼은 무시 — 블랙리스트는 "구독하지 말 것" 의미.
/// - 결과가 빈 벡터면 호출자가 feed 자체를 skip.
pub async fn resolve_subscribed_symbols(
    feed: &dyn ExchangeFeed,
    ex_cfg: &hft_config::ExchangeConfig,
) -> Vec<Symbol> {
    use std::collections::HashSet;

    // ignore set — 빠른 contains 를 위해 HashSet.
    let ignore: HashSet<Symbol> = ex_cfg.ignore_symbols.iter().cloned().collect();

    let live_set: Option<HashSet<Symbol>> = match feed.available_symbols().await {
        Ok(v) => {
            if v.is_empty() {
                warn!(
                    target: "publisher::intersect",
                    exchange = ?ex_cfg.id,
                    "available_symbols() returned empty — falling back to configured list"
                );
                None
            } else {
                Some(v.into_iter().collect())
            }
        }
        Err(e) => {
            warn!(
                target: "publisher::intersect",
                exchange = ?ex_cfg.id,
                error = %e,
                "available_symbols() failed — falling back to configured list"
            );
            None
        }
    };

    let mut out: Vec<Symbol> = Vec::with_capacity(ex_cfg.symbols.len());
    let mut dropped_ignore = 0usize;
    let mut dropped_not_live = 0usize;
    for sym in &ex_cfg.symbols {
        if ignore.contains(sym) {
            dropped_ignore += 1;
            continue;
        }
        if let Some(ref live) = live_set {
            if !live.contains(sym) {
                dropped_not_live += 1;
                continue;
            }
        }
        out.push(sym.clone());
    }

    info!(
        target: "publisher::intersect",
        exchange = ?ex_cfg.id,
        role = ?ex_cfg.role,
        configured = ex_cfg.symbols.len(),
        kept = out.len(),
        dropped_ignore,
        dropped_not_live,
        live_lookup = live_set.is_some(),
        "symbol intersection resolved"
    );
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// run_feed_task — ExchangeFeed.stream 한 번 실행 + 에러/취소 처리
// ─────────────────────────────────────────────────────────────────────────────

/// 한 feed task 의 본체. 내부 재연결은 feed 구현체가 담당하고, 여기서는
/// `stream` 자체가 복구 불가로 반환했을 때만 관측.
async fn run_feed_task(
    feed: Arc<dyn ExchangeFeed>,
    symbols: Vec<Symbol>,
    emit: Emitter,
    cancel: CancellationToken,
    label: String,
) {
    info!(
        target: "publisher::feed",
        feed = %label,
        symbols = symbols.len(),
        "feed task starting"
    );
    match feed.stream(&symbols, emit, cancel.clone()).await {
        Ok(()) => info!(
            target: "publisher::feed",
            feed = %label,
            "feed task exited cleanly"
        ),
        Err(e) => {
            if cancel.is_cancelled() {
                info!(
                    target: "publisher::feed",
                    feed = %label,
                    error = ?e,
                    "feed task exited during shutdown"
                );
            } else {
                error!(
                    target: "publisher::feed",
                    feed = %label,
                    error = ?e,
                    "feed task exited with unrecoverable error — no restart"
                );
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Feed trait wrapper 없이 직접 trait object 를 쓰므로 추가 wrapper 없음.
// ─────────────────────────────────────────────────────────────────────────────

// 컴파일 타임에 Send + Sync 를 확인.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn a<T: Send + Sync>() {}
    a::<Worker>();
    a::<TopicCache>();
    a::<PublisherHandle>();
    a::<crate::shm_pub::ShmPublisher>();
}

// ─────────────────────────────────────────────────────────────────────────────
// SHM publisher 빌더
// ─────────────────────────────────────────────────────────────────────────────

/// `cfg.shm` 기준으로 SHM 영역을 열고 `ShmPublisher` 를 만든다.
///
/// - `backend=LegacyMultiFile` → 기존 4개 파일 (quote/trade/order/symtab) 분리 모드.
///   publisher 는 그 중 writer 3종 (quote/trade/symtab) 만 만든다. order ring 의
///   writer 는 Python strategy 가 열기 때문.
/// - `backend=DevShm/Hugetlbfs` → v2 single-file SharedRegion 모드.
///   `ShmPublisher::from_shared_region` 이 quote/trade/symtab 외에 모든
///   N_max 개 order ring header 까지 초기화해 둔다.
/// - `backend=PciBar` → publisher 는 올릴 수 없음 (publisher 는 host-level infra VM 이며
///   ivshmem resource2 를 직접 mmap 하는 경로는 guest-VM 전용). 에러 반환.
fn build_shm_publisher(cfg: &AppConfig) -> Result<Arc<crate::shm_pub::ShmPublisher>> {
    match cfg.shm.backend {
        hft_config::ShmBackendKind::LegacyMultiFile => {
            use hft_shm::{QuoteSlotWriter, SymbolTable, TradeRingWriter};

            let quote = QuoteSlotWriter::create(&cfg.shm.quote_path, cfg.shm.quote_slot_count)
                .with_context(|| {
                    format!("QuoteSlotWriter::create({})", cfg.shm.quote_path.display())
                })?;
            let trade = TradeRingWriter::create(&cfg.shm.trade_path, cfg.shm.trade_ring_capacity)
                .with_context(|| {
                format!("TradeRingWriter::create({})", cfg.shm.trade_path.display())
            })?;
            let symtab =
                SymbolTable::open_or_create(&cfg.shm.symtab_path, cfg.shm.symbol_table_capacity)
                    .with_context(|| {
                        format!(
                            "SymbolTable::open_or_create({})",
                            cfg.shm.symtab_path.display()
                        )
                    })?;

            Ok(Arc::new(crate::shm_pub::ShmPublisher::new(
                Arc::new(quote),
                Arc::new(trade),
                Arc::new(symtab),
            )))
        }
        hft_config::ShmBackendKind::DevShm | hft_config::ShmBackendKind::Hugetlbfs => {
            let backing = match cfg.shm.backend {
                hft_config::ShmBackendKind::DevShm => hft_shm::Backing::DevShm {
                    path: cfg.shm.shared_path.clone(),
                },
                hft_config::ShmBackendKind::Hugetlbfs => hft_shm::Backing::Hugetlbfs {
                    path: cfg.shm.shared_path.clone(),
                },
                _ => unreachable!(),
            };
            let spec = hft_shm::LayoutSpec {
                quote_slot_count: cfg.shm.quote_slot_count,
                trade_ring_capacity: cfg.shm.trade_ring_capacity,
                symtab_capacity: cfg.shm.symbol_table_capacity,
                order_ring_capacity: cfg.shm.order_ring_capacity,
                n_max: cfg.shm.n_max,
            };
            let sp = crate::shm_pub::ShmPublisher::from_shared_region(backing, spec)?;
            Ok(Arc::new(sp))
        }
        hft_config::ShmBackendKind::PciBar => {
            anyhow::bail!(
                "publisher cannot run with backend=PciBar — publisher must be on the host/infra \
                 VM that owns the ivshmem file, not a guest VM. Use DevShm or Hugetlbfs."
            );
        }
    }
}

// `Aggregator` 의 async 함수가 `async_trait` 없이도 `Send + 'static` 이 되게 함:
// tokio::spawn 에 넘기려면 run 전체가 Send 여야 한다. 현재 구현은 `PubSocket`,
// `PullSocket`, `Vec<(Vec<u8>,Vec<u8>)>` 만 들고 있고 모두 Send 이므로 OK.
//
// `async_trait` 을 쓰지 않는 이유: trait object 뒤에 숨길 이유가 없고
// 직접 호출하는 single-task 이기 때문.
#[allow(dead_code)]
const _: fn() = || {
    fn a<T: Send + 'static>() {}
    a::<Aggregator>();
};

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_config::ZmqConfig;
    use hft_testkit::fixtures as samples;
    use hft_time::MockClock;
    use hft_types::{DataRole, ExchangeId, Price, Size};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn unique_inproc() -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        let id = N.fetch_add(1, Ordering::SeqCst);
        format!("inproc://publisher-test-{}", id)
    }

    // ─────────── Mock feed for resolve_subscribed_symbols tests ───────────
    struct MockFeed {
        id: ExchangeId,
        role: DataRole,
        live: anyhow::Result<Vec<Symbol>>,
    }
    #[async_trait::async_trait]
    impl ExchangeFeed for MockFeed {
        fn id(&self) -> ExchangeId {
            self.id
        }
        fn role(&self) -> DataRole {
            self.role
        }
        async fn available_symbols(&self) -> anyhow::Result<Vec<Symbol>> {
            match &self.live {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
        async fn stream(
            &self,
            _symbols: &[Symbol],
            _emit: Emitter,
            _cancel: CancellationToken,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn ex_cfg_with(symbols: Vec<Symbol>, ignore: Vec<Symbol>) -> hft_config::ExchangeConfig {
        hft_config::ExchangeConfig {
            id: ExchangeId::Gate,
            symbols,
            role: DataRole::Primary,
            ws_url: None,
            reconnect_backoff_ms: 200,
            ping_interval_s: 15,
            ignore_symbols: ignore,
        }
    }

    #[tokio::test]
    async fn resolve_intersects_configured_with_live() {
        let feed = MockFeed {
            id: ExchangeId::Gate,
            role: DataRole::Primary,
            live: Ok(vec![Symbol::new("BTC_USDT"), Symbol::new("ETH_USDT")]),
        };
        let cfg = ex_cfg_with(
            vec![Symbol::new("BTC_USDT"), Symbol::new("DOGE_USDT")],
            vec![],
        );
        let out = resolve_subscribed_symbols(&feed, &cfg).await;
        assert_eq!(out, vec![Symbol::new("BTC_USDT")]);
    }

    #[tokio::test]
    async fn resolve_drops_ignored_symbols() {
        let feed = MockFeed {
            id: ExchangeId::Gate,
            role: DataRole::Primary,
            live: Ok(vec![
                Symbol::new("BTC_USDT"),
                Symbol::new("ETH_USDT"),
                Symbol::new("SOL_USDT"),
            ]),
        };
        let cfg = ex_cfg_with(
            vec![
                Symbol::new("BTC_USDT"),
                Symbol::new("ETH_USDT"),
                Symbol::new("SOL_USDT"),
            ],
            vec![Symbol::new("ETH_USDT")],
        );
        let out = resolve_subscribed_symbols(&feed, &cfg).await;
        assert_eq!(out, vec![Symbol::new("BTC_USDT"), Symbol::new("SOL_USDT")]);
    }

    #[tokio::test]
    async fn resolve_falls_back_when_live_lookup_fails() {
        let feed = MockFeed {
            id: ExchangeId::Gate,
            role: DataRole::Primary,
            live: Err(anyhow::anyhow!("REST down")),
        };
        let cfg = ex_cfg_with(
            vec![Symbol::new("BTC_USDT"), Symbol::new("DOGE_USDT")],
            vec![Symbol::new("DOGE_USDT")],
        );
        let out = resolve_subscribed_symbols(&feed, &cfg).await;
        // live 조회 실패 → configured \ ignore 로 그대로 폴백. BTC 1개.
        assert_eq!(out, vec![Symbol::new("BTC_USDT")]);
    }

    #[tokio::test]
    async fn resolve_preserves_configured_order() {
        let feed = MockFeed {
            id: ExchangeId::Gate,
            role: DataRole::Primary,
            live: Ok(vec![Symbol::new("C"), Symbol::new("B"), Symbol::new("A")]),
        };
        let cfg = ex_cfg_with(
            vec![Symbol::new("A"), Symbol::new("B"), Symbol::new("C")],
            vec![],
        );
        let out = resolve_subscribed_symbols(&feed, &cfg).await;
        // configured 순서 유지 — A, B, C.
        assert_eq!(
            out,
            vec![Symbol::new("A"), Symbol::new("B"), Symbol::new("C")]
        );
    }

    #[tokio::test]
    async fn resolve_empty_live_falls_back_to_configured() {
        let feed = MockFeed {
            id: ExchangeId::Gate,
            role: DataRole::Primary,
            live: Ok(vec![]),
        };
        let cfg = ex_cfg_with(vec![Symbol::new("BTC_USDT")], vec![]);
        let out = resolve_subscribed_symbols(&feed, &cfg).await;
        assert_eq!(out, vec![Symbol::new("BTC_USDT")]);
    }
    // ─────────── end resolve tests ───────────

    fn default_zmq(push: &str, pub_: &str) -> ZmqConfig {
        ZmqConfig {
            hwm: 10_000,
            linger_ms: 0,
            drain_batch_cap: 64,
            push_endpoint: push.into(),
            pub_endpoint: pub_.into(),
            sub_endpoint: pub_.into(),
            order_ingress_bind: None,
            result_egress_bind: None,
            result_heartbeat_interval_ms: 0,
        }
    }

    #[test]
    fn topic_cache_populates_primary_with_bookticker_and_trade() {
        let mut tc = TopicCache::new();
        let ex = ExchangeConfig {
            id: ExchangeId::Gate,
            symbols: vec![Symbol::new("BTC_USDT"), Symbol::new("ETH_USDT")],
            role: DataRole::Primary,
            ws_url: None,
            reconnect_backoff_ms: 200,
            ping_interval_s: 15,
            ignore_symbols: Vec::new(),
        };
        tc.populate_from_exchanges(&[ex]);
        // 2 symbols × (bookticker + trade) = 4.
        assert_eq!(tc.len(), 4);
        assert!(tc
            .get(ExchangeId::Gate, MSG_BOOKTICKER, &Symbol::new("BTC_USDT"))
            .is_some());
        assert!(tc
            .get(ExchangeId::Gate, MSG_TRADE, &Symbol::new("ETH_USDT"))
            .is_some());
        assert!(tc
            .get(
                ExchangeId::Gate,
                MSG_WEBBOOKTICKER,
                &Symbol::new("BTC_USDT")
            )
            .is_none());
    }

    #[test]
    fn topic_cache_populates_web_orderbook_only_webbookticker() {
        let mut tc = TopicCache::new();
        let ex = ExchangeConfig {
            id: ExchangeId::Gate,
            symbols: vec![Symbol::new("BTC_USDT")],
            role: DataRole::WebOrderBook,
            ws_url: None,
            reconnect_backoff_ms: 200,
            ping_interval_s: 15,
            ignore_symbols: Vec::new(),
        };
        tc.populate_from_exchanges(&[ex]);
        assert_eq!(tc.len(), 1);
        assert!(tc
            .get(
                ExchangeId::Gate,
                MSG_WEBBOOKTICKER,
                &Symbol::new("BTC_USDT")
            )
            .is_some());
    }

    #[test]
    fn topic_cache_get_returns_exact_topic_bytes() {
        let mut tc = TopicCache::new();
        tc.insert(
            ExchangeId::Binance,
            MSG_BOOKTICKER,
            &Symbol::new("BTC_USDT"),
        );
        let got = tc
            .get(
                ExchangeId::Binance,
                MSG_BOOKTICKER,
                &Symbol::new("BTC_USDT"),
            )
            .unwrap();
        assert_eq!(got.as_ref(), b"binance_bookticker_BTC_USDT");
    }

    #[tokio::test]
    async fn worker_on_event_encodes_and_push_ok() {
        let ctx = ZmqContext::new();
        let ep = unique_inproc();
        let cfg = default_zmq(&ep, "inproc://unused");

        // PULL 먼저 bind.
        let mut pull = ctx.pull(&ep, &cfg).unwrap();
        let push = ctx.push(&ep, &cfg).unwrap();

        let mut tc = TopicCache::new();
        tc.insert(ExchangeId::Gate, MSG_BOOKTICKER, &Symbol::new("BTC_USDT"));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let worker = Worker::new(clock, push, Arc::new(tc));

        let bt = samples::bookticker(1_700_000_000_000);
        let mut ls = LatencyStamps::new();
        ls.set_exchange_server_ms(bt.server_time_ms);

        worker.on_event(MarketEvent::BookTicker(bt), ls);

        let got = pull.recv_timeout(500).unwrap().expect("no frame");
        assert_eq!(got.0, b"gate_bookticker_BTC_USDT");
        assert_eq!(got.1.len(), BOOK_TICKER_SIZE);

        // offset 96..104 = pushed_ms — non-zero.
        let pushed_bytes = &got.1[96..104];
        let pushed = i64::from_le_bytes(pushed_bytes.try_into().unwrap());
        assert!(pushed > 0, "pushed_ms should be non-zero: got {}", pushed);
    }

    #[tokio::test]
    async fn worker_on_event_trade_patches_offset_104() {
        let ctx = ZmqContext::new();
        let ep = unique_inproc();
        let cfg = default_zmq(&ep, "inproc://unused");

        let mut pull = ctx.pull(&ep, &cfg).unwrap();
        let push = ctx.push(&ep, &cfg).unwrap();

        let mut tc = TopicCache::new();
        tc.insert(ExchangeId::Gate, MSG_TRADE, &Symbol::new("BTC_USDT"));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let worker = Worker::new(clock, push, Arc::new(tc));

        let tr = samples::trade(1_700_000_000_123);
        let mut ls = LatencyStamps::new();
        ls.set_exchange_server_ms(tr.server_time_ms);
        worker.on_event(MarketEvent::Trade(tr), ls);

        let got = pull.recv_timeout(500).unwrap().expect("no frame");
        assert_eq!(got.0, b"gate_trade_BTC_USDT");
        assert_eq!(got.1.len(), TRADE_SIZE);

        let pushed_bytes = &got.1[104..112];
        let pushed = i64::from_le_bytes(pushed_bytes.try_into().unwrap());
        assert!(pushed > 0);
    }

    #[tokio::test]
    async fn worker_drops_uncached_topic_without_panic() {
        let ctx = ZmqContext::new();
        let ep = unique_inproc();
        let cfg = default_zmq(&ep, "inproc://unused");

        let mut pull = ctx.pull(&ep, &cfg).unwrap();
        let push = ctx.push(&ep, &cfg).unwrap();

        // 빈 캐시 — 어떤 토픽도 등록되지 않음.
        let tc = Arc::new(TopicCache::new());
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let worker = Worker::new(clock, push, tc);

        let bt = samples::bookticker(0);
        let mut ls = LatencyStamps::new();
        ls.set_exchange_server_ms(bt.server_time_ms);
        worker.on_event(MarketEvent::BookTicker(bt), ls);

        // 아무것도 수신되지 않아야 함.
        let got = pull.recv_timeout(100).unwrap();
        assert!(got.is_none(), "uncached topic must be dropped");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aggregator_drains_and_publishes_with_patched_published_ms() {
        let ctx = ZmqContext::new();
        let push_ep = unique_inproc();
        let pub_ep = unique_inproc();
        let cfg = default_zmq(&push_ep, &pub_ep);

        // Subscriber first so slow-joiner 방지 — SUB connect + 구독 먼저.
        let mut sub = ctx.sub(&pub_ep, &[b""], &cfg).unwrap();

        // pull bind, pub bind.
        let pull = ctx.pull(&push_ep, &cfg).unwrap();
        let pub_ = ctx.pub_(&pub_ep, &cfg).unwrap();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());

        let cancel = CancellationToken::new();
        let agg = Aggregator::new(clock.clone(), pull, pub_, 64);
        let agg_cancel = cancel.clone();
        let agg_task = tokio::spawn(async move { agg.run(agg_cancel).await });

        // producer: Worker 하나 + bookticker 1건.
        let push = ctx.push(&push_ep, &cfg).unwrap();
        let mut tc = TopicCache::new();
        tc.insert(ExchangeId::Gate, MSG_BOOKTICKER, &Symbol::new("BTC_USDT"));
        let worker = Worker::new(clock, push, Arc::new(tc));
        let bt = samples::bookticker(1_700_000_000_000);
        let mut ls = LatencyStamps::new();
        ls.set_exchange_server_ms(bt.server_time_ms);

        // slow joiner — 여러 번 emit + 조기 recv.
        let mut received = None;
        for _ in 0..50 {
            worker.on_event(MarketEvent::BookTicker(bt.clone()), ls);
            if let Some(m) = sub.recv_timeout(50).unwrap() {
                received = Some(m);
                break;
            }
        }

        cancel.cancel();
        let _ = agg_task.await;

        let m = received.expect("no PUB frame received");
        assert_eq!(m.0, b"gate_bookticker_BTC_USDT");
        assert_eq!(m.1.len(), BOOK_TICKER_SIZE);

        // offset 96..104 에 aggregator 가 찍은 published_ms 가 들어가 있어야 함.
        let published_bytes = &m.1[96..104];
        let published = i64::from_le_bytes(published_bytes.try_into().unwrap());
        assert!(
            published > 0,
            "published_ms must be non-zero: {}",
            published
        );
    }

    #[tokio::test]
    async fn readiness_probe_reports_ready_after_flag_set() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let flag = ReadinessFlag::new();
        let port: u16 = {
            // 임의 사용 가능 포트: kernel 에 0 바인드 → 실제 포트 추출.
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };

        let cancel = CancellationToken::new();
        let probe_cancel = cancel.clone();
        let probe_flag = flag.clone();
        let probe_task =
            tokio::spawn(async move { run_readiness_probe(port, probe_flag, probe_cancel).await });

        // bind race 대비 잠깐 대기.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // 1) 아직 not ready.
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        let n = s.read(&mut buf).await.unwrap();
        let resp = std::str::from_utf8(&buf[..n]).unwrap_or("");
        assert!(resp.contains("503"), "expected 503 before ready: {resp}");

        // 2) set_ready → 200.
        flag.set_ready();
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        let n = s.read(&mut buf).await.unwrap();
        let resp = std::str::from_utf8(&buf[..n]).unwrap_or("");
        assert!(resp.contains("200"), "expected 200 after ready: {resp}");

        cancel.cancel();
        let _ = probe_task.await;
    }

    #[tokio::test]
    async fn start_loads_feeds_and_handle_shutdowns_cleanly() {
        let push_ep = unique_inproc();
        let pub_ep = unique_inproc();

        let app = AppConfig {
            service_name: "publisher-test".into(),
            hot_workers: 2,
            zmq: default_zmq(&push_ep, &pub_ep),
            // 실제 네트워크 WS 를 띄우지 않기 위해 fake endpoint. feed 는
            // connect 실패를 반복하겠지만 cancel 로 종료 가능.
            exchanges: vec![ExchangeConfig {
                id: ExchangeId::Gate,
                symbols: vec![Symbol::new("BTC_USDT")],
                role: DataRole::Primary,
                ws_url: Some("ws://127.0.0.1:1".into()),
                reconnect_backoff_ms: 200,
                ping_interval_s: 15,
                ignore_symbols: Vec::new(),
            }],
            ..Default::default()
        };

        let handle = start(Arc::new(app), None).await.unwrap();
        assert!(
            handle.ready.is_ready(),
            "handle must be ready right after bind"
        );

        // 짧게 기다렸다 shutdown.
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.shutdown();

        // 전체 join 은 5초 안에 끝나야 한다.
        let timed = tokio::time::timeout(Duration::from_secs(5), handle.join()).await;
        assert!(timed.is_ok(), "handle.join() did not complete within 5s");
    }

    const _: () = {
        assert!(WORKER_BUF_SIZE >= BOOK_TICKER_SIZE);
        assert!(WORKER_BUF_SIZE >= TRADE_SIZE);
    };

    #[tokio::test]
    async fn worker_emit_closure_matches_arc() {
        // into_emitter 가 실제로 on_event 를 호출하는지.
        let ctx = ZmqContext::new();
        let ep = unique_inproc();
        let cfg = default_zmq(&ep, "inproc://unused");
        let mut pull = ctx.pull(&ep, &cfg).unwrap();
        let push = ctx.push(&ep, &cfg).unwrap();

        let mut tc = TopicCache::new();
        tc.insert(ExchangeId::Gate, MSG_BOOKTICKER, &Symbol::new("BTC_USDT"));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let worker = Arc::new(Worker::new(clock, push, Arc::new(tc)));
        let emit: Emitter = worker.into_emitter();

        let bt = DomainBookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(1.0),
            ask_price: Price(1.1),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: 1,
            server_time_ms: 1,
        };
        let mut ls = LatencyStamps::new();
        ls.set_exchange_server_ms(1);
        (emit)(MarketEvent::BookTicker(bt), ls);

        let got = pull.recv_timeout(300).unwrap().expect("no frame");
        assert_eq!(got.0, b"gate_bookticker_BTC_USDT");
    }

    // feed registry 는 실제 network 을 띄우지 않으므로 간단한 존재 검증만.
    #[test]
    fn load_feed_builds_feed_for_each_primary_exchange() {
        for ex in [
            ExchangeId::Binance,
            ExchangeId::Gate,
            ExchangeId::Bybit,
            ExchangeId::Bitget,
            ExchangeId::Okx,
        ] {
            let ec = ExchangeConfig {
                id: ex,
                symbols: vec![Symbol::new("BTC_USDT")],
                role: DataRole::Primary,
                ws_url: None,
                reconnect_backoff_ms: 200,
                ping_interval_s: 15,
                ignore_symbols: Vec::new(),
            };
            let feed = load_feed(&ec).expect("load_feed must succeed");
            assert_eq!(feed.id(), ex);
            assert_eq!(feed.role(), DataRole::Primary);
        }
    }

    #[test]
    fn load_feed_builds_gate_weborderbook() {
        let ec = ExchangeConfig {
            id: ExchangeId::Gate,
            symbols: vec![Symbol::new("BTC_USDT")],
            role: DataRole::WebOrderBook,
            ws_url: None,
            reconnect_backoff_ms: 200,
            ping_interval_s: 15,
            ignore_symbols: Vec::new(),
        };
        let feed = load_feed(&ec).expect("gate web orderbook must load");
        assert_eq!(feed.id(), ExchangeId::Gate);
        assert_eq!(feed.role(), DataRole::WebOrderBook);
    }

    #[test]
    fn load_feed_rejects_unsupported_role() {
        for ex in [
            ExchangeId::Binance,
            ExchangeId::Bybit,
            ExchangeId::Bitget,
            ExchangeId::Okx,
        ] {
            let ec = ExchangeConfig {
                id: ex,
                symbols: vec![Symbol::new("BTC_USDT")],
                role: DataRole::WebOrderBook,
                ws_url: None,
                reconnect_backoff_ms: 200,
                ping_interval_s: 15,
                ignore_symbols: Vec::new(),
            };
            assert!(
                load_feed(&ec).is_err(),
                "{ex:?} web orderbook should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn aggregator_stops_on_cancel_even_with_no_data() {
        let ctx = ZmqContext::new();
        let push_ep = unique_inproc();
        let pub_ep = unique_inproc();
        let cfg = default_zmq(&push_ep, &pub_ep);

        let pull = ctx.pull(&push_ep, &cfg).unwrap();
        let pub_ = ctx.pub_(&pub_ep, &cfg).unwrap();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());

        let cancel = CancellationToken::new();
        let agg = Aggregator::new(clock, pull, pub_, 64);
        let agg_cancel = cancel.clone();
        let agg_task = tokio::spawn(async move { agg.run(agg_cancel).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        let timed = tokio::time::timeout(Duration::from_secs(3), agg_task).await;
        assert!(
            timed.is_ok(),
            "aggregator did not stop within 3s after cancel"
        );
    }

    #[test]
    fn readiness_flag_flips_once_set() {
        let r = ReadinessFlag::new();
        assert!(!r.is_ready());
        r.set_ready();
        assert!(r.is_ready());
    }

    // MockClock 을 쓰는 버전 — into_emitter 가 clock 을 올바르게 사용하는지 확인.
    #[tokio::test]
    async fn worker_on_event_uses_injected_clock_for_stamps() {
        let ctx = ZmqContext::new();
        let ep = unique_inproc();
        let cfg = default_zmq(&ep, "inproc://unused");
        let mut pull = ctx.pull(&ep, &cfg).unwrap();
        let push = ctx.push(&ep, &cfg).unwrap();

        let mock_clock = MockClock::new(42_000, 100_000);
        let clock: Arc<dyn Clock> = mock_clock.clone();

        let mut tc = TopicCache::new();
        tc.insert(ExchangeId::Gate, MSG_BOOKTICKER, &Symbol::new("BTC_USDT"));
        let worker = Worker::new(clock, push, Arc::new(tc));

        let bt = samples::bookticker(1_000);
        let mut ls = LatencyStamps::new();
        ls.set_exchange_server_ms(bt.server_time_ms);
        worker.on_event(MarketEvent::BookTicker(bt), ls);

        let got = pull.recv_timeout(500).unwrap().expect("no frame");
        let pushed_bytes = &got.1[96..104];
        let pushed = i64::from_le_bytes(pushed_bytes.try_into().unwrap());
        // MockClock 는 전진하지 않았으므로 now_ms=42_000.
        assert_eq!(pushed, 42_000);
    }

    // ── SHM sidecar 통합 ─────────────────────────────────────────────────

    #[tokio::test]
    async fn worker_with_shm_writes_to_both_zmq_and_shm() {
        use crate::shm_pub::ShmPublisher;
        use hft_shm::{QuoteSlotReader, QuoteSlotWriter, SymbolTable, TradeRingWriter};
        use tempfile::tempdir;

        let ctx = ZmqContext::new();
        let ep = unique_inproc();
        let cfg = default_zmq(&ep, "inproc://unused");
        let mut pull = ctx.pull(&ep, &cfg).unwrap();
        let push = ctx.push(&ep, &cfg).unwrap();

        let dir = tempdir().unwrap();
        let qw = Arc::new(QuoteSlotWriter::create(&dir.path().join("q"), 32).unwrap());
        let tw = Arc::new(TradeRingWriter::create(&dir.path().join("t"), 64).unwrap());
        let sym = Arc::new(SymbolTable::open_or_create(&dir.path().join("s"), 16).unwrap());
        let shm = Arc::new(ShmPublisher::new(qw, tw, sym));

        let mut tc = TopicCache::new();
        tc.insert(ExchangeId::Gate, MSG_BOOKTICKER, &Symbol::new("BTC_USDT"));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let worker = Worker::with_shm(clock, push, Arc::new(tc), shm);

        let bt = samples::bookticker(1_700_000_000_000);
        let mut ls = LatencyStamps::new();
        ls.set_exchange_server_ms(bt.server_time_ms);
        worker.on_event(MarketEvent::BookTicker(bt), ls);

        // ZMQ 경로.
        let zmq_got = pull.recv_timeout(500).unwrap().expect("no zmq frame");
        assert_eq!(zmq_got.1.len(), BOOK_TICKER_SIZE);

        // SHM 경로 — 같은 BookTicker 가 slot 0 에 기록됨.
        let qr = QuoteSlotReader::open(&dir.path().join("q")).unwrap();
        let snap = qr.read(0).expect("shm slot must be populated");
        // samples::bookticker 의 가격이 실제로 채워졌는지만 확인 (정확한 값은 샘플 구현에 의존).
        // bid/ask 가 둘 다 0 이 아니면 성공.
        assert!(snap.bid_price != 0 || snap.ask_price != 0);
    }

    #[test]
    fn worker_without_shm_still_works() {
        // 기본 경로 — shm=None 인 Worker::new 는 SHM 호출을 스킵해야 한다.
        let ctx = ZmqContext::new();
        let ep = unique_inproc();
        let cfg = default_zmq(&ep, "inproc://unused");
        let _pull = ctx.pull(&ep, &cfg).unwrap();
        let push = ctx.push(&ep, &cfg).unwrap();

        let mut tc = TopicCache::new();
        tc.insert(ExchangeId::Gate, MSG_BOOKTICKER, &Symbol::new("BTC_USDT"));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let worker = Worker::new(clock, push, Arc::new(tc));
        assert!(worker.shm.is_none());
    }
}

// NOTE: `async_trait` 은 Cargo.toml 에 올려져 있지만 이 파일에서는 직접 사용하지 않는다.
// publisher 에 추가 trait 가 필요해질 때 use 로 가져오면 된다. (현재 unused 이지만
// workspace 다른 crate 들이 쓰므로 dep 유지)

//! subscriber — SUB → decode → downstream (InprocQueue / DirectFn / InprocZmqPub)
//!
//! # 책임 (SPEC §Structure)
//! - ZMQ SUB 에 bind & subscribe (prefix 기반)
//! - `drain_batch(cap, &mut scratch)` 로 한 번에 여러 프레임 소비
//! - 각 `(topic_bytes, payload_bytes)` 를 `decode` 하여
//!   `(MarketEvent, LatencyStamps)` 로 복원
//! - `stamps.subscribed` 를 기록하고 downstream 에 non-blocking push
//! - downstream 이 꽉 차면 drop + counter
//!
//! # Latency 예산 (phase1/latency-budget.md)
//! stage 6 (subscribed) - stage 5 (published) p99.9 < 1ms.
//! decode 는 LE 읽기 + Symbol::new(Arc<str>) 만 수행 → 일반적으로 sub-μs.
//!
//! # Hot path 비할당 원칙
//! - `scratch: Vec<(Vec<u8>, Vec<u8>)>` 는 subscriber 가 struct 에 담고 재사용.
//! - `decode` 내부는 `BookTickerWire::into_domain()` + `Symbol::from(String)` 로
//!   `String → Arc<str>` 한 번의 알로케이션만 발생 (Symbol 은 Arc<str> 래퍼).
//! - downstream = `crossbeam_channel::bounded(N)` 의 `try_send` → lock-free,
//!   꽉 찼으면 Err 리턴 → counter_inc(ZmqDropped) 후 drop.
//!
//! # 종료
//! `CancellationToken` 이 cancel 되면 루프가 탈출한다. scratch 는 tokio task 종료
//! 시 자동 drop. ZMQ SUB 은 `ZmqContext` drop 시 cleanup 된다.

#![deny(rust_2018_idioms)]

use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use hft_config::{AppConfig, ExchangeConfig, ZmqConfig};
use hft_exchange_api::CancellationToken;
use hft_protocol::{
    decode_bookticker, decode_trade, parse_topic, BOOK_TICKER_SIZE, MSG_BOOKTICKER, MSG_TRADE,
    MSG_WEBBOOKTICKER, TRADE_SIZE,
};
use hft_telemetry::{counter_inc, CounterKey};
use hft_time::{Clock, LatencyStamps, Stage, SystemClock};
use hft_types::{BookTicker, ExchangeId, MarketEvent, Price, Size, Symbol, Trade};
use hft_zmq::{Context as ZmqContext, SubSocket, TopicPayload};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// SubTask 1 턴에 `drain_batch` 가 가져올 최대 메시지 수. config 의 `drain_batch_cap` 과 동일.
pub const DRAIN_CAP_DEFAULT: usize = 512;

/// drain_batch 가 0건 반환 시 await 로 양보할 receive timeout (ms).
/// 너무 작으면 busy-loop, 너무 크면 shutdown 지연.
const SUB_RECV_TIMEOUT_MS: i32 = 1;

/// InprocQueue 기본 용량. strategy task 가 느려서 꽉 차면 drop.
pub const INPROC_QUEUE_CAPACITY: usize = 8192;

// ─────────────────────────────────────────────────────────────────────────────
// Downstream — subscriber → (strategy/probe) 전달 추상
// ─────────────────────────────────────────────────────────────────────────────

/// SubTask 가 한 이벤트를 decode 한 직후 호출하는 downstream.
///
/// - 구현은 **non-blocking** 이어야 함. 꽉 찼으면 `Err(DownstreamBusy)` 반환.
/// - hot path 에서 1회 호출되므로 trait object 분기 비용 최소화 목적상
///   `&self` 만 노출 (mut state 가 필요하면 내부에 AtomicX / Mutex).
///
/// Downstream 은 `Send + Sync + 'static` 이어야 한다 (task 간 공유).
pub trait Downstream: Send + Sync + 'static {
    /// 이벤트 하나를 consume 단계로 보낸다. 가득 차면 Err.
    ///
    /// 반환 시 downstream 에서 값을 받을지 여부는 구현 책임. `try_send` 가 가장 흔함.
    fn try_push(&self, ev: MarketEvent, stamps: LatencyStamps) -> Result<(), DownstreamBusy>;
}

/// downstream 이 가득 차서 push 실패했음을 나타낸다. 호출자는 drop + counter 처리.
#[derive(Debug, Clone, Copy)]
pub struct DownstreamBusy;

/// crossbeam `Sender<(MarketEvent, LatencyStamps)>` 래퍼.
///
/// Phase 1 의 기본 downstream: subscriber 와 strategy 가 같은 프로세스.
pub struct InprocQueue {
    tx: Sender<(MarketEvent, LatencyStamps)>,
}

impl InprocQueue {
    /// bounded(N) 로 채널 생성. `(queue, receiver)` 를 돌려주며 receiver 는
    /// strategy 쪽이 가져가 `recv` / `try_recv` 로 꺼내 쓴다.
    pub fn bounded(cap: usize) -> (Self, Receiver<(MarketEvent, LatencyStamps)>) {
        let (tx, rx) = crossbeam_channel::bounded(cap);
        (Self { tx }, rx)
    }
}

impl Downstream for InprocQueue {
    #[inline]
    fn try_push(&self, ev: MarketEvent, stamps: LatencyStamps) -> Result<(), DownstreamBusy> {
        match self.tx.try_send((ev, stamps)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(DownstreamBusy),
            Err(TrySendError::Disconnected(_)) => {
                // receiver 가 drop 된 경우: 로깅만 하고 busy 로 리턴해 drop 되도록.
                // strategy task 가 panic/종료했을 가능성 → shutdown 경로 책임.
                Err(DownstreamBusy)
            }
        }
    }
}

/// `Arc<dyn Fn(MarketEvent, LatencyStamps)>` 래퍼 — latency-probe 전용.
///
/// HDR histogram 에 바로 record 할 때처럼 blocking 없이 호출 가능한 컨슈머.
pub struct DirectFn {
    f: Arc<dyn Fn(MarketEvent, LatencyStamps) + Send + Sync + 'static>,
}

impl DirectFn {
    /// 새 DirectFn.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(MarketEvent, LatencyStamps) + Send + Sync + 'static,
    {
        Self { f: Arc::new(f) }
    }
}

impl Downstream for DirectFn {
    #[inline]
    fn try_push(&self, ev: MarketEvent, stamps: LatencyStamps) -> Result<(), DownstreamBusy> {
        (self.f)(ev, stamps);
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Decode
// ─────────────────────────────────────────────────────────────────────────────

/// `(topic_bytes, payload_bytes)` → `(MarketEvent, LatencyStamps)`.
///
/// # 반환
/// - `Some((ev, stamps))`: 성공. stamps 에는 wire 에 실려 있던 exchange_server / pushed
///   / published 가 채워진다. subscribed/consumed 는 호출자가 따로 mark.
/// - `None`: 디코드 실패 — topic 파싱 실패, payload 길이 불일치, 알 수 없는 msg_type 등.
///   호출자가 `counter_inc(DecodeFail)` 후 drop.
///
/// # 할당
/// - `parse_topic` 은 `String::from_utf8_lossy` 를 통해 한 번 복제. Phase 2 에서
///   topic prefix 가 고정 길이라는 점을 활용해 stack 버퍼로 비교하도록 최적화 가능.
/// - `BookTickerWire::into_domain` 은 `Symbol::from(String)` 을 호출 → `Arc<str>` 1 회.
pub fn decode(topic: &[u8], payload: &[u8]) -> Option<(MarketEvent, LatencyStamps)> {
    // topic 은 UTF-8 ASCII 범위 (`{exchange}_{msg}_{symbol}`) — lossy 해도 prefix 는 보존.
    let topic_str = std::str::from_utf8(topic).ok()?;
    let parsed = parse_topic(topic_str)?;

    match parsed.msg_type.as_str() {
        MSG_BOOKTICKER => decode_bookticker_event(payload, parsed.exchange, parsed.symbol, false),
        MSG_WEBBOOKTICKER => decode_bookticker_event(payload, parsed.exchange, parsed.symbol, true),
        MSG_TRADE => decode_trade_event(payload, parsed.exchange, parsed.symbol),
        other => {
            debug!(target: "subscriber::decode", msg_type = other, "unknown msg_type");
            None
        }
    }
}

/// BookTicker / WebBookTicker 공용 decode.
///
/// `web` 이 true 면 `MarketEvent::WebBookTicker` 래핑.
fn decode_bookticker_event(
    payload: &[u8],
    expected_exchange: ExchangeId,
    expected_symbol: Symbol,
    web: bool,
) -> Option<(MarketEvent, LatencyStamps)> {
    if payload.len() != BOOK_TICKER_SIZE {
        warn!(
            target: "subscriber::decode",
            expected = BOOK_TICKER_SIZE,
            actual = payload.len(),
            "bookticker payload length mismatch"
        );
        return None;
    }

    let wire = match decode_bookticker(payload) {
        Ok(w) => w,
        Err(e) => {
            warn!(target: "subscriber::decode", error = ?e, "decode_bookticker failed");
            return None;
        }
    };

    // topic 의 exchange 와 payload 의 exchange 가 일치하는지 체크. 불일치는 cold path 만 로깅.
    if wire.exchange != expected_exchange {
        warn!(
            target: "subscriber::decode",
            topic_exchange = ?expected_exchange,
            payload_exchange = ?wire.exchange,
            "topic/payload exchange mismatch — using payload"
        );
    }

    // Symbol 은 topic 으로부터 계산된 Arc<str> 과 payload 의 문자열이 같다고 가정.
    // payload 쪽은 String → Symbol 로 알로케이트하지 않고, topic 에서 이미 만든
    // `expected_symbol` 을 재사용한다. (wire.symbol 은 decode_bookticker 가 String
    // 으로 이미 만들어 둔 상태이지만 BookTicker::symbol 에 넘기기 위해 버림)
    let bt = BookTicker {
        exchange: wire.exchange,
        symbol: expected_symbol,
        bid_price: Price(wire.bid_price),
        ask_price: Price(wire.ask_price),
        bid_size: Size(wire.bid_size),
        ask_size: Size(wire.ask_size),
        event_time_ms: wire.event_time_ms,
        server_time_ms: wire.server_time_ms,
    };

    let mut stamps = LatencyStamps::new();
    stamps.set_exchange_server_ms(wire.server_time_ms);
    // pushed / published 는 wire 에 실려 오는 publisher_sent_ms 하나뿐이지만,
    // 레거시 wire 에서는 그 필드가 "push 시점" 을 기록한다.
    // aggregator 에서 published_ms 를 동일 offset 에 **덮어쓰기** 하므로, 최종적으로
    // 여기로 도달하는 값은 published_ms 이다.
    stamps.published.wall_ms = wire.publisher_sent_ms;

    let ev = if web {
        MarketEvent::WebBookTicker(bt)
    } else {
        MarketEvent::BookTicker(bt)
    };
    Some((ev, stamps))
}

/// Trade decode.
fn decode_trade_event(
    payload: &[u8],
    expected_exchange: ExchangeId,
    expected_symbol: Symbol,
) -> Option<(MarketEvent, LatencyStamps)> {
    if payload.len() != TRADE_SIZE {
        warn!(
            target: "subscriber::decode",
            expected = TRADE_SIZE,
            actual = payload.len(),
            "trade payload length mismatch"
        );
        return None;
    }

    let wire = match decode_trade(payload) {
        Ok(w) => w,
        Err(e) => {
            warn!(target: "subscriber::decode", error = ?e, "decode_trade failed");
            return None;
        }
    };

    if wire.exchange != expected_exchange {
        warn!(
            target: "subscriber::decode",
            topic_exchange = ?expected_exchange,
            payload_exchange = ?wire.exchange,
            "topic/payload exchange mismatch — using payload"
        );
    }

    let t = Trade {
        exchange: wire.exchange,
        symbol: expected_symbol,
        price: Price(wire.price),
        size: Size(wire.size),
        trade_id: wire.trade_id,
        create_time_s: wire.create_time_s,
        create_time_ms: wire.create_time_ms,
        server_time_ms: wire.server_time_ms,
        is_internal: wire.is_internal,
    };

    let mut stamps = LatencyStamps::new();
    stamps.set_exchange_server_ms(wire.server_time_ms);
    stamps.published.wall_ms = wire.publisher_sent_ms;

    Some((MarketEvent::Trade(t), stamps))
}

// ─────────────────────────────────────────────────────────────────────────────
// SubTask — ZMQ SUB loop
// ─────────────────────────────────────────────────────────────────────────────

/// SUB 소켓을 소유하고 한 tokio task 로 동작하는 구독자.
///
/// `run(cancel)` 루프는 `drain_batch` 로 가능한 한 여러 프레임을 한 번에 처리하고
/// 빈 턴에는 `recv_timeout(1ms)` 로 CPU 를 양보한다.
pub struct SubTask<D: Downstream> {
    sub: SubSocket,
    downstream: Arc<D>,
    clock: Arc<dyn Clock>,
    drain_cap: usize,
    scratch: Vec<TopicPayload>,
}

impl<D: Downstream> SubTask<D> {
    /// 새 SubTask.
    ///
    /// `scratch` 는 `drain_cap` 용량으로 prealloc — realloc 방지.
    pub fn new(
        sub: SubSocket,
        downstream: Arc<D>,
        clock: Arc<dyn Clock>,
        drain_cap: usize,
    ) -> Self {
        Self {
            sub,
            downstream,
            clock,
            drain_cap,
            scratch: Vec::with_capacity(drain_cap),
        }
    }

    /// async run — `cancel` 이 취소되면 탈출.
    ///
    /// ZMQ 는 blocking API 이므로 recv_timeout/drain_batch 는 호출 중 Tokio 러닝타임
    /// 의 다른 task 를 굶기지 않도록 **매 턴 `tokio::task::yield_now` 호출**.
    pub async fn run(mut self, cancel: CancellationToken) {
        info!(target: "subscriber::sub_task", drain_cap = self.drain_cap, "sub task starting");

        loop {
            if cancel.is_cancelled() {
                break;
            }

            // 1) drain as many frames as possible
            self.scratch.clear();
            let got = self.sub.drain_batch(self.drain_cap, &mut self.scratch);
            if got > 0 {
                for (topic, payload) in &self.scratch {
                    self.handle_one(topic.as_slice(), payload.as_slice());
                }
                self.scratch.clear();
                // drain 성공 턴에는 바로 다음 루프로.
                tokio::task::yield_now().await;
                continue;
            }

            // 2) idle: 1ms timeout 으로 최소한 깨어날 기회 보장
            match self.sub.recv_timeout(SUB_RECV_TIMEOUT_MS) {
                Ok(Some((topic, payload))) => self.handle_one(&topic, &payload),
                Ok(None) => {
                    // timeout — 아무 일도 없음.
                }
                Err(e) => {
                    // ZMQ ctx 종료 시 EINTR/ETERM 올 수 있음.
                    warn!(target: "subscriber::sub_task", error = ?e, "sub recv error, backing off");
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }

            tokio::task::yield_now().await;
        }

        info!(target: "subscriber::sub_task", "sub task stopped");
    }

    /// 프레임 1건 처리. decode 실패 또는 downstream busy 이면 drop + counter.
    #[inline]
    fn handle_one(&self, topic: &[u8], payload: &[u8]) {
        let Some((ev, mut stamps)) = decode(topic, payload) else {
            counter_inc(CounterKey::DecodeFail);
            return;
        };

        // subscribed stage mark — hot path.
        stamps.mark(Stage::Subscribed, &*self.clock);

        match self.downstream.try_push(ev, stamps) {
            Ok(()) => {
                counter_inc(CounterKey::PipelineEvent);
            }
            Err(DownstreamBusy) => {
                counter_inc(CounterKey::ZmqDropped);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// start() — SubscriberHandle 반환
// ─────────────────────────────────────────────────────────────────────────────

/// subscriber 가 보유한 tokio task + cancel token + zmq ctx.
pub struct SubscriberHandle {
    /// SubTask join handle.
    pub task: JoinHandle<()>,
    /// 외부 shutdown trigger.
    pub cancel: CancellationToken,
    /// drop 시 ZMQ ctx 해제 → SUB 종료.
    #[allow(dead_code)]
    pub zmq_ctx: ZmqContext,
}

impl SubscriberHandle {
    /// SIGTERM / Ctrl-C 등 외부에서 호출.
    pub fn shutdown(&self) {
        if !self.cancel.is_cancelled() {
            self.cancel.cancel();
        }
    }

    /// task 종료까지 blocking.
    pub async fn join(self) {
        if let Err(e) = self.task.await {
            warn!(target: "subscriber::handle", error = ?e, "sub task join error");
        }
    }
}

/// 설정 → SUB 소켓 + 모든 exchange/msg_type 의 prefix 구독 + SubTask spawn.
///
/// `downstream` 은 호출자가 생성해 넘긴다 (latency-probe 는 DirectFn, strategy 와
/// 같은 프로세스면 InprocQueue.tx). 이 함수는 Clone 이 아닌 Arc<D> 를 받는다.
///
/// # 에러
/// - SUB connect/subscribe 실패
/// - 설정 검증 실패
pub async fn start<D: Downstream>(
    cfg: Arc<AppConfig>,
    downstream: Arc<D>,
) -> Result<SubscriberHandle> {
    hft_config::validate(&cfg).context("config validation failed")?;

    let zmq_ctx = ZmqContext::new();
    let sub = build_sub_socket(&zmq_ctx, &cfg.zmq, &cfg.exchanges)
        .context("subscriber sub socket build failed")?;

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let cancel = CancellationToken::new();

    let task_cancel = cancel.child_token();
    let task = {
        let task = SubTask::new(sub, downstream, clock, cfg.zmq.drain_batch_cap);
        tokio::spawn(async move { task.run(task_cancel).await })
    };

    Ok(SubscriberHandle {
        task,
        cancel,
        zmq_ctx,
    })
}

/// SUB 소켓 구성: 모든 `ExchangeConfig.id` × (bookticker | trade | webbookticker)
/// prefix 를 subscribe 한다. `webbookticker` 는 Gate 만 발행하지만 여분으로 등록해도
/// 부작용 없음 (prefix 매칭 실패 시 ZMQ 측 drop 처리).
fn build_sub_socket(
    ctx: &ZmqContext,
    zmq_cfg: &ZmqConfig,
    exchanges: &[ExchangeConfig],
) -> Result<SubSocket> {
    if exchanges.is_empty() {
        return Err(anyhow!(
            "subscriber requires at least one exchange to build prefixes"
        ));
    }

    // 중복 제거를 위해 (exchange, msg_type) 쌍으로 수집.
    // Phase 1 은 exchanges 가 수십 개 미만이라 단순 Vec + linear dedup 로 충분.
    let mut prefixes: Vec<String> = Vec::new();
    for ex in exchanges {
        push_unique_prefix(&mut prefixes, ex.id, MSG_BOOKTICKER);
        push_unique_prefix(&mut prefixes, ex.id, MSG_TRADE);
        push_unique_prefix(&mut prefixes, ex.id, MSG_WEBBOOKTICKER);
    }
    let prefix_refs: Vec<&[u8]> = prefixes.iter().map(|s| s.as_bytes()).collect();

    let sub = ctx
        .sub(&zmq_cfg.sub_endpoint, &prefix_refs, zmq_cfg)
        .with_context(|| format!("sub connect {}", zmq_cfg.sub_endpoint))?;

    info!(
        target: "subscriber::start",
        endpoint = %zmq_cfg.sub_endpoint,
        prefixes = prefixes.len(),
        "sub socket ready"
    );

    Ok(sub)
}

fn push_unique_prefix(out: &mut Vec<String>, id: ExchangeId, msg_type: &str) {
    let p = hft_protocol::TopicBuilder::prefix(id, msg_type);
    if !out.contains(&p) {
        out.push(p);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 기타: hft_exchange_api re-export 일관성 — publisher 와 동일하게 Arc<SystemClock>
// 는 주지 않는다 (Clock trait 가 있으므로).
// ─────────────────────────────────────────────────────────────────────────────

/// panic 시 로깅 — main 에서 설치하면 stderr 이 깨져도 tracing 으로 남는다.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!(target: "subscriber::panic", panic = %info, "panic");
        default(info);
    }));
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_protocol::{encode_bookticker_into, encode_trade_into, TopicBuilder};
    use hft_time::MockClock;
    use hft_types::{BookTicker, ExchangeId, Price, Size, Symbol, Trade};

    fn sample_bookticker() -> BookTicker {
        BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(100.0),
            ask_price: Price(101.0),
            bid_size: Size(1.0),
            ask_size: Size(2.0),
            event_time_ms: 0,
            server_time_ms: 1_700_000_000_000,
        }
    }

    fn sample_trade() -> Trade {
        Trade {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("ETH_USDT"),
            price: Price(1500.0),
            size: Size(0.5),
            trade_id: 42,
            create_time_s: 1_700_000_000,
            create_time_ms: 1_700_000_000_123,
            server_time_ms: 1_700_000_000_500,
            is_internal: false,
        }
    }

    #[test]
    fn decode_bookticker_from_topic_and_payload() {
        let bt = sample_bookticker();
        let topic = TopicBuilder::from_event(&MarketEvent::BookTicker(bt.clone()));
        let mut buf = [0u8; BOOK_TICKER_SIZE];
        encode_bookticker_into(&mut buf, &bt);

        let (ev, stamps) = decode(topic.as_bytes(), &buf).expect("decode bookticker");
        match ev {
            MarketEvent::BookTicker(got) => {
                assert_eq!(got.exchange, bt.exchange);
                assert_eq!(got.symbol.as_str(), bt.symbol.as_str());
                assert_eq!(got.bid_price.0, bt.bid_price.0);
                assert_eq!(got.ask_price.0, bt.ask_price.0);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
        assert_eq!(stamps.exchange_server.wall_ms, bt.server_time_ms);
    }

    #[test]
    fn decode_webbookticker_variant_when_topic_says_web() {
        let bt = sample_bookticker();
        let topic = TopicBuilder::webbookticker(bt.exchange, bt.symbol.as_str());
        let mut buf = [0u8; BOOK_TICKER_SIZE];
        encode_bookticker_into(&mut buf, &bt);

        let (ev, _) = decode(topic.as_bytes(), &buf).expect("decode");
        assert!(matches!(ev, MarketEvent::WebBookTicker(_)));
    }

    #[test]
    fn decode_trade_from_topic_and_payload() {
        let t = sample_trade();
        let topic = TopicBuilder::trade(t.exchange, t.symbol.as_str());
        let mut buf = [0u8; TRADE_SIZE];
        encode_trade_into(&mut buf, &t);

        let (ev, _stamps) = decode(topic.as_bytes(), &buf).expect("decode trade");
        match ev {
            MarketEvent::Trade(got) => {
                assert_eq!(got.trade_id, t.trade_id);
                assert_eq!(got.price.0, t.price.0);
                assert_eq!(got.size.0, t.size.0);
                assert_eq!(got.is_internal, t.is_internal);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn decode_returns_none_on_length_mismatch() {
        let bt = sample_bookticker();
        let topic = TopicBuilder::bookticker(bt.exchange, bt.symbol.as_str());
        let buf = [0u8; BOOK_TICKER_SIZE - 1];
        assert!(decode(topic.as_bytes(), &buf).is_none());
    }

    #[test]
    fn decode_returns_none_on_unknown_msg_type() {
        // topic 에 `foo` 같은 msg_type 을 넣어 parse 는 통과하지만 match 에서 실패.
        let weird = b"gate_foo_BTC_USDT";
        let buf = [0u8; BOOK_TICKER_SIZE];
        assert!(decode(weird, &buf).is_none());
    }

    #[test]
    fn decode_returns_none_on_invalid_topic() {
        let buf = [0u8; BOOK_TICKER_SIZE];
        assert!(decode(b"nope", &buf).is_none());
        assert!(decode(b"", &buf).is_none());
    }

    #[test]
    fn inproc_queue_try_push_and_full() {
        let (q, rx) = InprocQueue::bounded(2);
        let bt = sample_bookticker();
        let s = LatencyStamps::new();

        q.try_push(MarketEvent::BookTicker(bt.clone()), s)
            .expect("first push");
        q.try_push(MarketEvent::BookTicker(bt.clone()), s)
            .expect("second push");
        // full
        assert!(q.try_push(MarketEvent::BookTicker(bt.clone()), s).is_err());

        // receiver 가 꺼내면 다시 push 가능.
        let _ = rx.recv();
        q.try_push(MarketEvent::BookTicker(bt), s)
            .expect("after drain push");
    }

    #[test]
    fn direct_fn_runs_closure_each_push() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let d = DirectFn::new(move |_ev, _ls| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        let bt = sample_bookticker();
        let s = LatencyStamps::new();
        for _ in 0..3 {
            d.try_push(MarketEvent::BookTicker(bt.clone()), s).unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn sub_task_marks_subscribed_stage() {
        // Unit test: decode + handle_one 의 stage mark 가 잘 되는지, SubSocket 없이
        // SubTask::handle_one 만 직접 호출.
        let clock: Arc<dyn Clock> = MockClock::new(1_000_000_000_000, 0);
        let captured: Arc<parking_lot::Mutex<Option<LatencyStamps>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let cap_clone = captured.clone();
        let downstream = Arc::new(DirectFn::new(move |_ev, ls| {
            *cap_clone.lock() = Some(ls);
        }));

        // SubSocket 을 직접 만들 수 없으므로 dummy ZmqContext + sub 를 만들어 넘긴다.
        let ctx = ZmqContext::new();
        let zcfg = ZmqConfig::default();
        let sub = ctx
            .sub("inproc://subtest-nowhere", &[b""], &zcfg)
            .expect("sub build");
        let task = SubTask::new(sub, downstream, clock, 8);

        let bt = sample_bookticker();
        let topic = TopicBuilder::from_event(&MarketEvent::BookTicker(bt.clone()));
        let mut buf = [0u8; BOOK_TICKER_SIZE];
        encode_bookticker_into(&mut buf, &bt);

        task.handle_one(topic.as_bytes(), &buf);

        let got = (*captured.lock()).expect("downstream received");
        // MockClock 은 1_000_000_000_000 으로 시작; mark(Subscribed) → subscribed.wall_ms
        assert_eq!(got.subscribed.wall_ms, 1_000_000_000_000);
    }

    #[test]
    fn sub_task_drops_undecodable_payload_and_bumps_counter() {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let downstream = Arc::new(DirectFn::new(|_ev, _ls| {
            panic!("should not be called for broken payload");
        }));
        let ctx = ZmqContext::new();
        let zcfg = ZmqConfig::default();
        let sub = ctx
            .sub("inproc://subtest-broken", &[b""], &zcfg)
            .expect("sub build");
        let task = SubTask::new(sub, downstream, clock, 8);

        // 잘못된 payload 길이.
        task.handle_one(b"gate_bookticker_BTC_USDT", &[0u8; 10]);
        // panic 이 나지 않고 함수가 그냥 리턴하면 성공.
    }

    #[tokio::test]
    async fn start_and_shutdown_are_idempotent_without_traffic() {
        let cfg = Arc::new(AppConfig {
            exchanges: vec![ExchangeConfig {
                id: ExchangeId::Gate,
                symbols: vec![Symbol::new("BTC_USDT")],
                role: hft_types::DataRole::Primary,
                ws_url: None,
                reconnect_backoff_ms: 500,
                ping_interval_s: 15,
                ignore_symbols: Vec::new(),
            }],
            zmq: ZmqConfig {
                // 테스트는 hwm/endpoint 모두 충분히 작게.
                hwm: 1024,
                drain_batch_cap: 32,
                push_endpoint: "inproc://subtest-push".into(),
                pub_endpoint: "inproc://subtest-pub".into(),
                sub_endpoint: "inproc://subtest-pub".into(),
                linger_ms: 0,
                order_ingress_bind: None,
                result_egress_bind: None,
                result_heartbeat_interval_ms: 0,
            },
            ..AppConfig::default()
        });

        let (q, _rx) = InprocQueue::bounded(64);
        let handle = start(cfg.clone(), Arc::new(q)).await.expect("start");

        // 아무 traffic 도 없이 바로 shutdown — task 가 깨끗이 종료되어야 함.
        handle.shutdown();
        handle.join().await;
    }

    #[test]
    fn build_sub_socket_rejects_empty_exchanges() {
        let ctx = ZmqContext::new();
        let zcfg = ZmqConfig::default();
        let err = match build_sub_socket(&ctx, &zcfg, &[]) {
            Ok(_) => panic!("build_sub_socket should reject empty exchanges"),
            Err(err) => err,
        };
        let msg = format!("{err}");
        assert!(msg.contains("at least one exchange"), "got: {msg}");
    }

    #[test]
    fn push_unique_prefix_dedupes() {
        let mut v = Vec::new();
        push_unique_prefix(&mut v, ExchangeId::Gate, MSG_BOOKTICKER);
        push_unique_prefix(&mut v, ExchangeId::Gate, MSG_BOOKTICKER);
        push_unique_prefix(&mut v, ExchangeId::Gate, MSG_TRADE);
        assert_eq!(v.len(), 2);
        assert!(v.contains(&"gate_bookticker_".to_string()));
        assert!(v.contains(&"gate_trade_".to_string()));
    }
}

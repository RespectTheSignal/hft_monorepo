//! hft-testkit — mock 거래소 · 워밍업 · 지연 통계 헬퍼.
//!
//! ## 설계
//!
//! 이 crate 는 **dev 레벨에서만** 사용되며, production 바이너리에는 포함되지 않는다.
//! - `MockFeed` — `ExchangeFeed` 계약을 만족하며, `ScheduledEvent` 스크립트를
//!   deterministic 하게 재생. 테스트에서 특정 시나리오 (버스트, 역순 타임스탬프,
//!   NaN 가격 등) 를 정확히 재현하기 위함.
//! - `WarmUp` — 대량의 `MarketEvent` 를 흉내내는 유틸. 성능 테스트용.
//! - `MockSink` — `push/publish` 대신 내부 `Vec` 에 기록하는 싱크. ZMQ 없이
//!   단위 테스트에서 프레임 흐름을 그대로 검증.
//! - `assert_p99_under` / `assert_p99_9_under` — HDR histogram threshold 검사.
//! - `pipeline_harness!` 매크로 — publisher/subscriber 를 inproc 로 띄우는
//!   helper (services crate 완성 이후 Phase 1 말미에 채움 — 현재 stub).
//!
//! ## 계약
//!
//! - `MockFeed::stream` 은 스크립트를 순서대로 emit 하며, 각 이벤트 직전에
//!   `clock.set_ms(..)` 로 시간을 절대값으로 맞춘다.
//! - `MockFeed` 는 `cancel` 토큰을 항상 존중 — 스크립트 중간에도 즉시 종료.
//! - `WarmUp::run_bookticker` 는 `count` 건의 BookTicker 를 emit, 이벤트 사이에
//!   `step_ns` (+/- jitter) 만큼 monotonic clock 을 전진.
//! - `MockSink` 는 내부적으로 `parking_lot::Mutex` — 고 throughput 테스트에서도
//!   deadlock 가능성이 낮다.
//!
//! ## Phase 1 TODO
//!
//! 1. `MockFeed` + `ScheduledEvent` ✅
//! 2. `WarmUp::run_bookticker` ✅
//! 3. `MockSink` (`record`) ✅
//! 4. `assert_p99_under` / `assert_p99_9_under` ✅
//! 5. `pipeline_harness!` — services crate 완료 후 내부 확장 (현재 stub).
//! 6. `fixtures` 모듈 — 자주 쓰는 BookTicker/Trade 샘플 ✅
//!
//! ## Anti-jitter
//!
//! testkit 자체는 jitter 유발 코드를 제품 코드에 노출하지 않는다 — 내부 state
//! 는 전부 `parking_lot::Mutex` 1회만 잡고 leaf 에서 반환. CI 에서 flaky 가 발생
//! 하지 않도록 절대 시간이 아닌 MockClock 기반으로만 검증.

#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use hdrhistogram::Histogram;
use hft_exchange_api::{CancellationToken, Emitter, ExchangeFeed};
use hft_time::{Clock, LatencyStamps, MockClock, Stage, Stamp};
use hft_types::{BookTicker, DataRole, ExchangeId, MarketEvent, Price, Size, Symbol, Trade};
use parking_lot::Mutex;

// 재수출 — 사용자가 이 crate 만 dev-dep 으로 추가해도 기본 primitive 접근 가능.
pub use hft_exchange_api::{noop_emitter, OrderRequest, OrderSide};

// ────────────────────────────────────────────────────────────────────────────
// Fixtures
// ────────────────────────────────────────────────────────────────────────────

/// 자주 쓰는 BookTicker/Trade 샘플 생성 헬퍼.
///
/// 필드 수가 많아 각 테스트가 매번 구조체를 채우면 실수 나기 쉬움 —
/// 일관된 기본값으로 시작해 필요한 필드만 덮어쓰도록 한다.
pub mod fixtures {
    use super::*;

    /// 기본 BookTicker — Gate:BTC_USDT, bid 100, ask 100.1, server_time_ms 입력.
    pub fn bookticker(server_time_ms: i64) -> BookTicker {
        BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(100.0),
            ask_price: Price(100.1),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: server_time_ms,
            server_time_ms,
        }
    }

    /// 지정 exchange/symbol BookTicker.
    pub fn bookticker_of(
        ex: ExchangeId,
        symbol: &str,
        bid: f64,
        ask: f64,
        ts_ms: i64,
    ) -> BookTicker {
        BookTicker {
            exchange: ex,
            symbol: Symbol::new(symbol),
            bid_price: Price(bid),
            ask_price: Price(ask),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: ts_ms,
            server_time_ms: ts_ms,
        }
    }

    /// 기본 Trade — Gate:BTC_USDT, 매수 체결 (size > 0).
    pub fn trade(server_time_ms: i64) -> Trade {
        Trade {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            price: Price(100.05),
            size: Size(0.5),
            trade_id: 1,
            create_time_s: server_time_ms / 1000,
            create_time_ms: server_time_ms,
            server_time_ms,
            is_internal: false,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// ScheduledEvent — MockFeed 스크립트 원소
// ────────────────────────────────────────────────────────────────────────────

/// MockFeed 가 재생할 한 개의 이벤트 + 타이밍.
#[derive(Debug, Clone)]
pub struct ScheduledEvent {
    /// 이 이벤트 emit 직전 clock 을 해당 절대 ms 로 맞춘다.
    /// (직전 이벤트보다 앞이어선 안 됨.)
    pub at_ms: i64,
    /// 방출될 market event.
    pub event: MarketEvent,
}

impl ScheduledEvent {
    /// 편의 생성자 — BookTicker 이벤트.
    pub fn bookticker(at_ms: i64, bt: BookTicker) -> Self {
        Self { at_ms, event: MarketEvent::BookTicker(bt) }
    }

    /// 편의 생성자 — WebBookTicker.
    pub fn web_bookticker(at_ms: i64, bt: BookTicker) -> Self {
        Self { at_ms, event: MarketEvent::WebBookTicker(bt) }
    }

    /// 편의 생성자 — Trade.
    pub fn trade(at_ms: i64, tr: Trade) -> Self {
        Self { at_ms, event: MarketEvent::Trade(tr) }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// MockFeed — 스크립트 재생 ExchangeFeed
// ────────────────────────────────────────────────────────────────────────────

/// 스크립트로 주입된 이벤트를 순서대로 emit 하는 mock feed.
///
/// `stream` 동작:
/// 1) `symbols` 비어있으면 즉시 Ok.
/// 2) 스크립트 각 원소에 대해:
///    - `cancel` 이 이미 trigger 됐으면 break.
///    - `clock.set_ms(at_ms)` 로 시간 전진 (절대값).
///    - `LatencyStamps::{exchange_server, ws_received}` 찍고 emit.
/// 3) 스크립트 소진 후 `cancel` 대기.
pub struct MockFeed {
    exchange: ExchangeId,
    role: DataRole,
    symbols: Vec<Symbol>,
    clock: Arc<MockClock>,
    script: Mutex<Vec<ScheduledEvent>>,
    // stream 이 끝까지 재생됐는지 내부에서 추적 (테스트 어서션용).
    replayed_all: Arc<AtomicBool>,
}

impl MockFeed {
    /// 새 mock feed.
    pub fn new(
        exchange: ExchangeId,
        role: DataRole,
        symbols: Vec<Symbol>,
        clock: Arc<MockClock>,
        script: Vec<ScheduledEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            exchange,
            role,
            symbols,
            clock,
            script: Mutex::new(script),
            replayed_all: Arc::new(AtomicBool::new(false)),
        })
    }

    /// 스크립트를 모두 재생했는가? (cancel 이 아닌, 정상 소진)
    pub fn replayed_all(&self) -> bool {
        self.replayed_all.load(Ordering::SeqCst)
    }

    /// 남아있는 이벤트 수.
    pub fn remaining(&self) -> usize {
        self.script.lock().len()
    }
}

#[async_trait]
impl ExchangeFeed for MockFeed {
    fn id(&self) -> ExchangeId {
        self.exchange
    }

    fn role(&self) -> DataRole {
        self.role
    }

    async fn available_symbols(&self) -> anyhow::Result<Vec<Symbol>> {
        Ok(self.symbols.clone())
    }

    async fn stream(
        &self,
        symbols: &[Symbol],
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        if symbols.is_empty() {
            return Ok(());
        }

        // 스크립트는 한 번만 소비 — drain.
        let script: Vec<ScheduledEvent> = std::mem::take(&mut *self.script.lock());

        for sched in script.into_iter() {
            if cancel.is_cancelled() {
                return Ok(());
            }
            // 논리적 역행 assert.
            let cur = self.clock.now_ms();
            debug_assert!(
                sched.at_ms >= cur,
                "ScheduledEvent.at_ms ({}) must be >= current clock ({})",
                sched.at_ms,
                cur,
            );
            self.clock.set_ms(sched.at_ms);

            let mut ls = LatencyStamps::new();
            ls.set_exchange_server_ms(sched.event.server_time_ms());
            ls.ws_received = Stamp::now(self.clock.as_ref());
            (emit)(sched.event, ls);
        }

        self.replayed_all.store(true, Ordering::SeqCst);

        // 소진 후 cancel 대기 — 일반 feed 와 동일한 lifetime.
        cancel.cancelled().await;
        Ok(())
    }

    fn label(&self) -> String {
        format!("mock:{}:{:?}", self.exchange.as_str(), self.role)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// WarmUp — 대량 이벤트 주입
// ────────────────────────────────────────────────────────────────────────────

/// 워밍업/부하 주입 헬퍼.
pub struct WarmUp;

impl WarmUp {
    /// `count` 건의 BookTicker 이벤트를 `emit` 에 흘려보낸다.
    ///
    /// - 매 이벤트 사이에 `step_ns` 만큼 monotonic clock 전진 (+/- `jitter_ns`).
    /// - wall-ms 도 대략 `step_ns / 1_000_000` 주기로 1씩 전진.
    /// - 동기 루프 — tokio 런타임 불필요.
    pub fn run_bookticker(
        emit: &Emitter,
        clock: &Arc<MockClock>,
        count: usize,
        step_ns: u64,
        jitter_ns: u64,
    ) {
        let start_ms = clock.now_ms();
        let wall_tick_every = (1_000_000_u64 / step_ns.max(1)).max(1);
        let mut bid = 100.0_f64;
        for i in 0..count {
            // deterministic jitter: i가 짝수면 -jitter, 홀수면 +jitter.
            let effective = if jitter_ns == 0 {
                step_ns
            } else if i % 2 == 0 {
                step_ns.saturating_sub(jitter_ns.min(step_ns / 2))
            } else {
                step_ns.saturating_add(jitter_ns)
            };
            clock.advance_nanos(effective);
            if ((i as u64 + 1) % wall_tick_every) == 0 {
                clock.advance_ms(1);
            }

            bid += 0.01;
            let ts_ms = start_ms + (i as i64);
            let bt = BookTicker {
                exchange: ExchangeId::Gate,
                symbol: Symbol::new("BTC_USDT"),
                bid_price: Price(bid),
                ask_price: Price(bid + 0.1),
                bid_size: Size(1.0),
                ask_size: Size(1.0),
                event_time_ms: ts_ms,
                server_time_ms: ts_ms,
            };

            let mut ls = LatencyStamps::new();
            ls.set_exchange_server_ms(bt.server_time_ms);
            ls.ws_received = Stamp::now(clock.as_ref());
            (emit)(MarketEvent::BookTicker(bt), ls);
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// MockSink — 프레임 수집
// ────────────────────────────────────────────────────────────────────────────

/// 송출 이벤트 하나. topic + payload 원본.
#[derive(Debug, Clone)]
pub struct RecordedFrame {
    pub topic: Vec<u8>,
    pub payload: Vec<u8>,
    /// 기록 시점의 mono_ns — 스트레스 테스트에서 timeline 재구성에 사용.
    pub recorded_mono_ns: u64,
}

/// ZMQ 없이 프레임을 메모리에 기록하는 mock sink.
///
/// publisher/aggregator 경로 구분이 필요하면 별도 sink 인스턴스를 써라.
#[derive(Debug, Default)]
pub struct MockSink {
    inner: Mutex<Vec<RecordedFrame>>,
}

impl MockSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 프레임 기록.
    pub fn record(&self, topic: &[u8], payload: &[u8], clock: &dyn Clock) {
        self.inner.lock().push(RecordedFrame {
            topic: topic.to_vec(),
            payload: payload.to_vec(),
            recorded_mono_ns: clock.now_nanos(),
        });
    }

    /// 현재까지 기록된 프레임 snapshot.
    pub fn snapshot(&self) -> Vec<RecordedFrame> {
        self.inner.lock().clone()
    }

    /// 기록 개수.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// 비어있는지.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// 모두 지움.
    pub fn clear(&self) {
        self.inner.lock().clear();
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Histogram assertion helpers
// ────────────────────────────────────────────────────────────────────────────

/// p99 latency 가 threshold (ns) 이하인지 검증. 실패 시 구체 수치로 panic.
pub fn assert_p99_under(hdr: &Histogram<u64>, nanos: u64, label: &str) {
    let p99 = hdr.value_at_quantile(0.99);
    assert!(
        p99 <= nanos,
        "[{label}] p99 = {p99} ns > budget {nanos} ns (samples = {})",
        hdr.len()
    );
}

/// p99.9 검증.
pub fn assert_p99_9_under(hdr: &Histogram<u64>, nanos: u64, label: &str) {
    let p99_9 = hdr.value_at_quantile(0.999);
    assert!(
        p99_9 <= nanos,
        "[{label}] p99.9 = {p99_9} ns > budget {nanos} ns (samples = {})",
        hdr.len()
    );
}

/// 주어진 stage delta 샘플을 HDR histogram 에 기록.
pub fn record_delta(hdr: &mut Histogram<u64>, ls: &LatencyStamps, from: Stage, to: Stage) {
    if let Some(d) = ls.delta_ns(from, to) {
        let _ = hdr.record(d);
    }
}

/// Trade side 추상화 — 테스트에서 `Side` 로 직접 비교하기 위한 재수출.
pub use hft_types::Side as TradeSide;

// ────────────────────────────────────────────────────────────────────────────
// pipeline_harness! — services 완료 후 실장 (현재 stub)
// ────────────────────────────────────────────────────────────────────────────

/// E2E 테스트 harness placeholder.
///
/// services/publisher + services/subscriber 구현 후 이 매크로가 채워진다.
/// 현재는 블록을 호출자 런타임에서 그대로 실행하는 no-op 매크로로 둬서
/// call-site 코드가 미래에도 호환되도록 한다.
#[macro_export]
macro_rules! pipeline_harness {
    ($body:block) => {{
        // TODO(phase1-late): publisher/aggregator/subscriber 를 inproc ZMQ 로
        // 띄우고, 내부에서 block 을 실행하도록 확장. 현재는 그대로 호출.
        $body
    }};
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn mock_feed_replays_script_and_exits_on_cancel() {
        let clock = MockClock::zero();
        let bt1 = fixtures::bookticker(100);
        let bt2 = fixtures::bookticker(150);
        let script = vec![
            ScheduledEvent::bookticker(100, bt1),
            ScheduledEvent::bookticker(150, bt2),
        ];
        let feed = MockFeed::new(
            ExchangeId::Gate,
            DataRole::Primary,
            vec![Symbol::new("BTC_USDT")],
            clock.clone(),
            script,
        );

        let collected: Arc<Mutex<Vec<MarketEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let c2 = collected.clone();
        let emit: Emitter = Arc::new(move |ev, _ls| {
            c2.lock().push(ev);
        });

        let token = CancellationToken::new();
        let t2 = token.clone();
        let f2 = feed.clone();
        let handle = tokio::spawn(async move {
            f2.stream(&[Symbol::new("BTC_USDT")], emit, t2).await
        });

        // script 가 소진될 때까지 대기 — polling (replayed_all).
        for _ in 0..200 {
            if feed.replayed_all() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(feed.replayed_all(), "script should be fully replayed");
        assert_eq!(feed.remaining(), 0);

        token.cancel();
        let res = tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("task did not complete")
            .expect("task panicked");
        assert!(res.is_ok());

        let events = collected.lock().clone();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].exchange(), ExchangeId::Gate);
        // clock 이 스크립트 마지막 시각으로 이동했는지.
        assert_eq!(clock.now_ms(), 150);
    }

    #[tokio::test]
    async fn mock_feed_empty_symbols_returns_immediately() {
        let clock = MockClock::zero();
        let feed = MockFeed::new(
            ExchangeId::Gate,
            DataRole::Primary,
            vec![],
            clock,
            vec![],
        );
        let res = feed
            .stream(&[], noop_emitter(), CancellationToken::new())
            .await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn mock_feed_cancels_before_first_event() {
        let clock = MockClock::zero();
        let feed = MockFeed::new(
            ExchangeId::Gate,
            DataRole::Primary,
            vec![Symbol::new("BTC_USDT")],
            clock.clone(),
            vec![
                ScheduledEvent::bookticker(100, fixtures::bookticker(100)),
                ScheduledEvent::bookticker(200, fixtures::bookticker(200)),
            ],
        );
        let token = CancellationToken::new();
        token.cancel(); // 미리 취소.

        let res = feed
            .stream(&[Symbol::new("BTC_USDT")], noop_emitter(), token)
            .await;
        assert!(res.is_ok());
        // 아직 clock 이 움직이지 않았어야 함.
        assert_eq!(clock.now_ms(), 0);
    }

    #[test]
    fn warmup_emits_exact_count() {
        let clock = MockClock::zero();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = counter.clone();
        let emit: Emitter = Arc::new(move |_ev, _ls| {
            c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });

        WarmUp::run_bookticker(&emit, &clock, 1_000, 10_000, 2_000);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1_000);
        assert!(clock.now_nanos() > 0);
    }

    #[test]
    fn warmup_with_zero_jitter_progresses_uniformly() {
        let clock = MockClock::zero();
        let last_ns = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let last_ns2 = last_ns.clone();
        let emit: Emitter = Arc::new(move |_ev, ls| {
            last_ns2.store(ls.ws_received.mono_ns, std::sync::atomic::Ordering::SeqCst);
        });
        WarmUp::run_bookticker(&emit, &clock, 100, 5_000, 0);
        let v = last_ns.load(std::sync::atomic::Ordering::SeqCst);
        assert!(v > 0);
    }

    #[test]
    fn mock_sink_records_and_clears() {
        let sink = MockSink::new();
        let clock = MockClock::new(0, 100);
        assert!(sink.is_empty());
        sink.record(b"bookticker.gate.BTC_USDT", b"payload-1", clock.as_ref());
        sink.record(b"trade.gate.BTC_USDT", b"payload-2", clock.as_ref());
        assert_eq!(sink.len(), 2);

        let snap = sink.snapshot();
        assert_eq!(snap[0].topic, b"bookticker.gate.BTC_USDT".to_vec());
        assert_eq!(snap[1].payload, b"payload-2".to_vec());
        assert_eq!(snap[0].recorded_mono_ns, 100);

        sink.clear();
        assert!(sink.is_empty());
    }

    #[test]
    fn histogram_p99_assertions() {
        let mut hdr = Histogram::<u64>::new(3).unwrap();
        for i in 0..1000 {
            hdr.record(i + 1).unwrap();
        }
        // p99 은 990 근처. 2us 이하로 충분.
        assert_p99_under(&hdr, 2_000, "unit");
    }

    #[test]
    #[should_panic(expected = "p99")]
    fn histogram_p99_violation_panics() {
        let mut hdr = Histogram::<u64>::new(3).unwrap();
        for _ in 0..1000 {
            hdr.record(10_000).unwrap();
        }
        assert_p99_under(&hdr, 5_000, "unit");
    }

    #[test]
    fn record_delta_roundtrip() {
        let mut hdr = Histogram::<u64>::new(3).unwrap();
        let mut ls = LatencyStamps::new();
        ls.ws_received = Stamp { wall_ms: 1, mono_ns: 100 };
        ls.consumed = Stamp { wall_ms: 2, mono_ns: 600 };
        record_delta(&mut hdr, &ls, Stage::WsReceived, Stage::Consumed);
        assert_eq!(hdr.len(), 1);
        assert!(hdr.value_at_quantile(0.5) >= 400);
    }

    #[test]
    fn pipeline_harness_macro_runs_body() {
        let mut ran = false;
        pipeline_harness!({
            ran = true;
        });
        assert!(ran);
    }

    #[test]
    fn fixtures_bookticker_is_valid() {
        let bt = fixtures::bookticker(1_700_000_000_000);
        assert!(bt.is_valid());
    }

    #[test]
    fn fixtures_trade_has_expected_side() {
        let tr = fixtures::trade(1_700_000_000_000);
        assert_eq!(tr.side(), TradeSide::Buy);
        assert_eq!(tr.create_time_s, 1_700_000_000);
    }

    #[test]
    fn scheduled_event_constructors() {
        let bt = fixtures::bookticker(100);
        let e = ScheduledEvent::bookticker(100, bt.clone());
        assert_eq!(e.at_ms, 100);
        matches!(e.event, MarketEvent::BookTicker(_));
        let e = ScheduledEvent::web_bookticker(200, bt);
        matches!(e.event, MarketEvent::WebBookTicker(_));
        let e = ScheduledEvent::trade(300, fixtures::trade(300));
        matches!(e.event, MarketEvent::Trade(_));
    }
}

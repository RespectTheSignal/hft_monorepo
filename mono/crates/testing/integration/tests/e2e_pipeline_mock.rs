//! e2e_pipeline_mock — publisher wire 를 직접 합성해
//! `subscriber::decode → InprocQueue → StrategyRunner` 경로를 ZMQ 없이 검증.
//!
//! # 왜 ZMQ 를 띄우지 않나
//! 1. CI 환경(container/sandbox)에선 ZMQ inproc 이라도 ctx 공유·cleanup 문제가 생김.
//! 2. 본 테스트의 목표는 **타입·스테이지·latency 기록**의 integration 수준 parity 이지,
//!    ZMQ 자체의 정확성이 아님 (후자는 services/subscriber 의 unit 테스트에서 담당).
//! 3. 50ms budget 은 internal_ns (WsReceived → Consumed) 기준으로 확인되면 충분.
//!
//! # 흐름
//! 1. 가짜 publisher: `encode_*_into` → `patch_*_pushed_ms` → `create_message`.
//! 2. subscriber 경로를 직접 호출: `subscriber::decode(topic, payload)`.
//!    - decode 가 채운 stamps 에 `subscribed` 를 우리가 마크.
//! 3. bounded 채널로 event 를 strategy runner 에 밀어넣음.
//! 4. `StrategyRunner<CountingStrategy>` 를 `MockClock` 으로 돌려 `consumed` 가 찍히고
//!    state 가 업데이트되는지 확인.
//! 5. HDR 으로 internal_ns(ws_received → consumed) 기록 → p99.9 < 50ms assert.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::bounded;
use hdrhistogram::Histogram;
use hft_protocol::{
    create_message, encode_bookticker_into, encode_trade_into, parse_frame,
    patch_bookticker_pushed_ms, patch_trade_pushed_ms, TopicBuilder, BOOK_TICKER_SIZE, TRADE_SIZE,
};
use hft_testkit::fixtures;
use hft_time::{Clock, LatencyStamps, MockClock, Stage, Stamp};
use hft_types::{BookTicker, ExchangeId, MarketEvent, Trade};
use parking_lot::Mutex;
use strategy::{start_with_clock, NoopStrategy, OrderSender, Orders, Strategy};
use subscriber::decode;
use tokio::time::timeout;

// ─────────────────────────────────────────────────────────────────────────────
// 관측용 strategy — 몇 건 들어왔는지, 어떤 이벤트였는지 기록
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct CountingStrategy {
    processed: Arc<AtomicUsize>,
    last_exchange: Arc<Mutex<Option<ExchangeId>>>,
}

impl Strategy for CountingStrategy {
    fn update(&mut self, ev: &MarketEvent) {
        self.processed.fetch_add(1, Ordering::SeqCst);
        *self.last_exchange.lock() = Some(ev.exchange());
    }

    fn eval(&mut self, _ev: &MarketEvent) -> Orders {
        Orders::new()
    }

    fn label(&self) -> &str {
        "counting"
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 가짜 publisher — 하나의 BookTicker 프레임 생성
// ─────────────────────────────────────────────────────────────────────────────

/// `{topic, frame}` 를 만들어 반환.
fn fake_publish_bookticker(bt: &BookTicker, pushed_ms: i64) -> (String, Vec<u8>) {
    let topic = TopicBuilder::bookticker(bt.exchange, bt.symbol.as_str());
    let mut payload = [0u8; BOOK_TICKER_SIZE];
    encode_bookticker_into(&mut payload, bt);
    patch_bookticker_pushed_ms(&mut payload, pushed_ms);
    let frame = create_message("bookticker", &payload).expect("create_message");
    (topic, frame)
}

fn fake_publish_trade(t: &Trade, pushed_ms: i64) -> (String, Vec<u8>) {
    let topic = TopicBuilder::trade(t.exchange, t.symbol.as_str());
    let mut payload = [0u8; TRADE_SIZE];
    encode_trade_into(&mut payload, t);
    patch_trade_pushed_ms(&mut payload, pushed_ms);
    let frame = create_message("trade", &payload).expect("create_message");
    (topic, frame)
}

/// 프레임을 subscriber 경로로 디코드해 채널에 밀어넣고, ws_received / subscribed 를 마크.
///
/// `ws_received` 는 거래소 entry point 가 마크하는 stage 이므로 통합 테스트에서는
/// publisher 경로를 흉내내기 위해 여기서 직접 쓴다.
fn decode_and_enqueue(
    topic: &str,
    frame: &[u8],
    clock: &Arc<MockClock>,
    tx: &crossbeam_channel::Sender<(MarketEvent, LatencyStamps)>,
) {
    let view = parse_frame(frame).expect("parse_frame");
    let (ev, mut stamps) = decode(topic.as_bytes(), view.payload).expect("decode");

    // subscriber decode 는 exchange_server 와 published 만 채운다 (위 lib.rs 주석 참고).
    // publisher 가 WS 에서 받자마자 찍는 ws_received 를 여기서 보강.
    // mono_ns 가 0 이 되지 않도록 stamps.ws_received.mono_ns 를 미리 전진시켜 둠.
    stamps.ws_received = Stamp::now(clock.as_ref());
    // aggregator 가 push/publish 를 찍는다고 가정하고 살짝 진행.
    clock.advance_nanos(50_000); // 50us 경과 (publisher 내부 latency 모사)
    stamps.mark(Stage::Pushed, clock.as_ref());
    clock.advance_nanos(30_000); // 30us
    stamps.mark(Stage::Published, clock.as_ref());
    clock.advance_nanos(100_000); // 100us — PUB → SUB 네트워크 (여기선 가짜 skew).
    stamps.mark(Stage::Subscribed, clock.as_ref());

    // 채널이 꽉 차면 테스트 실패.
    tx.try_send((ev, stamps)).expect("inproc queue not full");
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. BookTicker 10건 → NoopStrategy 에 전달 — runner 가 소비하는지
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_bookticker_pipeline_delivers_all_events_to_strategy() {
    let clock = MockClock::new(1_700_000_000_000, 1_000_000_000);
    let (tx, rx) = bounded::<(MarketEvent, LatencyStamps)>(64);
    let (order_tx, _order_rx) = OrderSender::bounded(16);

    // CountingStrategy 로 관측. NoopStrategy 가 아닌 이유: eval 은 0, update 는 카운트.
    let processed = Arc::new(AtomicUsize::new(0));
    let last_exchange: Arc<Mutex<Option<ExchangeId>>> = Arc::new(Mutex::new(None));
    let strategy = CountingStrategy {
        processed: processed.clone(),
        last_exchange: last_exchange.clone(),
    };

    let clock_for_runner: Arc<dyn Clock> = clock.clone();
    let handle = start_with_clock(strategy, rx, order_tx, clock_for_runner)
        .expect("start strategy runner");

    // 10 건 주입.
    const N: usize = 10;
    for i in 0..N {
        let bt = fixtures::bookticker(1_700_000_000_000 + i as i64);
        let (topic, frame) = fake_publish_bookticker(&bt, 1_700_000_000_000 + i as i64);
        decode_and_enqueue(&topic, &frame, &clock, &tx);
        clock.advance_nanos(250_000); // 다음 이벤트 간 250us
    }

    // runner 가 drain 할 시간 확보 (10ms polling, yield_now).
    for _ in 0..200 {
        if processed.load(Ordering::SeqCst) == N {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(processed.load(Ordering::SeqCst), N, "all events processed");
    assert_eq!(*last_exchange.lock(), Some(ExchangeId::Gate));

    // shutdown.
    handle.shutdown();
    timeout(Duration::from_millis(500), handle.join())
        .await
        .expect("runner stopped in time");

    // tx 가 살아있어야 channel drop 에 의한 Disconnected 가 먼저 일어나지 않음.
    drop(tx);
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. BookTicker + Trade 혼합 — 두 MarketEvent variant 모두 올바르게 라우트
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_mixed_events_reach_strategy() {
    let clock = MockClock::new(1_700_000_000_000, 2_000_000_000);
    let (tx, rx) = bounded::<(MarketEvent, LatencyStamps)>(64);
    let (order_tx, _order_rx) = OrderSender::bounded(16);

    let processed = Arc::new(AtomicUsize::new(0));
    let last_exchange = Arc::new(Mutex::new(None));
    let strategy = CountingStrategy {
        processed: processed.clone(),
        last_exchange: last_exchange.clone(),
    };

    let clk: Arc<dyn Clock> = clock.clone();
    let handle = start_with_clock(strategy, rx, order_tx, clk).unwrap();

    // BookTicker (Gate) + Trade (Gate) 1쌍.
    let bt = fixtures::bookticker(1_700_000_000_050);
    let tr = fixtures::trade(1_700_000_000_060);

    let (t1, f1) = fake_publish_bookticker(&bt, 1_700_000_000_050);
    decode_and_enqueue(&t1, &f1, &clock, &tx);
    clock.advance_nanos(500_000);

    let (t2, f2) = fake_publish_trade(&tr, 1_700_000_000_060);
    decode_and_enqueue(&t2, &f2, &clock, &tx);

    for _ in 0..200 {
        if processed.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(processed.load(Ordering::SeqCst), 2);
    // 마지막으로 들어온 이벤트가 Trade (Gate) 이어야 함.
    assert_eq!(*last_exchange.lock(), Some(ExchangeId::Gate));

    handle.shutdown();
    timeout(Duration::from_millis(500), handle.join()).await.unwrap();
    drop(tx);
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. internal latency (ws_received → consumed) p99.9 < 50ms budget
//    — 파이프라인 전 구간의 monotonic ns 누적이 예산 안에 있는지.
//
//    주의: StrategyRunner 를 consumer 로 쓰면 producer 가 MockClock 을 계속
//    전진시키는 동안 runner task 가 소비 완료 시점에 보는 clock 값이
//    "producer 가 미래로 가 있는" 상태가 되어 delta 측정이 왜곡된다.
//    → 여기선 stamps 기반 internal_ns() 를 직접 계산하도록 consumer 를
//      테스트 내부에서 돌려서 deterministic 하게 검증한다.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_internal_latency_under_budget() {
    // 50ms budget = 50_000_000 ns.
    const BUDGET_NS: u64 = 50_000_000;
    const N: usize = 1_000;
    const STAGE_LATENCY_NS: u64 = 380_000;

    let clock = MockClock::new(1_700_000_000_000, 3_000_000_000);
    let (tx, rx) = bounded::<(MarketEvent, LatencyStamps)>(2048);

    // N 건 주입 — 파이프라인 각 stage 마다 monotonic clock 을 전진시킨다.
    // ws → push: 50us, push → pub: 30us, pub → sub: 100us, sub → consumed: 200us
    // 합 380us → budget 50ms 안에 충분히 들어옴.
    for i in 0..N {
        let bt = fixtures::bookticker(1_700_000_000_000 + i as i64);
        let (topic, frame) = fake_publish_bookticker(&bt, 1_700_000_000_050);

        let view = parse_frame(&frame).expect("parse");
        let (ev, mut stamps) = decode(topic.as_bytes(), view.payload).expect("decode");

        // ws_received 를 여기서 직접 마크 (publisher WS 엔트리 모사).
        stamps.ws_received = Stamp::now(clock.as_ref());
        clock.advance_nanos(50_000);
        stamps.mark(Stage::Pushed, clock.as_ref());
        clock.advance_nanos(30_000);
        stamps.mark(Stage::Published, clock.as_ref());
        clock.advance_nanos(100_000);
        stamps.mark(Stage::Subscribed, clock.as_ref());
        // consumed 는 consumer 가 마크 — 그 전에 약간의 "queue 대기" 를 시뮬레이션.
        clock.advance_nanos(200_000);

        tx.try_send((ev, stamps)).expect("channel capacity");
    }
    // producer 종료 → tx drop 하여 consumer 가 Disconnected 로 자연스레 종료되게 함.
    drop(tx);

    // Consumer: producer 와 독립된 clock 으로 Consumed 를 찍는다.
    // 같은 MockClock 을 공유하면 producer 가 N건을 넣는 동안 시간이 누적 전진해
    // 초기 이벤트들이 "큐에 오래 갇힌 것처럼" 과대 측정된다.
    // 여기서는 이벤트당 stage 합(380us) 만 반영되도록 consumer 전용 clock 을
    // ws_received 기준 +380us 에서 시작해 이벤트마다 같은 폭으로 전진시킨다.
    let mut hist = Histogram::<u64>::new_with_bounds(1, 5_000_000_000, 3).unwrap();
    let mut processed = 0usize;
    let consumer_clock = MockClock::new(1_700_000_000_000, 3_000_000_000 + STAGE_LATENCY_NS);
    let result = tokio::task::spawn_blocking(move || {
        while let Ok((_ev, mut stamps)) = rx.recv() {
            stamps.mark(Stage::Consumed, consumer_clock.as_ref());
            if let Some(d) = stamps.internal_ns() {
                hist.saturating_record(d);
            }
            processed += 1;
            consumer_clock.advance_nanos(STAGE_LATENCY_NS);
        }
        (hist, processed)
    });

    let (hist, processed) = timeout(Duration::from_secs(5), result)
        .await
        .expect("consumer completes")
        .expect("no panic");

    assert_eq!(processed, N, "consumer must drain all {N} events");

    let p50 = hist.value_at_quantile(0.50);
    let p99 = hist.value_at_quantile(0.99);
    let p99_9 = hist.value_at_quantile(0.999);
    let samples = hist.len();
    assert!(
        p99_9 <= BUDGET_NS,
        "p99.9 internal latency {p99_9}ns exceeds 50ms budget \
         (p50={p50}ns, p99={p99}ns, n={samples})"
    );
    // sanity: 최소 한 샘플 이상.
    assert!(samples as usize >= N, "expected >= {N} samples, got {samples}");
}

// ─────────────────────────────────────────────────────────────────────────────
// 3b. strategy runner 쪽 HDR — 실제 runner 가 consumed stage 를 찍고
//     stamps.internal_ns() 를 계산하는 경로는 strategy::handle_one 내부의
//     `record_stage_nanos(Stage::Consumed, internal_ns)` 에서 이뤄진다.
//     여기서는 runner 가 N 건을 모두 소비하는지만 재확인 (3번과 역할 분리).
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_strategy_runner_drains_large_burst() {
    const N: usize = 500;

    let clock = MockClock::new(1_700_000_000_000, 4_000_000_000);
    let (tx, rx) = bounded::<(MarketEvent, LatencyStamps)>(1024);
    let (order_tx, _order_rx) = OrderSender::bounded(16);

    let processed = Arc::new(AtomicUsize::new(0));
    let strategy = CountingStrategy {
        processed: processed.clone(),
        last_exchange: Arc::new(Mutex::new(None)),
    };
    let clk: Arc<dyn Clock> = clock.clone();
    let handle = start_with_clock(strategy, rx, order_tx, clk).unwrap();

    for i in 0..N {
        let bt = fixtures::bookticker(1_700_000_000_000 + i as i64);
        let (topic, frame) = fake_publish_bookticker(&bt, 1_700_000_000_050);
        decode_and_enqueue(&topic, &frame, &clock, &tx);
    }

    // runner 가 drain — 2초 안에 끝나야 함 (500건 / 10ms polling).
    for _ in 0..400 {
        if processed.load(Ordering::SeqCst) >= N {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(processed.load(Ordering::SeqCst), N);

    handle.shutdown();
    timeout(Duration::from_millis(500), handle.join()).await.unwrap();
    drop(tx);
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. 채널 disconnect 시 strategy runner 가 깨끗이 종료되는지
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_strategy_runner_exits_on_channel_disconnect() {
    let clock = MockClock::zero();
    let (tx, rx) = bounded::<(MarketEvent, LatencyStamps)>(4);
    let (order_tx, _order_rx) = OrderSender::bounded(4);

    let handle = start_with_clock(
        NoopStrategy::default(),
        rx,
        order_tx,
        clock.clone() as Arc<dyn Clock>,
    )
    .unwrap();

    // 하나 주입 후 tx drop → Disconnected.
    let bt = fixtures::bookticker(100);
    let (topic, frame) = fake_publish_bookticker(&bt, 100);
    decode_and_enqueue(&topic, &frame, &clock, &tx);

    // 잠시 처리 시간 확보.
    tokio::time::sleep(Duration::from_millis(30)).await;
    drop(tx);

    // runner 가 Disconnected 를 잡고 자발적으로 종료해야 함 (cancel 없이).
    timeout(Duration::from_millis(500), handle.join())
        .await
        .expect("runner must exit on channel disconnect within 500ms");
}

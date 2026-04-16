//! strategy — `MarketEvent` 소비 → `OrderRequest` 생성 파이프라인.
//!
//! # 역할 (SPEC.md)
//! - subscriber 의 `InprocQueue` 에서 `(MarketEvent, LatencyStamps)` 를 받는다.
//! - `stamps.consumed` 을 현재 시각으로 찍고 HDR histogram 에 stage=Consumed 기록.
//! - `Strategy::update(&ev)` 로 내부 state 갱신, `Strategy::eval(&ev)` 로 주문 생성.
//! - 생성된 `OrderRequest` 는 order-gateway 채널 (crossbeam) 로 `try_send`.
//!
//! # Phase 1 범위
//! - 실제 알고리즘은 Phase 3+. 여기서는 `NoopStrategy` 만 제공.
//! - 공개 `Strategy` trait 를 통해 다중 플러그인 지원.
//! - consume loop 는 blocking `crossbeam_channel::Receiver::recv_timeout` 을 스레드
//!   가 아닌 tokio task 에서 돌린다 (SubTask 와 동일 패턴). 10ms timeout 으로 shutdown
//!   polling.
//!
//! # Hot path 원칙
//! - eval alloc 0, state 접근은 `&mut self` (lock 없음).
//! - order_tx try_send — full 시 drop + counter + warn. 실 주문 경로지만 Phase 1 은
//!   Noop 이라 상관 없음; Phase 3+ 에서 capacity 를 넉넉히 잡거나 async channel
//!   로 교체.
//! - telemetry: `record_stage_nanos(Stage::Consumed, internal_ns)` 만 hot path 에서.

#![deny(rust_2018_idioms)]

/// V6 전략 스캐폴드 (Phase 2 A 트랙 #3).
pub mod v6;
/// V7 전략 스캐폴드 (Phase 2 A 트랙 #3).
pub mod v7;
/// V8 전략 스캐폴드 (Phase 2 A 트랙).
pub mod v8;
mod egress_seed;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use hft_exchange_api::{CancellationToken, OrderRequest};
use hft_protocol::WireLevel;
use hft_strategy_core::decision::OrderLevel;
use hft_telemetry::{counter_inc, record_stage_nanos, CounterKey};
use hft_time::{Clock, LatencyStamps, Stage, SystemClock};
use hft_types::MarketEvent;
use tokio::task::JoinHandle;
use tracing::{info, warn};

pub use egress_seed::OrderEgressMetaSeed;

/// 전략이 drain 쪽으로 넘기는 주문 payload.
pub type OrderEnvelope = (OrderRequest, OrderEgressMetaSeed);

/// `Strategy::eval` 반환값의 별칭.
pub type Orders = Vec<OrderEnvelope>;

/// decision layer 의 `OrderLevel` 을 drain seed 로 정규화한다.
pub fn make_order_seed(
    client_seq: u64,
    level: OrderLevel,
    strategy_tag: &'static str,
) -> OrderEgressMetaSeed {
    OrderEgressMetaSeed {
        client_seq,
        level: match level {
            OrderLevel::LimitOpen => WireLevel::Open,
            OrderLevel::LimitClose | OrderLevel::MarketClose => WireLevel::Close,
        },
        reduce_only: level.is_close(),
        strategy_tag,
    }
}

/// 한 이벤트에 대응하는 strategy 로직.
///
/// # 계약
/// - `update(&mut self, ev)`: state 를 **먼저** 갱신. eval 은 이 결과를 읽을 수 있다.
/// - `eval(&mut self, ev) -> Orders`: 이 이벤트에 기반한 주문 벡터. 빈 벡터면 주문 없음.
///
/// 구현체는 **alloc 최소화**가 원칙. 필요하면 `Orders` 재사용을 위해 `eval_into` 스타일
/// 을 추후 추가할 수 있다. Phase 1 NoopStrategy 는 항상 빈 Vec 을 반환 (alloc 없음: `Vec::new()` 는 capacity 0 시 알로케이트 안 함).
pub trait Strategy: Send + 'static {
    /// state 갱신.
    fn update(&mut self, ev: &MarketEvent);

    /// 주문 평가. 빈 Vec 이면 주문 없음.
    fn eval(&mut self, ev: &MarketEvent) -> Orders;

    /// 라벨 (로그/metric 에 사용).
    fn label(&self) -> &str {
        "anonymous"
    }

    /// wire/text tag 용 정규화된 strategy 식별자.
    fn tag(&self) -> &'static str {
        "anonymous"
    }

    /// hot-path 외 제어 메시지. 기본 no-op — 계정 잔고 / net-position 업데이트 등
    /// 비동기 주기 이벤트를 hot path (`update`/`eval`) 와 **분리** 하기 위한 훅.
    ///
    /// 전달 경로: main 이 `StrategyHandle::control_tx` 로 메시지를 보내면, 러너 루프가
    /// 매 iteration 시작부에서 `try_recv` 로 drain 해 `on_control` 에 넘긴다. hot path
    /// (주문 eval) 에서 `&mut self` 잠금 충돌을 피하기 위한 설계.
    #[inline]
    fn on_control(&mut self, _ctrl: &StrategyControl) {}
}

/// 러너로 주입되는 제어 메시지. 전략 외부 (REST account poller 등) 에서
/// strategy 내부 상태의 일부를 비동기 주기 업데이트 시 사용.
///
/// 전용 enum 으로 타입-안전하게 확장 가능. Phase 2 는 계정 잔고/net-position/
/// rate-tracker decay 신호가 주 고객.
#[derive(Debug, Clone)]
pub enum StrategyControl {
    /// 계정 전체 USDT 잔고 및 미실현 PnL 업데이트.
    SetAccountBalance {
        /// total margin balance (USDT).
        total_usdt: f64,
        /// unrealized PnL (USDT).
        unrealized_pnl_usdt: f64,
    },
    /// (V6/V7 전용) 계정 net position USDT — positions 합산치.
    SetAccountNetPosition {
        /// 전 심볼 notional 합계 (long +, short -).
        net_usdt: f64,
    },
}

/// Phase 1 의 기본 strategy — 주문 0건.
///
/// integration 테스트에서 subscribe → strategy → (주문 0) 경로를 채우는 용도.
#[derive(Default)]
pub struct NoopStrategy {
    /// 누적 이벤트 수 (observability).
    pub seen: u64,
}

impl Strategy for NoopStrategy {
    #[inline]
    fn update(&mut self, _ev: &MarketEvent) {
        self.seen = self.seen.saturating_add(1);
    }

    #[inline]
    fn eval(&mut self, _ev: &MarketEvent) -> Orders {
        Orders::new()
    }

    fn label(&self) -> &str {
        "noop"
    }

    fn tag(&self) -> &'static str {
        "noop"
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Orders channel helpers
// ─────────────────────────────────────────────────────────────────────────────

/// order-gateway 로의 채널 wrapper.
///
/// hot path 에서 `try_send` 만 호출 — full 이면 drop + counter.
pub struct OrderSender {
    tx: Sender<OrderEnvelope>,
}

impl OrderSender {
    /// bounded(N) 로 생성. 반환한 (sender, receiver) 중 receiver 는 order-gateway 가 보유.
    pub fn bounded(cap: usize) -> (Self, Receiver<OrderEnvelope>) {
        let (tx, rx) = crossbeam_channel::bounded(cap);
        (Self { tx }, rx)
    }

    /// try_send. full 이면 Err, disconnected 면 Err (로그는 caller 책임).
    #[inline]
    pub fn try_send(&self, order: OrderEnvelope) -> Result<(), OrderSendError> {
        match self.tx.try_send(order) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(OrderSendError::Full),
            Err(TrySendError::Disconnected(_)) => Err(OrderSendError::Disconnected),
        }
    }
}

/// `OrderSender::try_send` 의 에러.
#[derive(Debug, Clone, Copy)]
pub enum OrderSendError {
    /// 버퍼가 꽉 참.
    Full,
    /// receiver 가 drop 됨.
    Disconnected,
}

// ─────────────────────────────────────────────────────────────────────────────
// Runner — 한 tokio task 로 strategy 를 구동
// ─────────────────────────────────────────────────────────────────────────────

/// strategy consume loop 의 runtime 묶음.
///
/// `rx` 는 subscriber 의 InprocQueue 수신단, `orders` 는 order-gateway 로의 발신단.
/// 둘 다 crossbeam bounded channel.
pub struct StrategyRunner<S: Strategy> {
    strategy: S,
    rx: Receiver<(MarketEvent, LatencyStamps)>,
    orders: OrderSender,
    clock: Arc<dyn Clock>,
    /// `StrategyControl` 수신 — drain 주기는 매 loop iter.
    control_rx: Option<Receiver<StrategyControl>>,
}

impl<S: Strategy> StrategyRunner<S> {
    /// 새 runner. control channel 없이 동작 (기본).
    pub fn new(
        strategy: S,
        rx: Receiver<(MarketEvent, LatencyStamps)>,
        orders: OrderSender,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            strategy,
            rx,
            orders,
            clock,
            control_rx: None,
        }
    }

    /// control channel 주입 버전. main 에서 account poller 와 wire 시 사용.
    pub fn with_control(mut self, control_rx: Receiver<StrategyControl>) -> Self {
        self.control_rx = Some(control_rx);
        self
    }

    /// 메인 루프. `cancel` 이 trip 되면 즉시 탈출 (채널은 drop 될 뿐 추가 drain 없음 —
    /// 실주문은 Phase 3+ 에서 graceful drain 필요).
    pub async fn run(mut self, cancel: CancellationToken) {
        info!(
            target: "strategy::runner",
            strategy = %self.strategy.label(),
            "strategy runner starting"
        );

        loop {
            if cancel.is_cancelled() {
                break;
            }

            // control drain (hot path 비경유). try_recv 는 crossbeam 빈 채널에서 ~3ns.
            // 버스트 방지를 위해 한 iter 당 최대 16 건.
            if let Some(crx) = self.control_rx.as_ref() {
                for _ in 0..16 {
                    match crx.try_recv() {
                        Ok(ctrl) => self.strategy.on_control(&ctrl),
                        Err(crossbeam_channel::TryRecvError::Empty) => break,
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            // control sender 가 drop 됐다고 러너를 죽이지는 않는다.
                            self.control_rx = None;
                            break;
                        }
                    }
                }
            }

            // 10ms timeout — shutdown polling 과 latency 사이 타협.
            // 이벤트가 꾸준히 들어오면 timeout 까지 기다리지 않고 바로 반환.
            match self.rx.recv_timeout(Duration::from_millis(10)) {
                Ok((ev, stamps)) => {
                    self.handle_one(ev, stamps);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // 평시 정상. tokio 스케줄러에게 기회 양보.
                    tokio::task::yield_now().await;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    warn!(
                        target: "strategy::runner",
                        "input channel disconnected — exiting"
                    );
                    break;
                }
            }
        }

        info!(target: "strategy::runner", "strategy runner stopped");
    }

    /// hot path 핸들러 — mark(consumed), HDR record, strategy.update + eval, try_send.
    #[inline]
    fn handle_one(&mut self, ev: MarketEvent, mut stamps: LatencyStamps) {
        stamps.mark(Stage::Consumed, &*self.clock);
        counter_inc(CounterKey::PipelineEvent);

        // stage 7 latency (WsReceived → Consumed, 내부 monotonic).
        if let Some(ns) = stamps.internal_ns() {
            record_stage_nanos(Stage::Consumed, ns);
        }

        self.strategy.update(&ev);
        let orders = self.strategy.eval(&ev);
        if orders.is_empty() {
            return;
        }

        for order in orders {
            match self.orders.try_send(order) {
                Ok(()) => {}
                Err(OrderSendError::Full) => {
                    // TODO(4c-followup): rename to StrategyOrderChannelFull; see CounterKey vocabulary cleanup.
                    counter_inc(CounterKey::ZmqDropped);
                    warn!(
                        target: "strategy::runner",
                        "order channel full — dropping order"
                    );
                }
                Err(OrderSendError::Disconnected) => {
                    warn!(
                        target: "strategy::runner",
                        "order channel disconnected — order dropped"
                    );
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Handle + start()
// ─────────────────────────────────────────────────────────────────────────────

/// strategy 가 소유하는 tokio task + cancel token + control sender.
pub struct StrategyHandle {
    /// runner task.
    pub task: JoinHandle<()>,
    /// 외부 shutdown.
    pub cancel: CancellationToken,
    /// 제어 메시지 발신 — `StrategyControl::SetAccountBalance` 등. clone 가능.
    /// 기본 start 는 `Some` 이며, 수신측이 drop 되면 send 는 `Err` 를 돌려준다.
    pub control_tx: Sender<StrategyControl>,
}

impl StrategyHandle {
    /// 즉시 종료 신호.
    pub fn shutdown(&self) {
        if !self.cancel.is_cancelled() {
            self.cancel.cancel();
        }
    }

    /// task join.
    pub async fn join(self) {
        if let Err(e) = self.task.await {
            warn!(target: "strategy::handle", error = ?e, "strategy task join error");
        }
    }

    /// 제어 메시지 발신 편의. channel full 이면 drop + warn 처리 (뷰어는 Relaxed).
    pub fn push_control(&self, ctrl: StrategyControl) {
        if let Err(e) = self.control_tx.try_send(ctrl) {
            warn!(target: "strategy::handle", error = ?e, "control channel send failed");
        }
    }
}

/// 기본 start — Clock 은 `SystemClock` 을 내부 생성.
///
/// 테스트에서는 `start_with_clock` 을 쓰면 MockClock 을 주입 가능.
pub fn start<S: Strategy>(
    strategy: S,
    rx: Receiver<(MarketEvent, LatencyStamps)>,
    orders: OrderSender,
) -> Result<StrategyHandle> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    start_with_clock(strategy, rx, orders, clock)
}

/// Clock 주입 버전. integration test 에서 MockClock 로 deterministic latency 측정.
pub fn start_with_clock<S: Strategy>(
    strategy: S,
    rx: Receiver<(MarketEvent, LatencyStamps)>,
    orders: OrderSender,
    clock: Arc<dyn Clock>,
) -> Result<StrategyHandle> {
    let cancel = CancellationToken::new();
    let task_cancel = cancel.child_token();
    // control channel — unbounded-free bounded(256); account poller 주기는 초 단위라 충분.
    let (control_tx, control_rx) = crossbeam_channel::bounded(256);
    let runner = StrategyRunner::new(strategy, rx, orders, clock).with_control(control_rx);
    let task = tokio::spawn(async move { runner.run(task_cancel).await });
    Ok(StrategyHandle {
        task,
        cancel,
        control_tx,
    })
}

/// 외부 바이너리 entry 에서 쓰는 래퍼 — `subscriber::InprocQueue` 의 receiver 와
/// `OrderSender` 를 외부에서 이미 만들어 넘겨준다고 가정.
pub async fn wire_and_run<S: Strategy>(
    strategy: S,
    rx: Receiver<(MarketEvent, LatencyStamps)>,
    orders: OrderSender,
) -> Result<StrategyHandle> {
    start(strategy, rx, orders).context("strategy start failed")
}

// ─────────────────────────────────────────────────────────────────────────────
// 테스트
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_exchange_api::{OrderRequest, OrderSide, OrderType, TimeInForce};
    use hft_time::MockClock;
    use hft_types::{BookTicker, ExchangeId, Price, Size, Symbol};

    fn bt_event() -> (MarketEvent, LatencyStamps) {
        let bt = BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(100.0),
            ask_price: Price(101.0),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: 0,
            server_time_ms: 1_700_000_000_000,
        };
        let mut s = LatencyStamps::new();
        s.set_exchange_server_ms(bt.server_time_ms);
        // ws_received 를 1ms 뒤로 찍어둔다 (stage 7 internal_ns 의 anchor).
        s.ws_received = hft_time::Stamp {
            wall_ms: bt.server_time_ms + 1,
            mono_ns: 1_000_000,
        };
        (MarketEvent::BookTicker(bt), s)
    }

    #[test]
    fn noop_strategy_returns_empty_orders() {
        let mut s = NoopStrategy::default();
        let (ev, _) = bt_event();
        s.update(&ev);
        let orders = s.eval(&ev);
        assert!(orders.is_empty());
        assert_eq!(s.seen, 1);
    }

    #[test]
    fn order_sender_try_send_full_and_disconnected() {
        let (tx, rx) = OrderSender::bounded(1);
        let req = OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            qty: 0.1,
            price: Some(100.0),
            tif: TimeInForce::Gtc,
            client_id: Arc::from("cid-1"),
        };
        let seed = OrderEgressMetaSeed {
            client_seq: 1,
            level: WireLevel::Open,
            reduce_only: false,
            strategy_tag: "test",
        };

        tx.try_send((req.clone(), seed)).unwrap();
        assert!(matches!(tx.try_send((req.clone(), seed)), Err(OrderSendError::Full)));

        drop(rx);
        assert!(matches!(
            tx.try_send((req, seed)),
            Err(OrderSendError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn runner_consumes_events_and_drains() {
        let (ev_tx, ev_rx) = crossbeam_channel::bounded(64);
        let (orders_tx, _orders_rx) = OrderSender::bounded(64);
        let clock: Arc<dyn Clock> = MockClock::new(1_700_000_000_000, 0);

        let handle = start_with_clock(NoopStrategy::default(), ev_rx, orders_tx, clock).unwrap();

        // 3개 이벤트 push.
        for _ in 0..3 {
            let (e, s) = bt_event();
            ev_tx.send((e, s)).unwrap();
        }

        // 러너가 3건 처리할 시간 확보.
        tokio::time::sleep(Duration::from_millis(50)).await;

        handle.shutdown();
        handle.join().await;
    }

    #[tokio::test]
    async fn runner_shutdown_on_disconnected_channel() {
        let (ev_tx, ev_rx) = crossbeam_channel::bounded::<(MarketEvent, LatencyStamps)>(4);
        let (orders_tx, _orders_rx) = OrderSender::bounded(4);
        let handle = start(NoopStrategy::default(), ev_rx, orders_tx).unwrap();

        drop(ev_tx); // sender 를 닫으면 runner 는 Disconnected 로 loop 탈출.
        // shutdown 토큰을 cancel 하지 않아도 러너가 빠져나옴.
        handle.join().await;
    }

    #[tokio::test]
    async fn runner_records_consumed_stage_on_event() {
        let (ev_tx, ev_rx) = crossbeam_channel::bounded(4);
        let (orders_tx, _orders_rx) = OrderSender::bounded(4);
        let mock: Arc<MockClock> = MockClock::new(1_700_000_000_010, 10_000_000);
        let clock: Arc<dyn Clock> = mock.clone();

        let handle =
            start_with_clock(NoopStrategy::default(), ev_rx, orders_tx, clock).unwrap();

        let (e, s) = bt_event();
        ev_tx.send((e, s)).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        handle.shutdown();
        handle.join().await;
        // HDR 내용은 전역이라 직접 검사 어려움 — panic 없이 끝나면 OK.
    }

    /// 커스텀 strategy: 입력이 bookticker 이면 매 이벤트마다 주문 1개 생성.
    struct EchoStrategy {
        client_seq: u64,
    }

    impl Strategy for EchoStrategy {
        fn update(&mut self, _ev: &MarketEvent) {}
        fn eval(&mut self, ev: &MarketEvent) -> Orders {
            self.client_seq += 1;
            let id: Arc<str> = Arc::from(format!("echo-{}", self.client_seq));
            let sym = ev.symbol();
            let exch = ev.exchange();
            vec![(
                OrderRequest {
                    exchange: exch,
                    symbol: sym,
                    side: OrderSide::Buy,
                    order_type: OrderType::Limit,
                    qty: 1.0,
                    price: Some(1.0),
                    tif: TimeInForce::Gtc,
                    client_id: id,
                },
                OrderEgressMetaSeed {
                    client_seq: self.client_seq,
                    level: WireLevel::Open,
                    reduce_only: false,
                    strategy_tag: "echo",
                },
            )]
        }

        fn tag(&self) -> &'static str {
            "echo"
        }
    }

    #[tokio::test]
    async fn runner_forwards_orders_from_eval() {
        let (ev_tx, ev_rx) = crossbeam_channel::bounded(16);
        let (orders_tx, orders_rx) = OrderSender::bounded(16);
        let handle = start(EchoStrategy { client_seq: 0 }, ev_rx, orders_tx).unwrap();

        for _ in 0..5 {
            let (e, s) = bt_event();
            ev_tx.send((e, s)).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(80)).await;

        let mut got = 0usize;
        let mut seqs = Vec::new();
        while let Ok((_, seed)) = orders_rx.try_recv() {
            got += 1;
            assert_eq!(seed.strategy_tag, "echo");
            seqs.push(seed.client_seq);
        }
        assert_eq!(got, 5, "expected 5 echo orders, got {got}");
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);

        handle.shutdown();
        handle.join().await;
    }
}

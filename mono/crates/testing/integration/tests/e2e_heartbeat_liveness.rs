//! e2e_heartbeat_liveness — heartbeat liveness guard 와 reject 결과 경로를
//! in-process 통합 형태로 고정한다.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use crossbeam_channel::bounded;
use hft_exchange_api::{
    ApiError, ExchangeExecutor, OrderAck, OrderRequest, OrderSide, OrderType, TimeInForce,
};
use hft_protocol::order_wire::{OrderResultWire, STATUS_ACCEPTED, STATUS_REJECTED};
use hft_testkit::fixtures;
use hft_time::{Clock, LatencyStamps, MockClock, Stage, SystemClock};
use hft_types::{ExchangeId, MarketEvent};
use order_gateway::{
    start_with_arc, IngressEnvelope, IngressMeta, RetryPolicy, Route, RoutingTable,
    DEDUP_CACHE_CAP_DEFAULT,
};
use parking_lot::Mutex;
use strategy::{
    spawn_order_drain_loop_with_now, start, GatewayLiveness, OrderEgressMetaSeed, OrderEnvelope,
    OrderResultInfo, OrderSender, ResultStatus, Strategy, StrategyControl,
};

#[derive(Clone)]
struct SingleShotStrategy {
    sent: Arc<AtomicBool>,
    results: Arc<Mutex<Vec<OrderResultInfo>>>,
}

impl SingleShotStrategy {
    fn new(results: Arc<Mutex<Vec<OrderResultInfo>>>) -> Self {
        Self {
            sent: Arc::new(AtomicBool::new(false)),
            results,
        }
    }
}

impl Strategy for SingleShotStrategy {
    fn update(&mut self, _ev: &MarketEvent) {}

    fn eval(&mut self, _ev: &MarketEvent) -> strategy::Orders {
        if self.sent.swap(true, Ordering::SeqCst) {
            return Vec::new();
        }

        vec![(
            OrderRequest {
                exchange: ExchangeId::Gate,
                symbol: "BTC_USDT".into(),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                qty: 1.0,
                price: Some(100.0),
                reduce_only: false,
                tif: TimeInForce::Gtc,
                client_seq: 1,
                origin_ts_ns: 0,
                client_id: Arc::from("itest-heartbeat-1"),
            },
            OrderEgressMetaSeed {
                client_seq: 1,
                level: hft_protocol::WireLevel::Open,
                reduce_only: false,
                strategy_tag: "itest",
            },
        )]
    }

    fn label(&self) -> &str {
        "single-shot-heartbeat"
    }

    fn tag(&self) -> &'static str {
        "itest"
    }

    fn on_order_result(&mut self, info: &OrderResultInfo) {
        self.results.lock().push(info.clone());
    }
}

struct RejectingExecutor {
    id: ExchangeId,
}

#[async_trait]
impl ExchangeExecutor for RejectingExecutor {
    fn id(&self) -> ExchangeId {
        self.id
    }

    async fn place_order(&self, _req: OrderRequest) -> anyhow::Result<OrderAck> {
        Err(ApiError::Rejected("integration reject".into()).into())
    }

    async fn cancel(&self, _exchange_order_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn label(&self) -> String {
        format!("reject:{}", self.id.as_str())
    }
}

fn ingress_from_order(order: OrderEnvelope) -> IngressEnvelope {
    let (req, seed) = order;
    let mut text_tag = [0u8; 32];
    let bytes = seed.strategy_tag.as_bytes();
    let len = bytes.len().min(text_tag.len());
    text_tag[..len].copy_from_slice(&bytes[..len]);
    (
        req,
        IngressMeta {
            origin_vm_id: None,
            text_tag,
        },
    )
}

fn decode_zero_padded_string(bytes: &[u8]) -> Result<String> {
    let len = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let text = std::str::from_utf8(&bytes[..len]).context("utf8")?;
    Ok(text.to_string())
}

fn result_info_from_wire(wire: &OrderResultWire) -> Result<OrderResultInfo> {
    Ok(OrderResultInfo {
        client_seq: wire.client_seq,
        status: match wire.status {
            STATUS_ACCEPTED => ResultStatus::Accepted,
            STATUS_REJECTED => ResultStatus::Rejected,
            other => anyhow::bail!("unexpected result status={other}"),
        },
        exchange_order_id: decode_zero_padded_string(&wire.exchange_order_id)?,
        gateway_ts_ns: wire.gateway_ts_ns,
        text_tag: decode_zero_padded_string(&wire.text_tag)?,
    })
}

fn sample_event() -> (MarketEvent, LatencyStamps) {
    let mut stamps = LatencyStamps::new();
    let clock = SystemClock::new();
    stamps.mark(Stage::WsReceived, &clock);
    (
        MarketEvent::BookTicker(fixtures::bookticker(1_700_000_000_000)),
        stamps,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_keeps_drain_alive() {
    let results = Arc::new(Mutex::new(Vec::<OrderResultInfo>::new()));
    let strategy = SingleShotStrategy::new(results);
    let (event_tx, event_rx) = bounded::<(MarketEvent, LatencyStamps)>(8);
    let (orders_tx, orders_rx) = OrderSender::bounded(8);
    let strategy_handle = start(strategy, event_rx, orders_tx).unwrap();

    let (submitted_tx, submitted_rx) = bounded::<OrderRequest>(8);
    let cancel = hft_exchange_api::CancellationToken::new();
    let clock = MockClock::new(1_700_000_000_000, 0);
    let now_ns = {
        let clock = clock.clone();
        move || clock.epoch_ns()
    };
    let liveness = GatewayLiveness::new(5_000);
    liveness.touch(clock.epoch_ns());

    let drain = spawn_order_drain_loop_with_now(
        orders_rx,
        cancel.clone(),
        now_ns,
        Some(liveness),
        move |req, _seed, _origin_ts_ns| submitted_tx.send(req).map_err(|e| e.to_string()),
    );

    event_tx.send(sample_event()).unwrap();
    let req = tokio::task::spawn_blocking(move || submitted_rx.recv_timeout(Duration::from_millis(500)))
        .await
        .unwrap()
        .expect("drain should forward live order");

    assert_eq!(req.client_seq, 1);
    assert_eq!(req.origin_ts_ns, clock.epoch_ns());

    drop(event_tx);
    strategy_handle.shutdown();
    cancel.cancel();
    let _ = drain.await;
    strategy_handle.join().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_gateway_drops_orders() {
    let results = Arc::new(Mutex::new(Vec::<OrderResultInfo>::new()));
    let strategy = SingleShotStrategy::new(results);
    let (event_tx, event_rx) = bounded::<(MarketEvent, LatencyStamps)>(8);
    let (orders_tx, orders_rx) = OrderSender::bounded(8);
    let strategy_handle = start(strategy, event_rx, orders_tx).unwrap();

    let (submitted_tx, submitted_rx) = bounded::<OrderRequest>(8);
    let cancel = hft_exchange_api::CancellationToken::new();
    let clock = MockClock::new(200, 0);
    let now_ns = {
        let clock = clock.clone();
        move || clock.epoch_ns()
    };
    let liveness = GatewayLiveness::new(100);
    liveness.touch(50_000_000);

    let drain = spawn_order_drain_loop_with_now(
        orders_rx,
        cancel.clone(),
        now_ns,
        Some(liveness),
        move |req, _seed, _origin_ts_ns| submitted_tx.send(req).map_err(|e| e.to_string()),
    );

    event_tx.send(sample_event()).unwrap();
    let recv = tokio::task::spawn_blocking(move || submitted_rx.recv_timeout(Duration::from_millis(500)))
        .await
        .unwrap();
    assert!(recv.is_err(), "stale gateway should drop order before submit");

    drop(event_tx);
    strategy_handle.shutdown();
    cancel.cancel();
    let _ = drain.await;
    strategy_handle.join().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_reject_reaches_strategy() {
    let results = Arc::new(Mutex::new(Vec::<OrderResultInfo>::new()));
    let strategy = SingleShotStrategy::new(results.clone());
    let (event_tx, event_rx) = bounded::<(MarketEvent, LatencyStamps)>(8);
    let (orders_tx, orders_rx) = OrderSender::bounded(8);
    let strategy_handle = start(strategy, event_rx, orders_tx).unwrap();

    let (req_tx, req_rx) = bounded::<IngressEnvelope>(8);
    let (result_tx, result_rx) = bounded::<OrderResultWire>(8);
    let mut routing = RoutingTable::new();
    routing.insert(
        ExchangeId::Gate,
        Route::Rust(Arc::new(RejectingExecutor { id: ExchangeId::Gate })),
    );
    let gateway = start_with_arc(
        Arc::new(routing),
        req_rx,
        None,
        Some(result_tx),
        DEDUP_CACHE_CAP_DEFAULT,
        RetryPolicy::default(),
    )
    .unwrap();

    event_tx.send(sample_event()).unwrap();
    let order = tokio::task::spawn_blocking(move || orders_rx.recv_timeout(Duration::from_millis(500)))
        .await
        .unwrap()
        .expect("strategy emitted order");
    req_tx.send(ingress_from_order(order)).unwrap();

    let wire = tokio::task::spawn_blocking(move || result_rx.recv_timeout(Duration::from_millis(1000)))
        .await
        .unwrap()
        .expect("gateway emitted rejected result wire");

    assert_eq!(wire.client_seq, 1);
    assert_eq!(wire.status, STATUS_REJECTED);

    strategy_handle.push_control(StrategyControl::OrderResult(
        result_info_from_wire(&wire).unwrap(),
    ));

    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        {
            let guard = results.lock();
            if let Some(info) = guard.first() {
                assert_eq!(info.client_seq, 1);
                assert_eq!(info.status, ResultStatus::Rejected);
                assert_eq!(info.text_tag, "itest");
                assert!(info.exchange_order_id.is_empty());
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for rejected order result"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    drop(event_tx);
    gateway.shutdown();
    strategy_handle.shutdown();
    gateway.join().await;
    strategy_handle.join().await;
}

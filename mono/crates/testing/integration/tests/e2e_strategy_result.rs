//! e2e_strategy_result — strategy 주문이 gateway router 를 거쳐 result wire 를 만들고,
//! 다시 strategy control plane 으로 되돌아가는 경로를 공용 API 로 검증한다.
//!
//! ZMQ ingress/result reverse path 자체는 Step 8 에서 service crate 테스트로 이미 검증됐다.
//! 여기서는 cleanup batch 용으로 더 작은 blast radius 의 통합 경로를 고정한다.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::bounded;
use hft_exchange_api::{OrderRequest, OrderSide, OrderType, TimeInForce};
use hft_protocol::order_wire::{OrderResultWire, STATUS_ACCEPTED};
use hft_testkit::fixtures;
use hft_time::{LatencyStamps, Stage, SystemClock};
use hft_types::{ExchangeId, MarketEvent};
use order_gateway::{
    start_with_arc, IngressEnvelope, IngressMeta, NoopExecutor, Route, RoutingTable,
    DEDUP_CACHE_CAP_DEFAULT, RetryPolicy,
};
use parking_lot::Mutex;
use strategy::{
    start, OrderEgressMetaSeed, OrderEnvelope, OrderResultInfo, OrderSender, ResultStatus,
    Strategy, StrategyControl,
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
                origin_ts_ns: 1_700_000_000_000_000_123,
                client_id: Arc::from("itest-1"),
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
        "single-shot"
    }

    fn tag(&self) -> &'static str {
        "itest"
    }

    fn on_order_result(&mut self, info: &OrderResultInfo) {
        self.results.lock().push(info.clone());
    }
}

fn encode_zero_padded<const N: usize>(text: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let bytes = text.as_bytes();
    let len = bytes.len().min(N);
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

fn decode_zero_padded_string(bytes: &[u8]) -> Result<String> {
    let len = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let text = std::str::from_utf8(&bytes[..len]).context("utf8")?;
    Ok(text.to_string())
}

fn ingress_from_order(order: OrderEnvelope) -> IngressEnvelope {
    let (req, seed) = order;
    (
        req,
        IngressMeta {
            origin_vm_id: None,
            text_tag: encode_zero_padded(seed.strategy_tag),
        },
    )
}

fn result_info_from_wire(wire: &OrderResultWire) -> Result<OrderResultInfo> {
    Ok(OrderResultInfo {
        client_seq: wire.client_seq,
        status: match wire.status {
            STATUS_ACCEPTED => ResultStatus::Accepted,
            other => anyhow::bail!("unexpected result status={other}"),
        },
        exchange_order_id: decode_zero_padded_string(&wire.exchange_order_id)?,
        gateway_ts_ns: wire.gateway_ts_ns,
        text_tag: decode_zero_padded_string(&wire.text_tag)?,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strategy_gateway_result_roundtrip_accepted() {
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
        Route::Rust(Arc::new(NoopExecutor::new(ExchangeId::Gate))),
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

    let mut stamps = LatencyStamps::new();
    let clock = SystemClock::new();
    stamps.mark(Stage::WsReceived, &clock);
    event_tx
        .send((MarketEvent::BookTicker(fixtures::bookticker(1_700_000_000_000)), stamps))
        .unwrap();

    let order = tokio::task::spawn_blocking(move || {
        orders_rx.recv_timeout(Duration::from_millis(500))
    })
    .await
    .unwrap()
    .expect("strategy emitted order");
    req_tx.send(ingress_from_order(order)).unwrap();

    let wire = tokio::task::spawn_blocking(move || {
        result_rx.recv_timeout(Duration::from_millis(1000))
    })
    .await
    .unwrap()
    .expect("gateway emitted result wire");

    assert_eq!(wire.client_seq, 1);
    assert_eq!(wire.status, STATUS_ACCEPTED);
    assert_eq!(decode_zero_padded_string(&wire.text_tag).unwrap(), "itest");
    assert!(decode_zero_padded_string(&wire.exchange_order_id)
        .unwrap()
        .starts_with("noop-"));

    strategy_handle.push_control(StrategyControl::OrderResult(
        result_info_from_wire(&wire).unwrap(),
    ));

    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        {
            let guard = results.lock();
            if let Some(info) = guard.first() {
                assert_eq!(info.client_seq, 1);
                assert_eq!(info.status, ResultStatus::Accepted);
                assert_eq!(info.text_tag, "itest");
                assert!(info.exchange_order_id.starts_with("noop-"));
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for strategy order result"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    drop(event_tx);
    gateway.shutdown();
    strategy_handle.shutdown();
    gateway.join().await;
    strategy_handle.join().await;
}

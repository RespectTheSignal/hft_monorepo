use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hft_config::order_egress::{BackpressurePolicy, ZmqOrderEgressConfig};
use hft_exchange_api::{OrderRequest, OrderSide, OrderType, TimeInForce};
use hft_protocol::{
    order_request_to_order_request_wire, order_wire::ORDER_REQUEST_WIRE_SIZE, OrderEgressMeta,
    WireLevel,
};
use hft_shm::{
    Backing, LayoutSpec, OrderRingReader, OrderRingWriter, PlaceAuxMeta, QuoteSlotWriter, Role,
    SharedRegion, SubKind, SymbolTable, TradeRingWriter, PLACE_LEVEL_CLOSE, PLACE_LEVEL_OPEN,
};
use hft_strategy_shm::StrategyShmClient;
use hft_telemetry::counters_snapshot;
use hft_types::{ExchangeId, Symbol};
use tempfile::tempdir;

use crate::{
    BlockingOrderEgress, FlakyOrderEgress, NoopOrderEgress, OrderEgress, PolicyOrderEgress,
    ShmOrderEgress, SubmitError, SubmitOutcome, ZmqOrderEgress,
};

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock_test() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn sample_req(exchange: ExchangeId) -> OrderRequest {
    OrderRequest {
        exchange,
        symbol: Symbol::new("BTC_USDT"),
        side: OrderSide::Buy,
        order_type: OrderType::Limit,
        qty: 1.0,
        price: Some(100.0),
        reduce_only: false,
        tif: TimeInForce::Gtc,
        client_seq: 42,
        origin_ts_ns: 1_765_432_100_000_000_000,
        client_id: Arc::from("v8-42"),
    }
}

fn sample_meta() -> OrderEgressMeta<'static> {
    OrderEgressMeta {
        strategy_tag: "v8",
        level: WireLevel::Open,
        symbol_id: Some(77),
        symbol_idx: Some(88),
        quantize: None,
    }
}

fn snapshot_map() -> HashMap<String, u64> {
    counters_snapshot().into_iter().collect()
}

fn counter_delta(before: &HashMap<String, u64>, after: &HashMap<String, u64>, key: &str) -> u64 {
    after.get(key).copied().unwrap_or(0) - before.get(key).copied().unwrap_or(0)
}

fn unique_inproc() -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let id = N.fetch_add(1, Ordering::SeqCst);
    format!("inproc://hft-order-egress-test-{id}")
}

fn spec(order_ring_capacity: u64) -> LayoutSpec {
    LayoutSpec {
        quote_slot_count: 16,
        trade_ring_capacity: 32,
        symtab_capacity: 16,
        order_ring_capacity,
        n_max: 1,
    }
}

fn boot_publisher(path: &std::path::Path, spec: LayoutSpec) -> SharedRegion {
    let sr = SharedRegion::create_or_attach(
        Backing::DevShm {
            path: path.to_path_buf(),
        },
        spec,
        Role::Publisher,
    )
    .expect("publisher shared region");
    let _ = QuoteSlotWriter::from_region(
        sr.sub_region(SubKind::Quote).unwrap(),
        spec.quote_slot_count,
    )
    .expect("quote writer");
    let _ = TradeRingWriter::from_region(
        sr.sub_region(SubKind::Trade).unwrap(),
        spec.trade_ring_capacity,
    )
    .expect("trade writer");
    let _ = SymbolTable::from_region(
        sr.sub_region(SubKind::Symtab).unwrap(),
        spec.symtab_capacity,
    )
    .expect("symtab");
    let sub = sr.sub_region(SubKind::OrderRing { vm_id: 0 }).unwrap();
    let _ = OrderRingWriter::from_region(sub, spec.order_ring_capacity).expect("order writer");
    sr
}

fn connect_strategy_client(path: &std::path::Path, spec: LayoutSpec) -> Arc<StrategyShmClient> {
    Arc::new(
        StrategyShmClient::attach(
            Backing::DevShm {
                path: path.to_path_buf(),
            },
            spec,
            0,
        )
        .expect("attach strategy"),
    )
}

fn zmq_cfg(endpoint: String, send_hwm: i32) -> ZmqOrderEgressConfig {
    ZmqOrderEgressConfig {
        endpoint,
        send_hwm,
        linger_ms: 0,
        reconnect_interval_ms: 10,
        reconnect_interval_max_ms: 10,
    }
}

#[test]
fn trait_object_safety_compile() {
    fn take_obj(_egress: Box<dyn OrderEgress>) {}
    take_obj(Box::new(NoopOrderEgress::default()));
}

#[test]
fn noop_always_sent() {
    let _g = lock_test();
    let req = sample_req(ExchangeId::Gate);
    let meta = sample_meta();
    let policies = [
        BackpressurePolicy::Drop,
        BackpressurePolicy::RetryWithTimeout {
            max_retries: 3,
            backoff_ns: 100,
            total_timeout_ns: 10_000,
        },
        BackpressurePolicy::BlockWithTimeout { timeout_ns: 10_000 },
    ];

    for policy in policies {
        let egress = PolicyOrderEgress::new(NoopOrderEgress::default(), policy);
        assert_eq!(egress.submit(&req, &meta).unwrap(), SubmitOutcome::Sent);
    }
}

#[test]
fn drop_policy_counts_backpressure() {
    let _g = lock_test();
    let req = sample_req(ExchangeId::Gate);
    let meta = sample_meta();
    let before = snapshot_map();
    let egress = PolicyOrderEgress::new(BlockingOrderEgress::default(), BackpressurePolicy::Drop);
    assert_eq!(
        egress.submit(&req, &meta).unwrap(),
        SubmitOutcome::WouldBlock
    );
    let after = snapshot_map();
    assert_eq!(
        counter_delta(&before, &after, "order_egress_backpressure_dropped"),
        1
    );
}

#[test]
fn retry_policy_succeeds_within_budget() {
    let _g = lock_test();
    let req = sample_req(ExchangeId::Gate);
    let meta = sample_meta();
    let before = snapshot_map();
    let egress = PolicyOrderEgress::new(
        FlakyOrderEgress::new(2),
        BackpressurePolicy::RetryWithTimeout {
            max_retries: 5,
            backoff_ns: 100,
            total_timeout_ns: 100_000,
        },
    );
    assert_eq!(egress.submit(&req, &meta).unwrap(), SubmitOutcome::Sent);
    let after = snapshot_map();
    assert_eq!(
        counter_delta(&before, &after, "order_egress_backpressure_retried"),
        2
    );
    assert_eq!(
        counter_delta(&before, &after, "order_egress_backpressure_retry_exhausted"),
        0
    );
}

#[test]
fn retry_policy_exhausts() {
    let _g = lock_test();
    let req = sample_req(ExchangeId::Gate);
    let meta = sample_meta();
    let before = snapshot_map();
    let egress = PolicyOrderEgress::new(
        BlockingOrderEgress::default(),
        BackpressurePolicy::RetryWithTimeout {
            max_retries: 3,
            backoff_ns: 100,
            total_timeout_ns: 100_000,
        },
    );
    assert_eq!(
        egress.submit(&req, &meta).unwrap(),
        SubmitOutcome::WouldBlock
    );
    let after = snapshot_map();
    assert_eq!(
        counter_delta(&before, &after, "order_egress_backpressure_retried"),
        3
    );
    assert_eq!(
        counter_delta(&before, &after, "order_egress_backpressure_retry_exhausted"),
        1
    );
}

#[test]
fn retry_policy_total_timeout_cuts_early() {
    let _g = lock_test();
    let req = sample_req(ExchangeId::Gate);
    let meta = sample_meta();
    let before = snapshot_map();
    let egress = PolicyOrderEgress::new(
        BlockingOrderEgress::default(),
        BackpressurePolicy::RetryWithTimeout {
            max_retries: 1000,
            backoff_ns: 500_000,
            total_timeout_ns: 1_000_000,
        },
    );
    assert_eq!(
        egress.submit(&req, &meta).unwrap(),
        SubmitOutcome::WouldBlock
    );
    let after = snapshot_map();
    let retried = counter_delta(&before, &after, "order_egress_backpressure_retried");
    assert!(
        retried < 1000,
        "timeout should cut retries early, got {retried}"
    );
    assert_eq!(
        counter_delta(&before, &after, "order_egress_backpressure_retry_exhausted"),
        1
    );
}

#[test]
fn block_policy_within_timeout_succeeds() {
    let _g = lock_test();
    let req = sample_req(ExchangeId::Gate);
    let meta = sample_meta();
    let before = snapshot_map();
    let egress = PolicyOrderEgress::new(
        FlakyOrderEgress::new(50),
        BackpressurePolicy::BlockWithTimeout {
            timeout_ns: 10_000_000,
        },
    );
    assert_eq!(egress.submit(&req, &meta).unwrap(), SubmitOutcome::Sent);
    let after = snapshot_map();
    assert_eq!(
        counter_delta(&before, &after, "order_egress_backpressure_blocked"),
        1
    );
}

#[test]
fn block_policy_timeout_exceeded() {
    let _g = lock_test();
    let req = sample_req(ExchangeId::Gate);
    let meta = sample_meta();
    let before = snapshot_map();
    let egress = PolicyOrderEgress::new(
        BlockingOrderEgress::default(),
        BackpressurePolicy::BlockWithTimeout {
            timeout_ns: 1_000_000,
        },
    );
    assert_eq!(
        egress.submit(&req, &meta).unwrap(),
        SubmitOutcome::WouldBlock
    );
    let after = snapshot_map();
    assert_eq!(
        counter_delta(&before, &after, "order_egress_backpressure_block_timeout"),
        1
    );
}

#[test]
fn adapt_error_propagates_without_transport_call() {
    let _g = lock_test();
    let mut req = sample_req(ExchangeId::Gate);
    req.qty = 1.5;
    let meta = sample_meta();
    let before = snapshot_map();
    let egress = ZmqOrderEgress::connect(&zmq_cfg("tcp://127.0.0.1:39071".into(), 8)).unwrap();

    assert!(matches!(
        egress.try_submit(&req, &meta),
        Err(SubmitError::Adapt(_))
    ));

    let after = snapshot_map();
    assert_eq!(
        counter_delta(&before, &after, "order_egress_serialize_error"),
        1
    );
    assert_eq!(counter_delta(&before, &after, "order_egress_zmq_ok"), 0);
}

#[test]
fn shm_egress_smoke() {
    let _g = lock_test();
    let dir = tempdir().unwrap();
    let path = dir.path().join("sr_smoke");
    let spec = spec(8);
    let sr_pub = boot_publisher(&path, spec);
    let client = connect_strategy_client(&path, spec);
    let egress = ShmOrderEgress::new(client);
    let req = sample_req(ExchangeId::Gate);
    let mut meta = sample_meta();
    meta.symbol_idx = None;
    let before = snapshot_map();

    assert_eq!(egress.try_submit(&req, &meta).unwrap(), SubmitOutcome::Sent);

    let sub = sr_pub.sub_region(SubKind::OrderRing { vm_id: 0 }).unwrap();
    let mut reader = OrderRingReader::from_region(sub).unwrap();
    let got = reader.try_consume().expect("one frame");
    let expected_idx = sr_pub
        .sub_region(SubKind::Symtab)
        .ok()
        .and_then(|sub| SymbolTable::open_from_region(sub).ok())
        .and_then(|symtab| symtab.lookup(ExchangeId::Gate, "BTC_USDT"))
        .expect("symbol idx");
    assert_eq!(got.exchange_id, hft_shm::exchange_to_u8(ExchangeId::Gate));
    assert_eq!(got.symbol_idx, expected_idx);
    assert_eq!(got.client_id, req.client_seq);
    assert_eq!(got.ts_ns, req.origin_ts_ns);
    assert_eq!(got.price, 10_000_000_000);
    assert_eq!(got.size, 1);
    let place_meta = PlaceAuxMeta::unpack(&got.aux);
    assert_eq!(place_meta.wire_level_code(), PLACE_LEVEL_OPEN);
    assert!(!place_meta.reduce_only());
    assert_eq!(place_meta.text_tag_str(), "v8");
    let after = snapshot_map();
    assert_eq!(counter_delta(&before, &after, "shm_order_published"), 1);
}

#[test]
fn shm_egress_preserves_close_reduce_only_meta() {
    let _g = lock_test();
    let dir = tempdir().unwrap();
    let path = dir.path().join("sr_reduce");
    let spec = spec(8);
    let sr_pub = boot_publisher(&path, spec);
    let client = connect_strategy_client(&path, spec);
    let egress = ShmOrderEgress::new(client);
    let mut req = sample_req(ExchangeId::Gate);
    req.reduce_only = true;
    let mut meta = sample_meta();
    meta.level = WireLevel::Close;
    meta.strategy_tag = "v7";
    meta.symbol_idx = None;

    assert_eq!(egress.try_submit(&req, &meta).unwrap(), SubmitOutcome::Sent);

    let sub = sr_pub.sub_region(SubKind::OrderRing { vm_id: 0 }).unwrap();
    let mut reader = OrderRingReader::from_region(sub).unwrap();
    let got = reader.try_consume().expect("one frame");
    let place_meta = PlaceAuxMeta::unpack(&got.aux);
    assert_eq!(place_meta.wire_level_code(), PLACE_LEVEL_CLOSE);
    assert!(place_meta.reduce_only());
    assert_eq!(place_meta.text_tag_str(), "v7");
}

#[test]
fn shm_egress_ring_full() {
    let _g = lock_test();
    let dir = tempdir().unwrap();
    let path = dir.path().join("sr_full");
    let spec = spec(4);
    let _sr_pub = boot_publisher(&path, spec);
    let client = connect_strategy_client(&path, spec);
    let egress = ShmOrderEgress::new(client);
    let req = sample_req(ExchangeId::Gate);
    let mut meta = sample_meta();
    meta.symbol_idx = None;
    let before = snapshot_map();

    for _ in 0..4 {
        assert_eq!(egress.try_submit(&req, &meta).unwrap(), SubmitOutcome::Sent);
    }
    assert_eq!(
        egress.try_submit(&req, &meta).unwrap(),
        SubmitOutcome::WouldBlock
    );

    let after = snapshot_map();
    assert_eq!(counter_delta(&before, &after, "shm_order_full_drop"), 1);
}

#[test]
fn zmq_egress_smoke() {
    let _g = lock_test();
    let ctx = hft_zmq::Context::new();
    let ep = unique_inproc();
    let pull = ctx.raw().socket(zmq::PULL).expect("pull socket");
    pull.bind(&ep).expect("bind pull");
    pull.set_rcvtimeo(500).expect("set timeout");
    let egress =
        ZmqOrderEgress::connect_with_context(ctx.clone(), &zmq_cfg(ep.clone(), 8)).unwrap();
    let req = sample_req(ExchangeId::Gate);
    let meta = sample_meta();
    let before = snapshot_map();

    assert_eq!(egress.try_submit(&req, &meta).unwrap(), SubmitOutcome::Sent);

    let got = pull.recv_bytes(0).expect("recv bytes");
    assert_eq!(got.len(), ORDER_REQUEST_WIRE_SIZE);
    let mut expected = [0u8; ORDER_REQUEST_WIRE_SIZE];
    order_request_to_order_request_wire(&req, &meta)
        .expect("wire")
        .encode(&mut expected);
    assert_eq!(got.as_slice(), expected.as_slice());
    let after = snapshot_map();
    assert_eq!(counter_delta(&before, &after, "order_egress_zmq_ok"), 1);
}

#[test]
fn zmq_egress_hwm_would_block() {
    let _g = lock_test();
    let ctx = hft_zmq::Context::new();
    let ep = unique_inproc();
    let pull = ctx.raw().socket(zmq::PULL).expect("pull socket");
    pull.set_rcvhwm(1).expect("set rcvhwm");
    pull.bind(&ep).expect("bind pull");
    std::thread::sleep(Duration::from_millis(5));
    let egress = ZmqOrderEgress::connect_with_context(ctx.clone(), &zmq_cfg(ep, 1)).unwrap();
    let req = sample_req(ExchangeId::Gate);
    let meta = sample_meta();
    let before = snapshot_map();

    assert_eq!(egress.try_submit(&req, &meta).unwrap(), SubmitOutcome::Sent);
    assert_eq!(egress.try_submit(&req, &meta).unwrap(), SubmitOutcome::Sent);
    // libzmq inproc pipe 는 sender/receiver queue 각각에 HWM 이 적용되어
    // HWM=1 이어도 2 frame 까지는 수용한다. 3번째부터 backpressure 가 걸린다.
    assert_eq!(
        egress.try_submit(&req, &meta).unwrap(),
        SubmitOutcome::WouldBlock
    );

    let after = snapshot_map();
    assert_eq!(counter_delta(&before, &after, "order_egress_zmq_ok"), 2);
}

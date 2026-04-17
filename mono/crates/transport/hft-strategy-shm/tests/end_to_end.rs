//! hft-strategy-shm 통합 테스트 — 전 경로 (publisher → strategies → gateway).
//!
//! 본 테스트는 crate 내부 단위 테스트(`src/lib.rs#tests`) 가 커버하지 못하는
//! **cross-role 시나리오** 를 검증한다:
//!
//! 1. Publisher 가 SharedRegion 을 초기화하고 quote/trade 를 한 번씩 흘린다.
//! 2. N 개의 StrategyShmClient 가 각자 `vm_id` 로 attach, `publish_order` 로
//!    자기 ring 에 OrderFrame 을 여러 건 푸시.
//! 3. Gateway 쪽이 `MultiOrderRingReader` 로 fan-in — 전체 개수 / VM 분포
//!    / symbol_idx 배정 일관성 검증.
//! 4. Strategy 측 QuoteSlotReader / TradeRingReader 가 publisher 가 쓴 값을
//!    동일 region 을 통해 관측하는지 확인.
//! 5. `attach_with_retry` 가 publisher boot lag 를 허용하는지 별도 스레드 테스트.

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hft_shm::{
    Backing, LayoutSpec, MultiOrderRingReader, OrderFrame, OrderKind, OrderRingWriter,
    QuoteSlotWriter, QuoteUpdate, Role, SharedRegion, SubKind, SymbolTable, TradeFrame,
    TradeRingWriter,
};
use hft_strategy_shm::StrategyShmClient;
use hft_types::ExchangeId;
use tempfile::tempdir;

fn spec(n_max: u32) -> LayoutSpec {
    LayoutSpec {
        quote_slot_count: 32,
        trade_ring_capacity: 64,
        symtab_capacity: 64,
        order_ring_capacity: 32,
        n_max,
    }
}

fn sample_frame(client_id: u64, price: i64) -> OrderFrame {
    OrderFrame {
        seq: 0,
        kind: OrderKind::Place as u8,
        exchange_id: 0,
        _pad1: [0; 2],
        symbol_idx: 0,
        side: 0,
        tif: 0,
        ord_type: 0,
        _pad2: [0; 1],
        price,
        size: 1,
        client_id,
        ts_ns: 0,
        aux: [0; 5],
        _pad3: [0; 16],
    }
}

/// 편의 함수 — publisher 가 모든 sub-region 을 초기화한 뒤 region handle 을 반환.
fn boot_publisher(path: &std::path::Path, s: LayoutSpec) -> SharedRegion {
    let sr = SharedRegion::create_or_attach(
        Backing::DevShm {
            path: path.to_path_buf(),
        },
        s,
        Role::Publisher,
    )
    .expect("create_or_attach publisher");
    let _ =
        QuoteSlotWriter::from_region(sr.sub_region(SubKind::Quote).unwrap(), s.quote_slot_count)
            .expect("init quote");
    let _ = TradeRingWriter::from_region(
        sr.sub_region(SubKind::Trade).unwrap(),
        s.trade_ring_capacity,
    )
    .expect("init trade");
    let _ = SymbolTable::from_region(sr.sub_region(SubKind::Symtab).unwrap(), s.symtab_capacity)
        .expect("init symtab");
    for vm_id in 0..s.n_max {
        let sub = sr.sub_region(SubKind::OrderRing { vm_id }).unwrap();
        let _ = OrderRingWriter::from_region(sub, s.order_ring_capacity).unwrap();
    }
    sr
}

#[test]
fn full_pipeline_publisher_n_strategies_gateway_fanin() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("pipeline");
    let s = spec(4);

    // ── Publisher: region + sub-handles ─────────────────────────────────────
    let sr_pub = boot_publisher(&path, s);
    let qw = QuoteSlotWriter::from_region(
        sr_pub.sub_region(SubKind::Quote).unwrap(),
        s.quote_slot_count,
    )
    .expect("QuoteSlotWriter");
    let tw = TradeRingWriter::from_region(
        sr_pub.sub_region(SubKind::Trade).unwrap(),
        s.trade_ring_capacity,
    )
    .expect("TradeRingWriter");
    let sym = SymbolTable::from_region(
        sr_pub.sub_region(SubKind::Symtab).unwrap(),
        s.symtab_capacity,
    )
    .expect("SymbolTable");
    let btc_idx = sym
        .get_or_intern(ExchangeId::Gate, "BTC_USDT")
        .expect("intern BTC");

    // Publisher 가 quote 최신값을 먼저 흘린다. quote slot 은 snapshot 구조라
    // strategy attach 이후에도 동일 값이 보인다.
    qw.publish(
        btc_idx,
        &QuoteUpdate {
            exchange_id: 1,
            bid_price: 99,
            bid_size: 5,
            ask_price: 101,
            ask_size: 7,
            event_ns: 10,
            recv_ns: 11,
            pub_ns: 12,
        },
    )
    .unwrap();

    // ── N 개의 strategy attach ───────────────────────────────────────────────
    let mut strategies: Vec<StrategyShmClient> = (0..s.n_max)
        .map(|vm_id| {
            StrategyShmClient::attach(Backing::DevShm { path: path.clone() }, s, vm_id)
                .expect("strategy attach")
        })
        .collect();

    // Trade ring reader 는 attach 시점부터 읽기 시작하므로, attach 후에 trade 를
    // publish 해야 모든 strategy 가 같은 1건을 소비할 수 있다.
    tw.publish(&TradeFrame {
        seq: AtomicU64::new(0),
        exchange_id: 1,
        _pad1: [0; 3],
        symbol_idx: btc_idx,
        price: 100,
        size: 2,
        trade_id: 9999,
        event_ns: 20,
        recv_ns: 21,
        pub_ns: 22,
        flags: 0,
        _pad2: [0; 28],
    });

    // 각 strategy 가 동일한 symbol 에 대해 3 건 publish.
    for client in strategies.iter() {
        for i in 0..3u64 {
            let f = sample_frame(1000 * client.vm_id() as u64 + i, 100 + i as i64);
            assert!(
                client.publish_order(ExchangeId::Gate, "BTC_USDT", f),
                "publish_order vm={} i={}",
                client.vm_id(),
                i
            );
        }
    }

    // ── symbol_idx 일관성: 모든 strategy 가 symtab intern 결과로 동일 idx 를 받아야 함 ──
    for client in strategies.iter() {
        let idx = client
            .symtab()
            .get_or_intern(ExchangeId::Gate, "BTC_USDT")
            .unwrap();
        assert_eq!(
            idx,
            btc_idx,
            "strategy vm={} symtab 이 publisher 와 다른 idx 를 반환",
            client.vm_id()
        );
    }

    // ── Gateway 측 fan-in ────────────────────────────────────────────────────
    let sr_gw = SharedRegion::open_view(
        Backing::DevShm { path: path.clone() },
        s,
        Role::OrderGateway,
    )
    .expect("open_view gateway");
    let mut multi = MultiOrderRingReader::attach(&sr_gw).expect("MultiOrderRingReader");

    let mut collected: Vec<(u32, u64, i64)> = Vec::new();
    let consumed = multi.poll_batch(16, |vm_id, frame| {
        collected.push((vm_id, frame.client_id, frame.price));
        true
    });
    assert_eq!(
        consumed,
        (3 * s.n_max) as usize,
        "total consumed {consumed}"
    );
    // 각 vm_id 에 대해 정확히 3건.
    for vm_id in 0..s.n_max {
        let n = collected.iter().filter(|(v, _, _)| *v == vm_id).count();
        assert_eq!(n, 3, "vm_id={vm_id} expected 3, got {n}");
    }
    // symbol_idx 는 OrderFrame 에 저장되지 않지만 (publish_order 가 set) 가격 증가 3종 전부 등장.
    let prices: Vec<i64> = collected.iter().map(|(_, _, p)| *p).collect();
    for p in [100i64, 101, 102] {
        assert!(prices.contains(&p), "price {p} missing from fan-in");
    }

    // ── Strategy 측이 publisher 가 쓴 quote / trade 를 볼 수 있는지 ───────────
    {
        let client = &mut strategies[0];
        let snap = client.read_quote(btc_idx).expect("quote snap via strategy");
        assert_eq!(snap.bid_price, 99);
        assert_eq!(snap.ask_price, 101);
    }
    // trade 는 SPMC broadcast — 각 strategy 각자 cursor 로 받는다.
    for client in strategies.iter_mut() {
        let t = client
            .try_consume_trade()
            .expect("trade visible to every strategy");
        assert_eq!(t.trade_id, 9999);
        assert_eq!(t.price, 100);
    }
}

/// Strategy 가 publisher 보다 먼저 부팅된 상황 — `attach_with_retry` 가 이를 흡수.
#[test]
fn attach_with_retry_waits_for_publisher() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("retry");
    let s = spec(2);
    let path_for_thread = path.clone();

    // strategy 쪽을 먼저 띄워 retry 로 대기시킨다.
    let strategy_result: Arc<Mutex<Option<Result<StrategyShmClient, anyhow::Error>>>> =
        Arc::new(Mutex::new(None));
    let result_slot = strategy_result.clone();

    let handle = std::thread::spawn(move || {
        let r = StrategyShmClient::attach_with_retry(
            Backing::DevShm {
                path: path_for_thread,
            },
            s,
            0,
            Duration::from_secs(3),
            Duration::from_millis(20),
        );
        *result_slot.lock().unwrap() = Some(r);
    });

    // 약간의 지연 후 publisher boot.
    std::thread::sleep(Duration::from_millis(150));
    let _sr_pub = boot_publisher(&path, s);

    // strategy 측 스레드가 attach_with_retry 를 완료할 때까지 대기.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if strategy_result.lock().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("attach_with_retry 가 publisher 부팅 후에도 종료되지 않음");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    handle.join().unwrap();

    let mut guard = strategy_result.lock().unwrap();
    let r = guard.take().unwrap();
    let client = r.expect("attach_with_retry should succeed after publisher boots");
    assert_eq!(client.vm_id(), 0);
}

/// n_max 를 넘는 vm_id 는 StrategyShmClient::attach 단에서 거절된다 (policy guard).
#[test]
fn vm_id_over_n_max_rejected_at_facade() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("oob");
    let s = spec(3);
    let _sr_pub = boot_publisher(&path, s);

    let err = StrategyShmClient::attach(
        Backing::DevShm { path: path.clone() },
        s,
        3, // n_max=3 → valid 는 0..3
    )
    .expect_err("out-of-range vm_id should fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("out of range") || msg.contains("vm_id"),
        "unexpected error: {msg}"
    );
}

//! SharedRegion end-to-end integration test.
//!
//! 다루는 시나리오:
//! 1. Publisher 가 region 을 열고 Quote/Trade/Symtab/OrderRing 모두 초기화.
//! 2. N 개의 "Strategy" 각자 자기 vm_id 의 order ring writer 를 연다.
//! 3. Gateway 가 open_view + MultiOrderRingReader 로 fan-in.
//! 4. 동일 region 위에서 quote roundtrip + trade roundtrip + order fan-in 검증.
//! 5. Layout digest mismatch 시 strict reject.

use hft_shm::{
    Backing, LayoutSpec, MultiOrderRingReader, OrderFrame, OrderKind, OrderRingWriter,
    QuoteSlotReader, QuoteSlotWriter, QuoteUpdate, Role, SharedRegion, SubKind, SymbolTable,
    TradeFrame, TradeRingReader, TradeRingWriter,
};
use std::sync::atomic::AtomicU64;
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

#[test]
fn publisher_gateway_strategies_interact_on_single_region() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("shared-e2e");
    let s = spec(4);

    // ── Publisher attaches with writable role and initializes all sub-regions ──
    let sr_pub = SharedRegion::create_or_attach(
        Backing::DevShm { path: p.clone() },
        s,
        Role::Publisher,
    )
    .expect("create_or_attach publisher");

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
    let sym_w = SymbolTable::from_region(
        sr_pub.sub_region(SubKind::Symtab).unwrap(),
        s.symtab_capacity,
    )
    .expect("SymbolTable");
    let btc = sym_w
        .get_or_intern(hft_types::ExchangeId::Gate, "BTC_USDT")
        .unwrap();

    // publisher initializes all N order rings so gateway strict attach is OK.
    for vm_id in 0..s.n_max {
        OrderRingWriter::from_region(
            sr_pub.sub_region(SubKind::OrderRing { vm_id }).unwrap(),
            s.order_ring_capacity,
        )
        .expect("order ring init");
    }

    // ── Strategies attach with `Strategy{vm_id}` role and open their own ring ──
    let mut strat_writers = Vec::new();
    for vm_id in 0..s.n_max {
        let sr_strat = SharedRegion::open_view(
            Backing::DevShm { path: p.clone() },
            s,
            Role::Strategy { vm_id },
        )
        .expect("open_view strategy");
        let w = OrderRingWriter::from_region(
            sr_strat
                .sub_region(SubKind::OrderRing { vm_id })
                .unwrap(),
            s.order_ring_capacity,
        )
        .expect("strategy OrderRingWriter");
        strat_writers.push(w);
    }

    // ── Gateway attaches read-only to the whole set of order rings ──
    let sr_gw = SharedRegion::open_view(
        Backing::DevShm { path: p.clone() },
        s,
        Role::OrderGateway,
    )
    .expect("open_view gateway");
    let mut multi = MultiOrderRingReader::attach(&sr_gw).expect("MultiOrderRingReader");

    // ── Publisher writes 1 quote + 1 trade ──
    let update = QuoteUpdate {
        exchange_id: 1,
        bid_price: 100,
        bid_size: 10,
        ask_price: 101,
        ask_size: 20,
        event_ns: 123,
        recv_ns: 124,
        pub_ns: 125,
    };
    qw.publish(btc, &update).unwrap();
    let trade = TradeFrame {
        seq: AtomicU64::new(0),
        exchange_id: 1,
        _pad1: [0; 3],
        symbol_idx: btc,
        price: 100,
        size: 5,
        trade_id: 777,
        event_ns: 1,
        recv_ns: 2,
        pub_ns: 3,
        flags: 0,
        _pad2: [0; 28],
    };
    tw.publish(&trade);

    // ── Strategies all push 3 orders to their ring ──
    for (vm_id, w) in strat_writers.iter().enumerate() {
        for i in 0..3u64 {
            let f = OrderFrame {
                seq: 0,
                kind: OrderKind::Place as u8,
                exchange_id: 1,
                _pad1: [0; 2],
                symbol_idx: btc,
                side: 0,
                tif: 0,
                ord_type: 0,
                _pad2: [0; 1],
                price: 100 + i as i64,
                size: 1,
                client_id: 1000 * vm_id as u64 + i,
                ts_ns: 0,
                aux: [0; 5],
                _pad3: [0; 16],
            };
            assert!(w.publish(&f), "publish order vm={vm_id} i={i}");
        }
    }

    // ── Gateway polls fan-in; expects 3 × n_max = 12 ──
    let mut seen: Vec<(u32, u64)> = Vec::new();
    let got = multi.poll_batch(16, |vm_id, f| {
        seen.push((vm_id, f.client_id));
        true
    });
    assert_eq!(got, (3 * s.n_max) as usize, "collected {got}");
    for vm_id in 0..s.n_max {
        let count = seen.iter().filter(|(v, _)| *v == vm_id).count();
        assert_eq!(count, 3, "vm_id={vm_id} got {count}");
    }

    // ── Readers on quote + trade from separate ReadOnly view ──
    let sr_ro = SharedRegion::open_view(
        Backing::DevShm { path: p.clone() },
        s,
        Role::ReadOnly,
    )
    .expect("open_view read-only");
    let qr = QuoteSlotReader::from_region(sr_ro.sub_region(SubKind::Quote).unwrap())
        .expect("QuoteSlotReader");
    let snap = qr.read(btc).expect("quote snap");
    assert_eq!(snap.bid_price, 100);
    assert_eq!(snap.ask_price, 101);

    let mut tr = TradeRingReader::from_region(sr_ro.sub_region(SubKind::Trade).unwrap())
        .expect("TradeRingReader");
    let f = tr.try_consume().expect("trade consume");
    assert_eq!(f.trade_id, 777);
    assert_eq!(f.price, 100);
}

#[test]
fn layout_digest_mismatch_is_rejected() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("digest-mismatch");

    // publisher creates with spec A.
    let spec_a = LayoutSpec {
        quote_slot_count: 8,
        trade_ring_capacity: 16,
        symtab_capacity: 8,
        order_ring_capacity: 16,
        n_max: 2,
    };
    let _sr =
        SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, spec_a, Role::Publisher)
            .expect("pub with spec_a");

    // reader opens with spec B (different n_max). digest mismatch.
    let spec_b = LayoutSpec {
        n_max: 4, // DIFFERENT
        ..spec_a
    };
    let err = SharedRegion::open_view(Backing::DevShm { path: p.clone() }, spec_b, Role::ReadOnly)
        .expect_err("open_view should fail on digest mismatch");
    let msg = format!("{err}");
    assert!(
        msg.to_ascii_lowercase().contains("layout") || msg.to_ascii_lowercase().contains("digest"),
        "unexpected error: {msg}"
    );
}

/// Publisher touch_heartbeat → reader 가 heartbeat_ns / heartbeat_seq / age 를
/// 관찰. non-publisher 가 touch 호출 시 silently ignore 되는 것도 확인.
#[test]
fn heartbeat_publish_and_observe() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("heartbeat");
    let s = spec(2);

    let sr_pub = SharedRegion::create_or_attach(
        Backing::DevShm { path: p.clone() },
        s,
        Role::Publisher,
    )
    .expect("create_or_attach publisher");

    // publisher 가 아직 touch 하지 않은 상태 — 모두 0 / None.
    assert_eq!(sr_pub.heartbeat_ns(), 0, "initial heartbeat_ns must be 0");
    assert_eq!(sr_pub.heartbeat_seq(), 0, "initial heartbeat_seq must be 0");
    assert!(
        sr_pub.heartbeat_age_ns(1_000_000_000).is_none(),
        "age 는 publisher 가 touch 하기 전에는 None"
    );

    // publisher 관점에서 touch.
    sr_pub.touch_heartbeat(1_000);
    assert_eq!(sr_pub.heartbeat_ns(), 1_000);
    assert_eq!(sr_pub.heartbeat_seq(), 1);

    // 별도 view (gateway 또는 strategy 관점) 에서 관찰.
    let sr_gw = SharedRegion::open_view(
        Backing::DevShm { path: p.clone() },
        s,
        Role::OrderGateway,
    )
    .expect("open_view gateway");
    assert_eq!(sr_gw.heartbeat_ns(), 1_000);
    assert_eq!(sr_gw.heartbeat_seq(), 1);
    assert_eq!(
        sr_gw.heartbeat_age_ns(1_500),
        Some(500),
        "age = now - last"
    );
    // 역주행 시 None.
    assert!(
        sr_gw.heartbeat_age_ns(500).is_none(),
        "now < last 이면 None"
    );

    // strategy view 에서 잘못된 touch 호출 — silently ignore.
    // (단, 우리는 이 호출이 header state 를 건드리지 않는 것만 확인.)
    let sr_strat = SharedRegion::open_view(
        Backing::DevShm { path: p.clone() },
        s,
        Role::Strategy { vm_id: 0 },
    )
    .expect("open_view strategy");
    sr_strat.touch_heartbeat(999_999_999); // ignored
    assert_eq!(
        sr_gw.heartbeat_seq(),
        1,
        "non-publisher touch must not advance seq"
    );
    assert_eq!(sr_gw.heartbeat_ns(), 1_000);

    // publisher 가 한 번 더 touch → seq=2.
    sr_pub.touch_heartbeat(2_000);
    assert_eq!(sr_gw.heartbeat_ns(), 2_000);
    assert_eq!(sr_gw.heartbeat_seq(), 2);
}

#[test]
fn reattach_same_spec_is_ok() {
    let dir = tempdir().unwrap();
    let p = dir.path().join("reattach");
    let s = spec(3);

    // first create.
    let sr1 =
        SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
            .unwrap();
    let _q = QuoteSlotWriter::from_region(
        sr1.sub_region(SubKind::Quote).unwrap(),
        s.quote_slot_count,
    )
    .unwrap();
    drop(sr1);

    // reattach with same spec must succeed (magic valid, digest matches).
    let sr2 =
        SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
            .expect("reattach with same spec");
    // sub-region still reachable.
    let _q = QuoteSlotWriter::from_region(
        sr2.sub_region(SubKind::Quote).unwrap(),
        s.quote_slot_count,
    )
    .expect("reopen quote writer");
}

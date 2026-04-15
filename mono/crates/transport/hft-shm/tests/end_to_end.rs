//! End-to-end 통합 테스트 — 3 영역(quote / trade / order) + symbol table 을
//! 실제 사용 흐름처럼 엮어본다.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hft_shm::*;
use hft_types::ExchangeId;
use tempfile::tempdir;

#[test]
fn e2e_publisher_writes_strategy_reads() {
    let dir = tempdir().unwrap();
    let qpath = dir.path().join("quotes_v2");
    let tpath = dir.path().join("trades_v2");
    let spath = dir.path().join("symtab_v2");

    // publisher side
    let symtab = Arc::new(SymbolTable::open_or_create(&spath, 1024).unwrap());
    let qw = QuoteSlotWriter::create(&qpath, 256).unwrap();
    let tw = TradeRingWriter::create(&tpath, 1024).unwrap();
    let idx_btc = symtab.get_or_intern(ExchangeId::Gate, "BTC_USDT").unwrap();
    let idx_eth = symtab.get_or_intern(ExchangeId::Gate, "ETH_USDT").unwrap();

    // strategy side
    let symtab_r = SymbolTable::open(&spath).unwrap();
    let qr = QuoteSlotReader::open(&qpath).unwrap();
    let mut tr = TradeRingReader::open(&tpath).unwrap();

    // publish quotes
    qw.publish(
        idx_btc,
        &QuoteUpdate {
            exchange_id: exchange_to_u8(ExchangeId::Gate),
            bid_price: 5_000_000,
            bid_size: 10,
            ask_price: 5_000_100,
            ask_size: 11,
            event_ns: 1_000,
            recv_ns: 1_001,
            pub_ns: 1_002,
        },
    )
    .unwrap();
    qw.publish(
        idx_eth,
        &QuoteUpdate {
            exchange_id: exchange_to_u8(ExchangeId::Gate),
            bid_price: 250_000,
            bid_size: 100,
            ask_price: 250_100,
            ask_size: 110,
            event_ns: 2_000,
            recv_ns: 2_001,
            pub_ns: 2_002,
        },
    )
    .unwrap();

    // publish trades
    for i in 0..5 {
        let frame = TradeFrame {
            seq: std::sync::atomic::AtomicU64::new(0),
            exchange_id: exchange_to_u8(ExchangeId::Gate),
            _pad1: [0; 3],
            symbol_idx: idx_btc,
            price: 5_000_000 + i,
            size: 1 + i,
            trade_id: 10_000 + i,
            event_ns: i as u64 + 1000,
            recv_ns: i as u64 + 1001,
            pub_ns: i as u64 + 1002,
            flags: 0,
            _pad2: [0; 28],
        };
        tw.publish(&frame);
    }

    // read back
    let s_btc = qr.read(idx_btc).unwrap();
    assert_eq!(s_btc.bid_price, 5_000_000);
    assert_eq!(s_btc.ask_size, 11);

    let s_eth = qr.read(idx_eth).unwrap();
    assert_eq!(s_eth.bid_price, 250_000);

    let mut got = Vec::new();
    tr.drain_into(&mut got, 10);
    assert_eq!(got.len(), 5);
    assert_eq!(got[0].price, 5_000_000);
    assert_eq!(got[4].price, 5_000_004);

    // symbol table roundtrip
    assert_eq!(symtab_r.lookup(ExchangeId::Gate, "BTC_USDT"), Some(idx_btc));
    let (ex, name) = symtab_r.resolve(idx_btc).unwrap();
    assert_eq!(ex, ExchangeId::Gate);
    assert_eq!(name, "BTC_USDT");
}

#[test]
fn e2e_concurrent_publisher_and_two_readers() {
    let dir = tempdir().unwrap();
    let tpath = dir.path().join("trades_v2");
    let w = Arc::new(TradeRingWriter::create(&tpath, 4096).unwrap());
    let mut r1 = TradeRingReader::open(&tpath).unwrap();
    let mut r2 = TradeRingReader::open(&tpath).unwrap();
    let stop = Arc::new(AtomicBool::new(false));

    let stop_w = stop.clone();
    let w_c = w.clone();
    let writer_t = thread::spawn(move || {
        let mut i = 0i64;
        while !stop_w.load(Ordering::Relaxed) && i < 20_000 {
            let f = TradeFrame {
                seq: std::sync::atomic::AtomicU64::new(0),
                exchange_id: 2,
                _pad1: [0; 3],
                symbol_idx: 0,
                price: i,
                size: 1,
                trade_id: i,
                event_ns: i as u64,
                recv_ns: i as u64,
                pub_ns: i as u64,
                flags: 0,
                _pad2: [0; 28],
            };
            w_c.publish(&f);
            i += 1;
        }
    });

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut a = Vec::new();
    let mut b = Vec::new();
    while Instant::now() < deadline && (a.len() < 15_000 || b.len() < 15_000) {
        r1.drain_into(&mut a, 1024);
        r2.drain_into(&mut b, 1024);
    }
    stop.store(true, Ordering::Relaxed);
    writer_t.join().unwrap();

    assert!(a.len() >= 5000, "reader1 too few: {}", a.len());
    assert!(b.len() >= 5000, "reader2 too few: {}", b.len());
    // 둘 다 동일 seq 를 보고 있어야 하지만 각자 독립이라 subset 이어도 OK.
}

#[test]
fn order_ring_spsc_end_to_end() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("orders_v2");
    let w = OrderRingWriter::create(&path, 64).unwrap();
    let mut r = OrderRingReader::open(&path).unwrap();

    for i in 0..30u64 {
        let f = OrderFrame {
            seq: 0,
            kind: OrderKind::Place as u8,
            exchange_id: exchange_to_u8(ExchangeId::Gate),
            _pad1: [0; 2],
            symbol_idx: 7,
            side: (i & 1) as u8,
            tif: 0,
            ord_type: 0,
            _pad2: [0; 1],
            price: 100_000 + i as i64,
            size: 1 + i as i64,
            client_id: i,
            ts_ns: i * 1000,
            aux: [i; 5],
            _pad3: [0; 16],
        };
        assert!(w.publish(&f));
    }
    let mut got = Vec::new();
    r.drain_into(&mut got, 100);
    assert_eq!(got.len(), 30);
    for (i, o) in got.iter().enumerate() {
        assert_eq!(o.client_id, i as u64);
        assert_eq!(o.kind, OrderKind::Place as u8);
    }
}

#[test]
fn writer_restart_preserves_readers() {
    // publisher 가 죽고 다시 올라와도 같은 SHM 을 계속 쓸 수 있어야 한다.
    let dir = tempdir().unwrap();
    let p = dir.path().join("quotes_r");
    {
        let w = QuoteSlotWriter::create(&p, 32).unwrap();
        w.publish(
            3,
            &QuoteUpdate {
                exchange_id: 1,
                bid_price: 100,
                bid_size: 1,
                ask_price: 101,
                ask_size: 1,
                event_ns: 0,
                recv_ns: 0,
                pub_ns: 0,
            },
        )
        .unwrap();
    }
    // 재시작.
    let w2 = QuoteSlotWriter::create(&p, 32).unwrap();
    // 이전 값 확인.
    let r = QuoteSlotReader::open(&p).unwrap();
    assert_eq!(r.read(3).unwrap().bid_price, 100);
    // 새 값 쓰기.
    w2.publish(
        3,
        &QuoteUpdate {
            exchange_id: 1,
            bid_price: 200,
            bid_size: 2,
            ask_price: 201,
            ask_size: 2,
            event_ns: 0,
            recv_ns: 0,
            pub_ns: 0,
        },
    )
    .unwrap();
    assert_eq!(r.read(3).unwrap().bid_price, 200);
}

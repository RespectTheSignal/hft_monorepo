use criterion::{black_box, criterion_group, criterion_main, Criterion};
use hft_protocol::order_wire::{
    OrderRequestWire, OrderResultWire, FLAG_REDUCE_ONLY, LEVEL_CLOSE, ORDER_REQUEST_WIRE_SIZE,
    ORDER_RESULT_WIRE_SIZE, ORDER_TYPE_LIMIT, SIDE_BUY, STATUS_ACCEPTED, TIF_GTC,
};
use hft_protocol::{decode_bookticker, decode_trade, encode_bookticker_into, encode_trade_into};
use hft_types::{BookTicker, ExchangeId, Price, Size, Symbol, Trade};

/// 현실적인 BookTicker fixture.
fn sample_bookticker() -> BookTicker {
    BookTicker {
        exchange: ExchangeId::Gate,
        symbol: Symbol::new("BTC_USDT"),
        bid_price: Price(65_431.25),
        ask_price: Price(65_431.75),
        bid_size: Size(12.5),
        ask_size: Size(8.75),
        event_time_ms: 1_765_432_100_123,
        server_time_ms: 1_765_432_100_120,
    }
}

/// 현실적인 Trade fixture.
fn sample_trade() -> Trade {
    Trade {
        exchange: ExchangeId::Binance,
        symbol: Symbol::new("ETH_USDT"),
        price: Price(3_245.125),
        size: Size(-4.25),
        trade_id: 9_876_543_210,
        create_time_s: 1_765_432_100,
        create_time_ms: 1_765_432_100_456,
        server_time_ms: 1_765_432_100_460,
        is_internal: false,
    }
}

/// 현실적인 OrderRequestWire fixture.
fn sample_order_request() -> OrderRequestWire {
    let mut text_tag = [0u8; 32];
    text_tag[..11].copy_from_slice(b"v7_ma_cross");
    OrderRequestWire {
        exchange: 2,
        side: SIDE_BUY,
        order_type: ORDER_TYPE_LIMIT,
        symbol_id: 0xDEAD_BEEF,
        tif: TIF_GTC,
        level: LEVEL_CLOSE,
        flags: FLAG_REDUCE_ONLY,
        _pad0: 0,
        _pad1: 0,
        price: 65_432.10,
        size: 1_000_000,
        client_seq: 0x0123_4567_89AB_CDEF,
        origin_ts_ns: 1_765_432_100_000_000_000,
        text_tag,
        _reserved: [0; 48],
    }
}

/// 현실적인 OrderResultWire fixture.
fn sample_order_result() -> OrderResultWire {
    let mut exchange_order_id = [0u8; 48];
    exchange_order_id[..11].copy_from_slice(b"EX123456789");
    let mut text_tag = [0u8; 32];
    text_tag[..11].copy_from_slice(b"v7_ma_cross");
    OrderResultWire {
        client_seq: 0x0123_4567_89AB_CDEF,
        gateway_ts_ns: 1_765_432_100_500_000_000,
        filled_size: 999_999,
        reject_code: 0,
        status: STATUS_ACCEPTED,
        _pad0: [0; 3],
        exchange_order_id,
        text_tag,
        _reserved: [0; 16],
    }
}

fn bench_encode_bookticker(c: &mut Criterion) {
    let bt = sample_bookticker();
    let mut buf = [0u8; 120];
    c.bench_function("encode_bookticker", |b| {
        b.iter(|| encode_bookticker_into(black_box(&mut buf), black_box(&bt)))
    });
}

fn bench_decode_bookticker(c: &mut Criterion) {
    let bt = sample_bookticker();
    let mut buf = [0u8; 120];
    encode_bookticker_into(&mut buf, &bt);
    c.bench_function("decode_bookticker", |b| {
        b.iter(|| black_box(decode_bookticker(black_box(&buf)).expect("decode bookticker")))
    });
}

fn bench_encode_trade(c: &mut Criterion) {
    let trade = sample_trade();
    let mut buf = [0u8; 128];
    c.bench_function("encode_trade", |b| {
        b.iter(|| encode_trade_into(black_box(&mut buf), black_box(&trade)))
    });
}

fn bench_decode_trade(c: &mut Criterion) {
    let trade = sample_trade();
    let mut buf = [0u8; 128];
    encode_trade_into(&mut buf, &trade);
    c.bench_function("decode_trade", |b| {
        b.iter(|| black_box(decode_trade(black_box(&buf)).expect("decode trade")))
    });
}

fn bench_encode_order_request(c: &mut Criterion) {
    let req = sample_order_request();
    let mut buf = [0u8; ORDER_REQUEST_WIRE_SIZE];
    c.bench_function("encode_order_request", |b| {
        b.iter(|| req.encode(black_box(&mut buf)))
    });
}

fn bench_decode_order_request(c: &mut Criterion) {
    let req = sample_order_request();
    let mut buf = [0u8; ORDER_REQUEST_WIRE_SIZE];
    req.encode(&mut buf);
    c.bench_function("decode_order_request", |b| {
        b.iter(|| black_box(OrderRequestWire::decode(black_box(&buf)).expect("decode request")))
    });
}

fn bench_encode_order_result(c: &mut Criterion) {
    let result = sample_order_result();
    let mut buf = [0u8; ORDER_RESULT_WIRE_SIZE];
    c.bench_function("encode_order_result", |b| {
        b.iter(|| result.encode(black_box(&mut buf)))
    });
}

fn bench_decode_order_result(c: &mut Criterion) {
    let result = sample_order_result();
    let mut buf = [0u8; ORDER_RESULT_WIRE_SIZE];
    result.encode(&mut buf);
    c.bench_function("decode_order_result", |b| {
        b.iter(|| black_box(OrderResultWire::decode(black_box(&buf)).expect("decode result")))
    });
}

criterion_group!(
    benches,
    bench_encode_bookticker,
    bench_decode_bookticker,
    bench_encode_trade,
    bench_decode_trade,
    bench_encode_order_request,
    bench_decode_order_request,
    bench_encode_order_result,
    bench_decode_order_result,
);
criterion_main!(benches);

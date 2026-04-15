//! End-to-end: domain → wire → frame → parse frame → decode → domain.
//!
//! publisher 가 하는 전체 직렬화 경로를 한 테스트에서 검증.

use hft_protocol::{
    create_message, decode_bookticker, decode_trade, encode_bookticker_into, encode_trade_into,
    parse_frame, parse_topic, patch_bookticker_pushed_ms, patch_trade_pushed_ms, TopicBuilder,
    BOOK_TICKER_SIZE, MSG_BOOKTICKER, MSG_TRADE, TRADE_SIZE,
};
use hft_types::{BookTicker, ExchangeId, Price, Size, Symbol, Trade};

#[test]
fn bookticker_full_pipeline() {
    let bt = BookTicker {
        exchange: ExchangeId::Gate,
        symbol: Symbol::new("BTC_USDT"),
        bid_price: Price(29_950.25),
        ask_price: Price(29_951.75),
        bid_size: Size(12.5),
        ask_size: Size(8.75),
        event_time_ms: 1_700_000_000_000,
        server_time_ms: 1_700_000_000_050,
    };

    // 1) 와이어 직렬화
    let mut wire_buf = [0u8; BOOK_TICKER_SIZE];
    encode_bookticker_into(&mut wire_buf, &bt);

    // 2) aggregator 가 PUSH 직전 publisher_sent_ms patch
    patch_bookticker_pushed_ms(&mut wire_buf, 1_700_000_000_100);

    // 3) 프레이밍 (type_len + type + payload)
    let frame = create_message(MSG_BOOKTICKER, &wire_buf).unwrap();
    assert_eq!(frame.len(), 1 + MSG_BOOKTICKER.len() + BOOK_TICKER_SIZE);

    // 4) 토픽 생성 (publisher 쪽)
    let topic = TopicBuilder::from_event(&hft_types::MarketEvent::BookTicker(bt.clone()));
    assert_eq!(topic, "gate_bookticker_BTC_USDT");

    // 5) subscriber 쪽: 토픽 파싱
    let parsed_topic = parse_topic(&topic).unwrap();
    assert_eq!(parsed_topic.exchange, ExchangeId::Gate);
    assert_eq!(parsed_topic.msg_type, MSG_BOOKTICKER);
    assert_eq!(parsed_topic.symbol.as_str(), "BTC_USDT");

    // 6) subscriber: 프레임 parse
    let view = parse_frame(&frame).unwrap();
    assert_eq!(view.msg_type, MSG_BOOKTICKER);
    assert_eq!(view.payload.len(), BOOK_TICKER_SIZE);

    // 7) 와이어 디코드
    let wire = decode_bookticker(view.payload).unwrap();
    assert_eq!(wire.publisher_sent_ms, 1_700_000_000_100);

    // 8) 도메인으로 복원
    let restored = wire.into_domain();
    assert_eq!(restored, bt);
}

#[test]
fn trade_full_pipeline() {
    let t = Trade {
        exchange: ExchangeId::Binance,
        symbol: Symbol::new("ETH_USDT"),
        price: Price(1_800.5),
        size: Size(-0.25),
        trade_id: 987_654_321,
        create_time_s: 1_700_000_000,
        create_time_ms: 1_700_000_000_321,
        server_time_ms: 1_700_000_000_400,
        is_internal: true,
    };

    let mut wire_buf = [0u8; TRADE_SIZE];
    encode_trade_into(&mut wire_buf, &t);
    patch_trade_pushed_ms(&mut wire_buf, 1_700_000_000_500);

    let frame = create_message(MSG_TRADE, &wire_buf).unwrap();
    let view = parse_frame(&frame).unwrap();
    assert_eq!(view.msg_type, MSG_TRADE);

    let wire = decode_trade(view.payload).unwrap();
    assert_eq!(wire.publisher_sent_ms, 1_700_000_000_500);

    let restored = wire.into_domain();
    assert_eq!(restored, t);
}

#[test]
fn five_exchanges_all_encode_decode_cleanly() {
    for &exchange in &ExchangeId::ALL {
        let bt = BookTicker {
            exchange,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(100.0),
            ask_price: Price(100.1),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: 1,
            server_time_ms: 2,
        };
        let mut buf = [0u8; BOOK_TICKER_SIZE];
        encode_bookticker_into(&mut buf, &bt);
        let wire = decode_bookticker(&buf).unwrap();
        assert_eq!(wire.exchange, exchange, "exchange {:?} roundtrip", exchange);
    }
}

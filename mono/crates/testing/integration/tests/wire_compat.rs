//! wire_compat — 와이어 포맷·프레임·토픽의 **integration 수준** 호환성 테스트.
//!
//! # 범위
//! 1. `encode_*_into` ↔ `decode_*` 의 byte-exact roundtrip.
//! 2. `patch_*_pushed_ms` 가 오직 해당 offset 8바이트만 덮어쓰는지.
//! 3. `[type_len][type][payload]` 프레임이 파이프라인 전 구간 (publisher → subscriber)
//!    에서 보존되는지 (frame ↔ payload zero-copy 경계).
//! 4. Topic 빌더 / 파서 역관계.
//! 5. `BookTickerWire::into_domain` 이 `BookTicker` 필드 전부를 1:1 보존.
//!
//! crate 내부 unit test 와 중복되는 부분도 있지만, integration 수준에선 **여러
//! 모듈을 함께** 검증하는 것이 목적 — 리팩토링으로 한쪽이 깨져도 다른 쪽에서
//! 잡도록 이중화한다. check.md §wire 의 "byte-exact parity" 요구를 CI 게이트로.

use byteorder::{ByteOrder, LittleEndian};
use hft_protocol::{
    create_message, decode_bookticker, decode_trade, encode_bookticker_into, encode_trade_into,
    parse_frame, parse_topic, patch_bookticker_pushed_ms, patch_trade_pushed_ms, write_frame_into,
    TopicBuilder, BOOKTICKER_PUBLISHER_SENT_OFFSET, BOOK_TICKER_SIZE, MSG_BOOKTICKER, MSG_TRADE,
    MSG_WEBBOOKTICKER, TRADE_PUBLISHER_SENT_OFFSET, TRADE_SIZE,
};
use hft_types::{BookTicker, ExchangeId, MarketEvent, Price, Size, Symbol, Trade};

// ─────────────────────────────────────────────────────────────────────────────
// 샘플 helpers — 각 테스트가 같은 기준점을 공유하도록.
// ─────────────────────────────────────────────────────────────────────────────

fn sample_bt() -> BookTicker {
    BookTicker {
        exchange: ExchangeId::Gate,
        symbol: Symbol::new("BTC_USDT"),
        bid_price: Price(30_000.123_456_789),
        ask_price: Price(30_001.987_654_321),
        bid_size: Size(0.5),
        ask_size: Size(1.25),
        event_time_ms: 1_700_000_000_000,
        server_time_ms: 1_700_000_000_050,
    }
}

fn sample_trade() -> Trade {
    Trade {
        exchange: ExchangeId::Binance,
        symbol: Symbol::new("ETH_USDT"),
        price: Price(1_800.5),
        size: Size(-0.25), // sell
        trade_id: 42,
        create_time_s: 1_700_000_000,
        create_time_ms: 1_700_000_000_050,
        server_time_ms: 1_700_000_000_050,
        is_internal: false,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. wire roundtrip
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn bookticker_encode_decode_roundtrip_is_exact() {
    let bt = sample_bt();
    let mut buf = [0u8; BOOK_TICKER_SIZE];
    encode_bookticker_into(&mut buf, &bt);

    let wire = decode_bookticker(&buf).expect("decode ok");
    assert_eq!(wire.exchange, bt.exchange);
    assert_eq!(wire.symbol, bt.symbol.as_str());
    assert_eq!(wire.bid_price, bt.bid_price.0);
    assert_eq!(wire.ask_price, bt.ask_price.0);
    assert_eq!(wire.bid_size, bt.bid_size.0);
    assert_eq!(wire.ask_size, bt.ask_size.0);
    assert_eq!(wire.event_time_ms, bt.event_time_ms);
    assert_eq!(wire.server_time_ms, bt.server_time_ms);
    // encode 직후엔 stamps 전부 0 (publisher_sent / sub_received / sub_dump).
    assert_eq!(wire.publisher_sent_ms, 0);
    assert_eq!(wire.subscriber_received_ms, 0);
    assert_eq!(wire.subscriber_dump_ms, 0);

    // 도메인 변환 후에도 값 불변.
    let back = wire.into_domain();
    assert_eq!(back, bt);
}

#[test]
fn trade_encode_decode_roundtrip_is_exact() {
    let t = sample_trade();
    let mut buf = [0u8; TRADE_SIZE];
    encode_trade_into(&mut buf, &t);

    let wire = decode_trade(&buf).expect("decode ok");
    assert_eq!(wire.exchange, t.exchange);
    assert_eq!(wire.symbol, t.symbol.as_str());
    assert_eq!(wire.price, t.price.0);
    assert_eq!(wire.size, t.size.0);
    assert_eq!(wire.trade_id, t.trade_id);
    assert_eq!(wire.create_time_s, t.create_time_s);
    assert_eq!(wire.create_time_ms, t.create_time_ms);
    assert_eq!(wire.is_internal, t.is_internal);
    assert_eq!(wire.server_time_ms, t.server_time_ms);
    assert_eq!(wire.publisher_sent_ms, 0);

    let back = wire.into_domain();
    assert_eq!(back, t);
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. patch_*_pushed_ms 는 8바이트만 수정 (나머지는 그대로)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn patch_bookticker_only_touches_publisher_sent_offset() {
    let bt = sample_bt();
    let mut buf = [0u8; BOOK_TICKER_SIZE];
    encode_bookticker_into(&mut buf, &bt);
    let before = buf;

    let pushed = 1_700_000_000_123_i64;
    patch_bookticker_pushed_ms(&mut buf, pushed);

    // 패치된 offset 값이 정확히 기록됐는가.
    let read_back = LittleEndian::read_i64(
        &buf[BOOKTICKER_PUBLISHER_SENT_OFFSET..BOOKTICKER_PUBLISHER_SENT_OFFSET + 8],
    );
    assert_eq!(read_back, pushed);

    // 앞 96B / 뒤 16B 는 완전히 동일해야 함.
    assert_eq!(
        &before[..BOOKTICKER_PUBLISHER_SENT_OFFSET],
        &buf[..BOOKTICKER_PUBLISHER_SENT_OFFSET],
        "patch must not modify pre-offset bytes"
    );
    assert_eq!(
        &before[BOOKTICKER_PUBLISHER_SENT_OFFSET + 8..],
        &buf[BOOKTICKER_PUBLISHER_SENT_OFFSET + 8..],
        "patch must not modify post-offset bytes"
    );

    // decode 로 꺼내봐도 동일.
    let wire = decode_bookticker(&buf).expect("decode ok");
    assert_eq!(wire.publisher_sent_ms, pushed);
    assert_eq!(wire.bid_price, bt.bid_price.0);
}

#[test]
fn patch_trade_only_touches_publisher_sent_offset() {
    let t = sample_trade();
    let mut buf = [0u8; TRADE_SIZE];
    encode_trade_into(&mut buf, &t);
    let before = buf;

    let pushed = 1_700_000_000_999_i64;
    patch_trade_pushed_ms(&mut buf, pushed);

    let read_back = LittleEndian::read_i64(
        &buf[TRADE_PUBLISHER_SENT_OFFSET..TRADE_PUBLISHER_SENT_OFFSET + 8],
    );
    assert_eq!(read_back, pushed);

    assert_eq!(
        &before[..TRADE_PUBLISHER_SENT_OFFSET],
        &buf[..TRADE_PUBLISHER_SENT_OFFSET],
    );
    assert_eq!(
        &before[TRADE_PUBLISHER_SENT_OFFSET + 8..],
        &buf[TRADE_PUBLISHER_SENT_OFFSET + 8..],
    );

    let wire = decode_trade(&buf).expect("decode ok");
    assert_eq!(wire.publisher_sent_ms, pushed);
    assert_eq!(wire.trade_id, t.trade_id);
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. 프레임 레이어 — encode → frame → parse_frame → decode 전 파이프
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn bookticker_frame_roundtrip_through_publisher_subscriber_path() {
    let bt = sample_bt();
    let mut payload = [0u8; BOOK_TICKER_SIZE];
    encode_bookticker_into(&mut payload, &bt);
    patch_bookticker_pushed_ms(&mut payload, 1_700_000_000_500);

    let frame = create_message(MSG_BOOKTICKER, &payload).expect("create frame");

    // 기대 레이아웃: [10]["bookticker"][120B payload]
    assert_eq!(frame[0], MSG_BOOKTICKER.len() as u8);
    assert_eq!(&frame[1..11], MSG_BOOKTICKER.as_bytes());
    assert_eq!(frame.len(), 1 + MSG_BOOKTICKER.len() + BOOK_TICKER_SIZE);

    // subscriber 경로: parse_frame → decode.
    let view = parse_frame(&frame).expect("parse_frame ok");
    assert_eq!(view.msg_type, MSG_BOOKTICKER);
    assert_eq!(view.payload.len(), BOOK_TICKER_SIZE);

    let wire = decode_bookticker(view.payload).expect("decode payload");
    assert_eq!(wire.bid_price, bt.bid_price.0);
    assert_eq!(wire.publisher_sent_ms, 1_700_000_000_500);
}

#[test]
fn trade_frame_roundtrip_through_publisher_subscriber_path() {
    let t = sample_trade();
    let mut payload = [0u8; TRADE_SIZE];
    encode_trade_into(&mut payload, &t);
    patch_trade_pushed_ms(&mut payload, 1_700_000_000_777);

    let frame = create_message(MSG_TRADE, &payload).expect("create frame");
    let view = parse_frame(&frame).expect("parse ok");
    assert_eq!(view.msg_type, MSG_TRADE);

    let wire = decode_trade(view.payload).expect("decode ok");
    assert_eq!(wire.trade_id, t.trade_id);
    assert_eq!(wire.publisher_sent_ms, 1_700_000_000_777);
}

#[test]
fn write_frame_into_matches_create_message() {
    // hot-path (aggregator) 의 재사용 버퍼 경로와 slow-path (디버그용) 가 완전히 동일해야.
    let bt = sample_bt();
    let mut payload = [0u8; BOOK_TICKER_SIZE];
    encode_bookticker_into(&mut payload, &bt);

    let alloc_frame = create_message(MSG_BOOKTICKER, &payload).unwrap();
    let mut reuse = Vec::with_capacity(alloc_frame.len());
    write_frame_into(&mut reuse, MSG_BOOKTICKER, &payload).unwrap();

    assert_eq!(alloc_frame, reuse);
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. 토픽 빌더 ↔ 파서 역관계
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn topic_builder_parse_roundtrip_all_types() {
    let cases = [
        (ExchangeId::Gate, MSG_BOOKTICKER, "BTC_USDT"),
        (ExchangeId::Binance, MSG_TRADE, "ETH_USDT"),
        (ExchangeId::Gate, MSG_WEBBOOKTICKER, "SOL_USDT"),
        (ExchangeId::Bybit, MSG_TRADE, "1000PEPE_USDT"), // symbol 내부 `_` 다중
    ];
    for (ex, ty, sym) in cases {
        let topic = TopicBuilder::build(ex, ty, sym);
        let parsed = parse_topic(&topic).expect("parse");
        assert_eq!(parsed.exchange, ex, "exchange roundtrip");
        assert_eq!(parsed.msg_type, ty, "msg_type roundtrip");
        assert_eq!(parsed.symbol.as_str(), sym, "symbol roundtrip");
    }
}

#[test]
fn topic_from_market_event_matches_builder() {
    let ev = MarketEvent::BookTicker(sample_bt());
    let topic_a = TopicBuilder::from_event(&ev);
    let topic_b = TopicBuilder::bookticker(ExchangeId::Gate, "BTC_USDT");
    assert_eq!(topic_a, topic_b);

    let tev = MarketEvent::Trade(sample_trade());
    let topic_c = TopicBuilder::from_event(&tev);
    let topic_d = TopicBuilder::trade(ExchangeId::Binance, "ETH_USDT");
    assert_eq!(topic_c, topic_d);
}

#[test]
fn topic_prefix_matches_for_sub_filter() {
    // subscriber 는 `{exchange}_{msg_type}_` prefix 로 구독. 한 prefix 에 여러 symbol 이 매치.
    let prefix = TopicBuilder::prefix(ExchangeId::Gate, MSG_BOOKTICKER);
    let t1 = TopicBuilder::bookticker(ExchangeId::Gate, "BTC_USDT");
    let t2 = TopicBuilder::bookticker(ExchangeId::Gate, "ETH_USDT");
    assert!(t1.starts_with(&prefix), "{t1} should start with {prefix}");
    assert!(t2.starts_with(&prefix));

    // 다른 exchange / type 은 매치되면 안됨.
    let other = TopicBuilder::bookticker(ExchangeId::Binance, "BTC_USDT");
    assert!(!other.starts_with(&prefix));
    let trade_topic = TopicBuilder::trade(ExchangeId::Gate, "BTC_USDT");
    assert!(!trade_topic.starts_with(&prefix));
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. 경계 케이스 — 긴 심볼 잘림 / 길이 불일치 에러
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn bookticker_symbol_too_long_is_truncated_preserving_null_terminator() {
    // 32 byte 초과 symbol → 31 byte + null.
    let mut bt = sample_bt();
    bt.symbol = Symbol::new("A".repeat(100));

    let mut buf = [0u8; BOOK_TICKER_SIZE];
    encode_bookticker_into(&mut buf, &bt);

    // symbol 영역: offset 16..48 (32바이트). 마지막 바이트는 null.
    let symbol_region = &buf[16..48];
    assert_eq!(
        symbol_region[31], 0,
        "last byte of symbol field must be null terminator"
    );
    for i in 0..31 {
        assert_eq!(symbol_region[i], b'A', "truncated symbol byte {i}");
    }

    // decode 는 null 까지만 읽음.
    let wire = decode_bookticker(&buf).expect("decode ok");
    assert_eq!(wire.symbol.len(), 31);
    assert!(wire.symbol.chars().all(|c| c == 'A'));
}

#[test]
fn bookticker_wrong_length_is_rejected() {
    let short = [0u8; BOOK_TICKER_SIZE - 1];
    assert!(decode_bookticker(&short).is_err());

    let long = [0u8; BOOK_TICKER_SIZE + 1];
    assert!(decode_bookticker(&long).is_err());
}

#[test]
fn trade_wrong_length_is_rejected() {
    let short = [0u8; TRADE_SIZE - 1];
    assert!(decode_trade(&short).is_err());

    let long = [0u8; TRADE_SIZE + 1];
    assert!(decode_trade(&long).is_err());
}

// ─────────────────────────────────────────────────────────────────────────────
// 6. 파이프라인 2 단계 patch — publisher_sent_ms 를 patch 해도 wire 다른 필드에 영향 X
//    (aggregator 의 반복 patch 시나리오를 시뮬레이션)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn repeated_patch_is_idempotent_on_other_fields() {
    let bt = sample_bt();
    let mut buf = [0u8; BOOK_TICKER_SIZE];
    encode_bookticker_into(&mut buf, &bt);

    for pushed_ms in [100i64, 200, 300, 400, 500] {
        patch_bookticker_pushed_ms(&mut buf, pushed_ms);
        let wire = decode_bookticker(&buf).expect("decode");
        assert_eq!(wire.publisher_sent_ms, pushed_ms);
        // 다른 필드는 계속 초기값.
        assert_eq!(wire.bid_price, bt.bid_price.0);
        assert_eq!(wire.ask_price, bt.ask_price.0);
        assert_eq!(wire.event_time_ms, bt.event_time_ms);
    }
}

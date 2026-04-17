//! 바이너리 와이어 포맷 (C-struct 호환, little-endian).
//!
//! # 설계
//! 레거시 `data_publisher_rust/src/serializer.rs` 와 **byte-exact parity**.
//! 기존 C/C++/Python 소비자 (data_subscriber, strategy_flipster_python) 가
//! 그대로 붙을 수 있어야 한다. 한 바이트라도 어긋나면 기존 downstream 이 깨진다.
//!
//! # BookTicker (120 bytes)
//! ```text
//! offset  size  field
//!  0      16    exchange      (null-term ASCII, max 15)
//! 16      32    symbol        (null-term ASCII, max 31)
//! 48       8    bid_price     (f64 LE)
//! 56       8    ask_price     (f64 LE)
//! 64       8    bid_size      (f64 LE)
//! 72       8    ask_size      (f64 LE)
//! 80       8    event_time    (i64 LE, ms)
//! 88       8    server_time   (i64 LE, ms)
//! 96       8    publisher_sent_ms        (i64 LE, ms)  ← patch point
//! 104      8    subscriber_received_ms   (i64 LE, ms)
//! 112      8    subscriber_dump_ms       (i64 LE, ms)
//! ```
//!
//! # Trade (128 bytes)
//! ```text
//! offset  size  field
//!  0      16    exchange      (null-term ASCII, max 15)
//! 16      32    symbol        (null-term ASCII, max 31)
//! 48       8    price         (f64 LE)
//! 56       8    size          (f64 LE)
//! 64       8    trade_id      (i64 LE)
//! 72       8    create_time_s (i64 LE)
//! 80       8    create_time_ms(i64 LE)
//! 88       1    is_internal   (u8, 0/1)
//! 89       7    padding       (zero)
//! 96       8    server_time   (i64 LE, ms)
//! 104      8    publisher_sent_ms        (i64 LE, ms)  ← patch point
//! 112      8    subscriber_received_ms   (i64 LE, ms)
//! 120      8    subscriber_dump_ms       (i64 LE, ms)
//! ```
//!
//! # publisher_sent_ms patch 전략
//! publisher worker → aggregator → PUSH send 경로에서,
//! 직렬화(`encode_*_into`) 는 **publisher_sent_ms=0** 으로 내보내고,
//! aggregator 가 PUSH 직전 `patch_bookticker_pushed_ms` / `patch_trade_pushed_ms`
//! 로 그 8 바이트만 덮어쓴다 — 버퍼 전체 재직렬화 없음.
//!
//! # 경계
//! - exchange, symbol 은 최대 길이 초과 시 **잘라낸다**. null terminator 를
//!   끝에 남겨 두도록 `len.min(field_len - 1)` 로 복사.
//! - 해석 단계(`decode_*`) 에서 길이가 안 맞으면 `WireError::WrongLength`.
//! - 거래소 문자열이 등록된 것과 다르면 `WireError::UnknownExchange`.

use byteorder::{ByteOrder, LittleEndian};
use hft_types::{BookTicker, ExchangeId, Price, Size, Symbol, Trade};
use thiserror::Error;

// ─────────────────────────────────────────────────────────────────────────────
// 공개 크기 / offset 상수 — 다른 crate (aggregator, subscriber) 에서 사용
// ─────────────────────────────────────────────────────────────────────────────

/// BookTicker 고정 크기.
pub const BOOK_TICKER_SIZE: usize = 120;

/// Trade 고정 크기.
pub const TRADE_SIZE: usize = 128;

/// Exchange 필드 길이.
pub const EXCHANGE_FIELD_LEN: usize = 16;

/// Symbol 필드 길이.
pub const SYMBOL_FIELD_LEN: usize = 32;

/// BookTicker 내 `publisher_sent_ms` offset (aggregator patch point).
pub const BOOKTICKER_PUBLISHER_SENT_OFFSET: usize = 96;

/// Trade 내 `publisher_sent_ms` offset (aggregator patch point).
pub const TRADE_PUBLISHER_SENT_OFFSET: usize = 104;

// ─────────────────────────────────────────────────────────────────────────────
// 내부 offset 모듈 — read/write 가독성
// ─────────────────────────────────────────────────────────────────────────────

mod bt_off {
    pub const EXCHANGE: usize = 0;
    pub const SYMBOL: usize = 16;
    pub const BID_PRICE: usize = 48;
    pub const ASK_PRICE: usize = 56;
    pub const BID_SIZE: usize = 64;
    pub const ASK_SIZE: usize = 72;
    pub const EVENT_TIME: usize = 80;
    pub const SERVER_TIME: usize = 88;
    pub const PUBLISHER_SENT: usize = 96;
    pub const SUBSCRIBER_RECEIVED: usize = 104;
    pub const SUBSCRIBER_DUMP: usize = 112;
}

mod tr_off {
    pub const EXCHANGE: usize = 0;
    pub const SYMBOL: usize = 16;
    pub const PRICE: usize = 48;
    pub const SIZE: usize = 56;
    pub const TRADE_ID: usize = 64;
    pub const CREATE_TIME_S: usize = 72;
    pub const CREATE_TIME_MS: usize = 80;
    pub const IS_INTERNAL: usize = 88;
    // padding 89..96
    pub const SERVER_TIME: usize = 96;
    pub const PUBLISHER_SENT: usize = 104;
    pub const SUBSCRIBER_RECEIVED: usize = 112;
    pub const SUBSCRIBER_DUMP: usize = 120;
}

// ─────────────────────────────────────────────────────────────────────────────
// Error
// ─────────────────────────────────────────────────────────────────────────────

/// 와이어 포맷 파싱 에러.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    /// 버퍼 길이 불일치.
    #[error("wrong wire length: expected {expected}, got {actual}")]
    WrongLength {
        /// 기대 길이.
        expected: usize,
        /// 실제 길이.
        actual: usize,
    },

    /// 미등록 거래소.
    #[error("unknown exchange id: {0:?}")]
    UnknownExchange(String),

    /// UTF-8 변환 실패.
    #[error("non-utf8 bytes in {0}")]
    Utf8(&'static str),
}

// ─────────────────────────────────────────────────────────────────────────────
// cstr helpers — null-terminated 고정 길이 ASCII 필드
// ─────────────────────────────────────────────────────────────────────────────

/// `src` 의 바이트를 `dst` (고정 길이) 에 복사.
/// 최대 `dst.len() - 1` 까지만 복사해 마지막 1 바이트는 null 로 남긴다.
/// 호출자는 미리 `dst` 를 0 으로 초기화했다고 가정.
fn write_cstr(dst: &mut [u8], src: &str) {
    debug_assert!(!dst.is_empty(), "cstr field cannot be zero length");
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&bytes[..n]);
    // dst[n..] 은 호출자가 0 으로 초기화했으므로 그대로 둠 → null terminated.
}

/// null-terminated 필드 → `&str` 참조. UTF-8 검사 포함.
fn read_cstr<'a>(src: &'a [u8], field_name: &'static str) -> Result<&'a str, WireError> {
    let end = src.iter().position(|&b| b == 0).unwrap_or(src.len());
    std::str::from_utf8(&src[..end]).map_err(|_| WireError::Utf8(field_name))
}

// ─────────────────────────────────────────────────────────────────────────────
// BookTicker wire ↔ domain
// ─────────────────────────────────────────────────────────────────────────────

/// 와이어에서 디코드된 BookTicker (stamps 포함).
#[derive(Debug, Clone, PartialEq)]
pub struct BookTickerWire {
    /// 거래소.
    pub exchange: ExchangeId,
    /// 심볼 문자열.
    pub symbol: String,
    /// 매수 호가.
    pub bid_price: f64,
    /// 매도 호가.
    pub ask_price: f64,
    /// 매수 수량.
    pub bid_size: f64,
    /// 매도 수량.
    pub ask_size: f64,
    /// 이벤트 시각 (ms).
    pub event_time_ms: i64,
    /// 서버 시각 (ms).
    pub server_time_ms: i64,
    /// publisher PUSH 시각 (aggregator 가 patch).
    pub publisher_sent_ms: i64,
    /// subscriber recv 시각.
    pub subscriber_received_ms: i64,
    /// subscriber dump 시각.
    pub subscriber_dump_ms: i64,
}

impl BookTickerWire {
    /// 도메인 타입으로 변환. stamps 는 버림 (별도 `LatencyStamps` 에 기록되어야 함).
    pub fn into_domain(self) -> BookTicker {
        BookTicker {
            exchange: self.exchange,
            symbol: Symbol::from(self.symbol),
            bid_price: Price(self.bid_price),
            ask_price: Price(self.ask_price),
            bid_size: Size(self.bid_size),
            ask_size: Size(self.ask_size),
            event_time_ms: self.event_time_ms,
            server_time_ms: self.server_time_ms,
        }
    }
}

/// BookTicker 를 120-byte 버퍼에 직렬화. `publisher_sent_ms=0` 으로 채움.
///
/// 이 함수는 **단일 할당 없음** — 호출자가 재사용 가능한 버퍼를 준다.
/// aggregator 에서는 PUSH 직전 `patch_bookticker_pushed_ms` 호출.
pub fn encode_bookticker_into(buf: &mut [u8; BOOK_TICKER_SIZE], bt: &BookTicker) {
    // 버퍼를 0 으로 초기화 (이전 내용 덮어쓰기 + null terminator 보장)
    buf.fill(0);

    write_cstr(
        &mut buf[bt_off::EXCHANGE..bt_off::SYMBOL],
        bt.exchange.as_str(),
    );
    write_cstr(
        &mut buf[bt_off::SYMBOL..bt_off::BID_PRICE],
        bt.symbol.as_str(),
    );

    LittleEndian::write_f64(
        &mut buf[bt_off::BID_PRICE..bt_off::ASK_PRICE],
        bt.bid_price.0,
    );
    LittleEndian::write_f64(
        &mut buf[bt_off::ASK_PRICE..bt_off::BID_SIZE],
        bt.ask_price.0,
    );
    LittleEndian::write_f64(&mut buf[bt_off::BID_SIZE..bt_off::ASK_SIZE], bt.bid_size.0);
    LittleEndian::write_f64(
        &mut buf[bt_off::ASK_SIZE..bt_off::EVENT_TIME],
        bt.ask_size.0,
    );

    LittleEndian::write_i64(
        &mut buf[bt_off::EVENT_TIME..bt_off::SERVER_TIME],
        bt.event_time_ms,
    );
    LittleEndian::write_i64(
        &mut buf[bt_off::SERVER_TIME..bt_off::PUBLISHER_SENT],
        bt.server_time_ms,
    );

    // publisher_sent_ms / subscriber_received_ms / subscriber_dump_ms 는 0 유지.
}

/// 120-byte 버퍼를 `BookTickerWire` 로 디코드.
pub fn decode_bookticker(buf: &[u8]) -> Result<BookTickerWire, WireError> {
    if buf.len() != BOOK_TICKER_SIZE {
        return Err(WireError::WrongLength {
            expected: BOOK_TICKER_SIZE,
            actual: buf.len(),
        });
    }

    let exchange_str = read_cstr(&buf[bt_off::EXCHANGE..bt_off::SYMBOL], "exchange")?;
    let exchange = ExchangeId::parse(exchange_str)
        .ok_or_else(|| WireError::UnknownExchange(exchange_str.to_owned()))?;
    let symbol = read_cstr(&buf[bt_off::SYMBOL..bt_off::BID_PRICE], "symbol")?.to_owned();

    Ok(BookTickerWire {
        exchange,
        symbol,
        bid_price: LittleEndian::read_f64(&buf[bt_off::BID_PRICE..bt_off::ASK_PRICE]),
        ask_price: LittleEndian::read_f64(&buf[bt_off::ASK_PRICE..bt_off::BID_SIZE]),
        bid_size: LittleEndian::read_f64(&buf[bt_off::BID_SIZE..bt_off::ASK_SIZE]),
        ask_size: LittleEndian::read_f64(&buf[bt_off::ASK_SIZE..bt_off::EVENT_TIME]),
        event_time_ms: LittleEndian::read_i64(&buf[bt_off::EVENT_TIME..bt_off::SERVER_TIME]),
        server_time_ms: LittleEndian::read_i64(&buf[bt_off::SERVER_TIME..bt_off::PUBLISHER_SENT]),
        publisher_sent_ms: LittleEndian::read_i64(
            &buf[bt_off::PUBLISHER_SENT..bt_off::SUBSCRIBER_RECEIVED],
        ),
        subscriber_received_ms: LittleEndian::read_i64(
            &buf[bt_off::SUBSCRIBER_RECEIVED..bt_off::SUBSCRIBER_DUMP],
        ),
        subscriber_dump_ms: LittleEndian::read_i64(&buf[bt_off::SUBSCRIBER_DUMP..BOOK_TICKER_SIZE]),
    })
}

/// aggregator PUSH 직전에 publisher_sent_ms (offset 96) 8 바이트만 덮어쓴다.
///
/// 전체 재직렬화 없이 한 번의 LE write 만 수행 — 핫패스용.
#[inline]
pub fn patch_bookticker_pushed_ms(buf: &mut [u8], pushed_ms: i64) {
    debug_assert_eq!(buf.len(), BOOK_TICKER_SIZE, "bookticker buf size mismatch");
    LittleEndian::write_i64(
        &mut buf[bt_off::PUBLISHER_SENT..bt_off::SUBSCRIBER_RECEIVED],
        pushed_ms,
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Trade wire ↔ domain
// ─────────────────────────────────────────────────────────────────────────────

/// 와이어에서 디코드된 Trade (stamps 포함).
#[derive(Debug, Clone, PartialEq)]
pub struct TradeWire {
    /// 거래소.
    pub exchange: ExchangeId,
    /// 심볼.
    pub symbol: String,
    /// 가격.
    pub price: f64,
    /// 수량.
    pub size: f64,
    /// 체결 ID.
    pub trade_id: i64,
    /// 체결 시각 (초).
    pub create_time_s: i64,
    /// 체결 시각 (ms).
    pub create_time_ms: i64,
    /// 내부 매매 flag.
    pub is_internal: bool,
    /// 서버 시각.
    pub server_time_ms: i64,
    /// publisher 시각.
    pub publisher_sent_ms: i64,
    /// subscriber recv 시각.
    pub subscriber_received_ms: i64,
    /// subscriber dump 시각.
    pub subscriber_dump_ms: i64,
}

impl TradeWire {
    /// 도메인 타입으로 변환.
    pub fn into_domain(self) -> Trade {
        Trade {
            exchange: self.exchange,
            symbol: Symbol::from(self.symbol),
            price: Price(self.price),
            size: Size(self.size),
            trade_id: self.trade_id,
            create_time_s: self.create_time_s,
            create_time_ms: self.create_time_ms,
            server_time_ms: self.server_time_ms,
            is_internal: self.is_internal,
        }
    }
}

/// Trade 를 128-byte 버퍼에 직렬화. `publisher_sent_ms=0`.
pub fn encode_trade_into(buf: &mut [u8; TRADE_SIZE], t: &Trade) {
    buf.fill(0);

    write_cstr(
        &mut buf[tr_off::EXCHANGE..tr_off::SYMBOL],
        t.exchange.as_str(),
    );
    write_cstr(&mut buf[tr_off::SYMBOL..tr_off::PRICE], t.symbol.as_str());

    LittleEndian::write_f64(&mut buf[tr_off::PRICE..tr_off::SIZE], t.price.0);
    LittleEndian::write_f64(&mut buf[tr_off::SIZE..tr_off::TRADE_ID], t.size.0);

    LittleEndian::write_i64(
        &mut buf[tr_off::TRADE_ID..tr_off::CREATE_TIME_S],
        t.trade_id,
    );
    LittleEndian::write_i64(
        &mut buf[tr_off::CREATE_TIME_S..tr_off::CREATE_TIME_MS],
        t.create_time_s,
    );
    LittleEndian::write_i64(
        &mut buf[tr_off::CREATE_TIME_MS..tr_off::IS_INTERNAL],
        t.create_time_ms,
    );

    buf[tr_off::IS_INTERNAL] = u8::from(t.is_internal);
    // padding 89..96 은 fill(0) 로 이미 0.

    LittleEndian::write_i64(
        &mut buf[tr_off::SERVER_TIME..tr_off::PUBLISHER_SENT],
        t.server_time_ms,
    );
    // publisher_sent / subscriber_received / subscriber_dump = 0 유지.
}

/// 128-byte 버퍼를 `TradeWire` 로 디코드.
pub fn decode_trade(buf: &[u8]) -> Result<TradeWire, WireError> {
    if buf.len() != TRADE_SIZE {
        return Err(WireError::WrongLength {
            expected: TRADE_SIZE,
            actual: buf.len(),
        });
    }

    let exchange_str = read_cstr(&buf[tr_off::EXCHANGE..tr_off::SYMBOL], "exchange")?;
    let exchange = ExchangeId::parse(exchange_str)
        .ok_or_else(|| WireError::UnknownExchange(exchange_str.to_owned()))?;
    let symbol = read_cstr(&buf[tr_off::SYMBOL..tr_off::PRICE], "symbol")?.to_owned();

    Ok(TradeWire {
        exchange,
        symbol,
        price: LittleEndian::read_f64(&buf[tr_off::PRICE..tr_off::SIZE]),
        size: LittleEndian::read_f64(&buf[tr_off::SIZE..tr_off::TRADE_ID]),
        trade_id: LittleEndian::read_i64(&buf[tr_off::TRADE_ID..tr_off::CREATE_TIME_S]),
        create_time_s: LittleEndian::read_i64(&buf[tr_off::CREATE_TIME_S..tr_off::CREATE_TIME_MS]),
        create_time_ms: LittleEndian::read_i64(&buf[tr_off::CREATE_TIME_MS..tr_off::IS_INTERNAL]),
        is_internal: buf[tr_off::IS_INTERNAL] != 0,
        server_time_ms: LittleEndian::read_i64(&buf[tr_off::SERVER_TIME..tr_off::PUBLISHER_SENT]),
        publisher_sent_ms: LittleEndian::read_i64(
            &buf[tr_off::PUBLISHER_SENT..tr_off::SUBSCRIBER_RECEIVED],
        ),
        subscriber_received_ms: LittleEndian::read_i64(
            &buf[tr_off::SUBSCRIBER_RECEIVED..tr_off::SUBSCRIBER_DUMP],
        ),
        subscriber_dump_ms: LittleEndian::read_i64(&buf[tr_off::SUBSCRIBER_DUMP..TRADE_SIZE]),
    })
}

/// aggregator PUSH 직전 Trade publisher_sent_ms (offset 104) 8 바이트 덮어쓰기.
#[inline]
pub fn patch_trade_pushed_ms(buf: &mut [u8], pushed_ms: i64) {
    debug_assert_eq!(buf.len(), TRADE_SIZE, "trade buf size mismatch");
    LittleEndian::write_i64(
        &mut buf[tr_off::PUBLISHER_SENT..tr_off::SUBSCRIBER_RECEIVED],
        pushed_ms,
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 테스트
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_types::{BookTicker, ExchangeId, Price, Size, Symbol, Trade};

    fn sample_bt() -> BookTicker {
        BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(29_950.25),
            ask_price: Price(29_951.75),
            bid_size: Size(12.5),
            ask_size: Size(8.75),
            event_time_ms: 1_700_000_000_123,
            server_time_ms: 1_700_000_000_100,
        }
    }

    fn sample_trade() -> Trade {
        Trade {
            exchange: ExchangeId::Binance,
            symbol: Symbol::new("ETH_USDT"),
            price: Price(1_800.5),
            size: Size(-0.25), // sell
            trade_id: 987_654_321,
            create_time_s: 1_700_000_000,
            create_time_ms: 1_700_000_000_321,
            server_time_ms: 1_700_000_000_400,
            is_internal: true,
        }
    }

    // --- BookTicker ----------------------------------------------------------

    #[test]
    fn bookticker_roundtrip() {
        let bt = sample_bt();
        let mut buf = [0u8; BOOK_TICKER_SIZE];
        encode_bookticker_into(&mut buf, &bt);
        let wire = decode_bookticker(&buf).expect("decode");
        assert_eq!(wire.exchange, bt.exchange);
        assert_eq!(wire.symbol, bt.symbol.as_str());
        assert_eq!(wire.bid_price, bt.bid_price.0);
        assert_eq!(wire.ask_price, bt.ask_price.0);
        assert_eq!(wire.bid_size, bt.bid_size.0);
        assert_eq!(wire.ask_size, bt.ask_size.0);
        assert_eq!(wire.event_time_ms, bt.event_time_ms);
        assert_eq!(wire.server_time_ms, bt.server_time_ms);
        // encode 직후에는 publisher_sent_ms=0
        assert_eq!(wire.publisher_sent_ms, 0);
        assert_eq!(wire.subscriber_received_ms, 0);
        assert_eq!(wire.subscriber_dump_ms, 0);

        // into_domain 도 값 보존
        let back = wire.into_domain();
        assert_eq!(back, bt);
    }

    #[test]
    fn bookticker_publisher_sent_is_zero_after_encode() {
        let mut buf = [0u8; BOOK_TICKER_SIZE];
        encode_bookticker_into(&mut buf, &sample_bt());
        assert_eq!(
            LittleEndian::read_i64(
                &buf[BOOKTICKER_PUBLISHER_SENT_OFFSET..BOOKTICKER_PUBLISHER_SENT_OFFSET + 8]
            ),
            0
        );
    }

    #[test]
    fn bookticker_patch_pushed_ms_only_touches_offset() {
        let mut buf = [0u8; BOOK_TICKER_SIZE];
        encode_bookticker_into(&mut buf, &sample_bt());
        let before = buf;

        patch_bookticker_pushed_ms(&mut buf, 1_700_000_000_500);

        // offset 96..104 는 변경됨
        let patched = LittleEndian::read_i64(
            &buf[BOOKTICKER_PUBLISHER_SENT_OFFSET..BOOKTICKER_PUBLISHER_SENT_OFFSET + 8],
        );
        assert_eq!(patched, 1_700_000_000_500);

        // 나머지는 동일
        assert_eq!(
            before[..BOOKTICKER_PUBLISHER_SENT_OFFSET],
            buf[..BOOKTICKER_PUBLISHER_SENT_OFFSET]
        );
        assert_eq!(
            before[BOOKTICKER_PUBLISHER_SENT_OFFSET + 8..],
            buf[BOOKTICKER_PUBLISHER_SENT_OFFSET + 8..]
        );
    }

    #[test]
    fn bookticker_wrong_length_rejected() {
        let short = [0u8; 100];
        assert!(matches!(
            decode_bookticker(&short),
            Err(WireError::WrongLength {
                expected: 120,
                actual: 100
            })
        ));
    }

    #[test]
    fn bookticker_unknown_exchange_rejected() {
        let mut buf = [0u8; BOOK_TICKER_SIZE];
        encode_bookticker_into(&mut buf, &sample_bt());
        // exchange 자리에 "bogus" 박기
        buf[..16].fill(0);
        buf[..5].copy_from_slice(b"bogus");
        let err = decode_bookticker(&buf).expect_err("should fail");
        match err {
            WireError::UnknownExchange(s) => assert_eq!(s, "bogus"),
            other => panic!("expected UnknownExchange, got {other:?}"),
        }
    }

    #[test]
    fn symbol_longer_than_31_is_truncated_without_panic() {
        let bt = BookTicker {
            symbol: Symbol::new("A".repeat(100)),
            ..sample_bt()
        };
        let mut buf = [0u8; BOOK_TICKER_SIZE];
        encode_bookticker_into(&mut buf, &bt);
        // 마지막 바이트는 null terminator 로 유지되어야 함.
        assert_eq!(buf[bt_off::SYMBOL + SYMBOL_FIELD_LEN - 1], 0);
        // 첫 31자리는 'A' 로 채워짐
        for i in 0..31 {
            assert_eq!(buf[bt_off::SYMBOL + i], b'A');
        }
    }

    // --- Trade ---------------------------------------------------------------

    #[test]
    fn trade_roundtrip() {
        let t = sample_trade();
        let mut buf = [0u8; TRADE_SIZE];
        encode_trade_into(&mut buf, &t);
        let wire = decode_trade(&buf).expect("decode");
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

    #[test]
    fn trade_patch_pushed_ms_only_touches_offset() {
        let mut buf = [0u8; TRADE_SIZE];
        encode_trade_into(&mut buf, &sample_trade());
        let before = buf;

        patch_trade_pushed_ms(&mut buf, 1_700_000_000_999);

        let patched = LittleEndian::read_i64(
            &buf[TRADE_PUBLISHER_SENT_OFFSET..TRADE_PUBLISHER_SENT_OFFSET + 8],
        );
        assert_eq!(patched, 1_700_000_000_999);

        assert_eq!(
            before[..TRADE_PUBLISHER_SENT_OFFSET],
            buf[..TRADE_PUBLISHER_SENT_OFFSET]
        );
        assert_eq!(
            before[TRADE_PUBLISHER_SENT_OFFSET + 8..],
            buf[TRADE_PUBLISHER_SENT_OFFSET + 8..]
        );
    }

    #[test]
    fn trade_is_internal_false_variant() {
        let t = Trade {
            is_internal: false,
            ..sample_trade()
        };
        let mut buf = [0u8; TRADE_SIZE];
        encode_trade_into(&mut buf, &t);
        assert_eq!(buf[tr_off::IS_INTERNAL], 0);
        // padding 도 0 이어야 함
        for (i, byte) in buf
            .iter()
            .enumerate()
            .take(tr_off::SERVER_TIME)
            .skip(tr_off::IS_INTERNAL + 1)
        {
            assert_eq!(*byte, 0, "padding byte {i} not zero");
        }
        let wire = decode_trade(&buf).unwrap();
        assert!(!wire.is_internal);
    }

    // --- 레거시 byte parity --------------------------------------------------
    //
    // 레거시 `data_publisher_rust/src/serializer.rs` 와 바이트 단위로 동일해야 함.
    // 여기서 레거시 로직을 인라인 재현해 비교한다.
    // 단, 레거시는 publisher_sent_ms 를 즉시 채우고 우리는 0 으로 두므로
    // 그 8 바이트만 별도 비교에서 제외.

    fn legacy_bookticker_bytes(exchange: &str, bt: &BookTicker) -> Vec<u8> {
        let mut buf = vec![0u8; 120];
        let eb = exchange.as_bytes();
        let n = eb.len().min(15);
        buf[..n].copy_from_slice(&eb[..n]);

        let sb = bt.symbol.as_str().as_bytes();
        let n = sb.len().min(31);
        buf[16..16 + n].copy_from_slice(&sb[..n]);

        LittleEndian::write_f64(&mut buf[48..56], bt.bid_price.0);
        LittleEndian::write_f64(&mut buf[56..64], bt.ask_price.0);
        LittleEndian::write_f64(&mut buf[64..72], bt.bid_size.0);
        LittleEndian::write_f64(&mut buf[72..80], bt.ask_size.0);
        LittleEndian::write_i64(&mut buf[80..88], bt.event_time_ms);
        LittleEndian::write_i64(&mut buf[88..96], bt.server_time_ms);
        // 레거시는 여기서 SystemTime::now() 를 찍지만, parity 비교에서는 0 으로 둔다
        // (테스트는 해당 8바이트를 비교에서 제외).
        LittleEndian::write_i64(&mut buf[96..104], 0);
        LittleEndian::write_i64(&mut buf[104..112], 0);
        LittleEndian::write_i64(&mut buf[112..120], 0);
        buf
    }

    fn legacy_trade_bytes(exchange: &str, t: &Trade) -> Vec<u8> {
        let mut buf = vec![0u8; 128];
        let eb = exchange.as_bytes();
        let n = eb.len().min(15);
        buf[..n].copy_from_slice(&eb[..n]);

        let sb = t.symbol.as_str().as_bytes();
        let n = sb.len().min(31);
        buf[16..16 + n].copy_from_slice(&sb[..n]);

        LittleEndian::write_f64(&mut buf[48..56], t.price.0);
        LittleEndian::write_f64(&mut buf[56..64], t.size.0);
        LittleEndian::write_i64(&mut buf[64..72], t.trade_id);
        LittleEndian::write_i64(&mut buf[72..80], t.create_time_s);
        LittleEndian::write_i64(&mut buf[80..88], t.create_time_ms);
        buf[88] = u8::from(t.is_internal);
        // padding 89..96 = 0
        LittleEndian::write_i64(&mut buf[96..104], t.server_time_ms);
        LittleEndian::write_i64(&mut buf[104..112], 0);
        LittleEndian::write_i64(&mut buf[112..120], 0);
        LittleEndian::write_i64(&mut buf[120..128], 0);
        buf
    }

    #[test]
    fn legacy_byte_parity_bookticker() {
        let bt = sample_bt();
        let legacy = legacy_bookticker_bytes(bt.exchange.as_str(), &bt);

        let mut ours = [0u8; BOOK_TICKER_SIZE];
        encode_bookticker_into(&mut ours, &bt);

        assert_eq!(
            &legacy[..],
            &ours[..],
            "byte-exact parity with legacy encoder"
        );
    }

    #[test]
    fn legacy_byte_parity_trade() {
        let t = sample_trade();
        let legacy = legacy_trade_bytes(t.exchange.as_str(), &t);

        let mut ours = [0u8; TRADE_SIZE];
        encode_trade_into(&mut ours, &t);

        assert_eq!(
            &legacy[..],
            &ours[..],
            "byte-exact parity with legacy trade encoder"
        );
    }
}

//! hft-types — 도메인 타입
//!
//! HFT 파이프라인에서 쓰는 Rust-idiomatic 타입들.
//! 와이어 포맷(C-struct 120B/128B)은 `hft-protocol` 에 분리.
//!
//! ## 설계 원칙
//! - `Symbol` 은 `Arc<str>` → clone 비용 포인터 1개.
//! - 가격/사이즈는 `f64` newtype 래핑해 단위 혼동 제거.
//! - hot path 에서 쓰이는 메시지 구조체는 `#[repr(C)]` 고정 (패딩 예측 가능).
//! - `ExchangeId` 는 enum → 문자열 비교/대소문자 이슈 제거.

#![deny(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

pub mod consts {
    //! 전역 매직 넘버 (`check.md §0` 대응).
    //!
    //! 0 으로 나누기 방지용 epsilon.
    pub const EPS: f64 = 1e-12;
}

// ─────────────────────────────────────────────────────────────────────────────
// ExchangeId
// ─────────────────────────────────────────────────────────────────────────────

/// 거래소 식별자. 문자열 literal 보다 enum 으로 관리해 오탈자를 컴파일 타임에 차단.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExchangeId {
    /// Binance Futures.
    Binance,
    /// Gate.io Futures (API + Web 둘 다 이 식별자를 사용, 구분은 `DataRole`).
    Gate,
    /// Bybit Linear.
    Bybit,
    /// Bitget USDT-Margined.
    Bitget,
    /// OKX Perpetual Swap.
    Okx,
}

impl ExchangeId {
    /// 모든 variant 의 컴파일 타임 리스트 (publisher/tool 이 전체 순회할 때 사용).
    pub const ALL: [Self; 5] = [
        Self::Binance,
        Self::Gate,
        Self::Bybit,
        Self::Bitget,
        Self::Okx,
    ];

    /// 와이어 포맷·토픽에서 쓰는 소문자 표기.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Gate => "gate",
            Self::Bybit => "bybit",
            Self::Bitget => "bitget",
            Self::Okx => "okx",
        }
    }

    /// 문자열에서 parse. 대소문자 무시.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "binance" => Some(Self::Binance),
            "gate" | "gateio" | "gate.io" => Some(Self::Gate),
            "bybit" => Some(Self::Bybit),
            "bitget" => Some(Self::Bitget),
            "okx" => Some(Self::Okx),
            _ => None,
        }
    }
}

impl fmt::Display for ExchangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Symbol — Arc<str> 로 clone 비용 최소화
// ─────────────────────────────────────────────────────────────────────────────

/// 거래 심볼 (예: `BTC_USDT`).
///
/// 내부적으로 `Arc<str>` 이므로 `clone()` 은 atomic refcount 1회 증가뿐.
/// 핫 패스에서 여러 소비자에게 복사할 때 힙 할당이 발생하지 않음.
#[derive(Clone, Serialize, Deserialize)]
pub struct Symbol(Arc<str>);

impl Symbol {
    /// 문자열 slice 에서 Symbol 생성. 1회 힙 할당.
    pub fn new(s: impl AsRef<str>) -> Self {
        Self(Arc::from(s.as_ref()))
    }

    /// 내부 문자열 참조.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 길이 (bytes).
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 비어있는지.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 두 Symbol 이 동일한 `Arc<str>` 인스턴스를 공유하는지 검사 —
    /// 내부 identity 비교이며, intern 한 caching 이 적용됐는지 확인할 때 사용.
    /// 문자열 값이 같아도 alloc 이 서로 다르면 `false` 가 될 수 있으므로,
    /// **값 비교** 는 `as_str() == as_str()` 또는 `PartialEq` 를 쓸 것.
    #[inline]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Symbol").field(&self.as_str()).finish()
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq for Symbol {
    fn eq(&self, other: &Self) -> bool {
        // Arc<str> 포인터 동일하면 바로 true → 캐시된 symbol 비교 O(1)
        Arc::ptr_eq(&self.0, &other.0) || self.as_str() == other.as_str()
    }
}
impl Eq for Symbol {}

impl std::hash::Hash for Symbol {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl From<&str> for Symbol {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for Symbol {
    fn from(s: String) -> Self {
        Self(Arc::from(s.as_str()))
    }
}

impl AsRef<str> for Symbol {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Price / Size — 단위 혼동 방지 newtype
// ─────────────────────────────────────────────────────────────────────────────

/// 가격 (f64). 대입/비교 의도를 타입으로 명시.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(transparent)]
pub struct Price(pub f64);

impl Price {
    /// 0.
    pub const ZERO: Self = Self(0.0);

    /// 내부 f64 반환.
    #[inline]
    pub const fn get(self) -> f64 {
        self.0
    }

    /// 유효한 가격인지 (finite + positive).
    #[inline]
    pub fn is_valid(self) -> bool {
        self.0.is_finite() && self.0 > 0.0
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// 사이즈 (계약수 또는 USDT 수량). 거래소마다 단위 다르므로 `PriceKind` 은 도입 보류.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(transparent)]
pub struct Size(pub f64);

impl Size {
    /// 0.
    pub const ZERO: Self = Self(0.0);

    /// 내부 f64 반환.
    #[inline]
    pub const fn get(self) -> f64 {
        self.0
    }

    /// 부호 제거 절대값.
    #[inline]
    pub fn abs(self) -> Self {
        Self(self.0.abs())
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BookTicker / Trade — in-memory 도메인 타입
// ─────────────────────────────────────────────────────────────────────────────

/// 실시간 best bid/ask 스냅샷.
///
/// `hft-protocol::BookTickerWire` (120B) 와 구별 — 이 타입은 비즈니스 로직용.
#[derive(Debug, Clone, PartialEq)]
#[repr(C)]
pub struct BookTicker {
    /// 거래소.
    pub exchange: ExchangeId,
    /// 심볼.
    pub symbol: Symbol,
    /// 최우선 매수 호가.
    pub bid_price: Price,
    /// 최우선 매도 호가.
    pub ask_price: Price,
    /// 매수 수량.
    pub bid_size: Size,
    /// 매도 수량.
    pub ask_size: Size,
    /// 거래소가 내부에서 이벤트 발생 시각 (ms, epoch).
    pub event_time_ms: i64,
    /// 거래소 서버 송신 시각 (ms, epoch). `check.md` latency budget 의 출발점.
    pub server_time_ms: i64,
}

impl BookTicker {
    /// 유효성 검사 — bid > 0, ask > 0, ask ≥ bid.
    pub fn is_valid(&self) -> bool {
        self.bid_price.is_valid()
            && self.ask_price.is_valid()
            && self.ask_price.0 >= self.bid_price.0
    }

    /// 중간가 ((bid + ask) / 2). 유효성 사전 확인 필수.
    #[inline]
    pub fn mid(&self) -> f64 {
        (self.bid_price.0 + self.ask_price.0) * 0.5
    }

    /// bp (basis point) 단위 spread = `(ask - bid) / mid * 10_000`.
    /// mid 가 0 에 가까우면 `f64::INFINITY` 반환.
    pub fn spread_bp(&self) -> f64 {
        let mid = self.mid();
        if mid.abs() < consts::EPS {
            return f64::INFINITY;
        }
        (self.ask_price.0 - self.bid_price.0) / mid * 10_000.0
    }
}

/// 체결 내역 스냅샷.
#[derive(Debug, Clone, PartialEq)]
#[repr(C)]
pub struct Trade {
    /// 거래소.
    pub exchange: ExchangeId,
    /// 심볼.
    pub symbol: Symbol,
    /// 체결 가격.
    pub price: Price,
    /// 체결 수량. 부호 있음 (buy: +, sell: -) — 거래소 규약에 따름.
    pub size: Size,
    /// 체결 고유 ID.
    pub trade_id: i64,
    /// 체결 시각 (초).
    pub create_time_s: i64,
    /// 체결 시각 (ms).
    pub create_time_ms: i64,
    /// 거래소 서버 송신 시각 (ms, epoch).
    pub server_time_ms: i64,
    /// 자체 매매 / 외부 매매 구분. Gate 의 내부 필드.
    pub is_internal: bool,
}

impl Trade {
    /// 체결 방향 (size 부호 기반).
    #[inline]
    pub fn side(&self) -> Side {
        if self.size.0 >= 0.0 {
            Side::Buy
        } else {
            Side::Sell
        }
    }
}

/// 매매 방향.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Buy / Long / Bid.
    Buy,
    /// Sell / Short / Ask.
    Sell,
}

impl Side {
    /// 반대 방향.
    #[inline]
    pub fn flip(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }

    /// 와이어 문자열.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MarketEvent
// ─────────────────────────────────────────────────────────────────────────────

/// 거래소 `ExchangeFeed` 가 방출하는 이벤트.
#[derive(Debug, Clone, PartialEq)]
pub enum MarketEvent {
    /// 일반 bookTicker (Binance, Gate API, Bybit, Bitget, OKX).
    BookTicker(BookTicker),
    /// 체결 내역.
    Trade(Trade),
    /// Gate 전용 `futures.order_book` 에서 뽑아낸 best bid/ask.
    ///
    /// 데이터 형태는 BookTicker 와 동일하지만 소스가 다르다. 토픽도 `webbookticker`.
    WebBookTicker(BookTicker),
}

impl MarketEvent {
    /// 이벤트에 포함된 거래소.
    pub fn exchange(&self) -> ExchangeId {
        match self {
            Self::BookTicker(b) | Self::WebBookTicker(b) => b.exchange,
            Self::Trade(t) => t.exchange,
        }
    }

    /// 이벤트 심볼 (저렴한 Arc clone).
    pub fn symbol(&self) -> Symbol {
        match self {
            Self::BookTicker(b) | Self::WebBookTicker(b) => b.symbol.clone(),
            Self::Trade(t) => t.symbol.clone(),
        }
    }

    /// 서버 시각 (latency 계산 기준점).
    pub fn server_time_ms(&self) -> i64 {
        match self {
            Self::BookTicker(b) | Self::WebBookTicker(b) => b.server_time_ms,
            Self::Trade(t) => t.server_time_ms,
        }
    }

    /// 이벤트 종류 와이어 문자열 (토픽용).
    pub const fn type_str(&self) -> &'static str {
        match self {
            Self::BookTicker(_) => "bookticker",
            Self::WebBookTicker(_) => "webbookticker",
            Self::Trade(_) => "trade",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DataRole — 같은 거래소에서 채널 구분
// ─────────────────────────────────────────────────────────────────────────────

/// 같은 거래소 안의 "데이터 소스 역할".
///
/// 예: Gate API bookticker = Primary, Gate Web orderbook = WebOrderBook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataRole {
    /// 기본 공식 채널 (bookTicker / trades).
    Primary,
    /// Gate 의 web orderbook 같은 보조 채널.
    WebOrderBook,
}

// ─────────────────────────────────────────────────────────────────────────────
// 테스트
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_id_roundtrip() {
        for e in ExchangeId::ALL {
            assert_eq!(ExchangeId::parse(e.as_str()), Some(e));
        }
        assert_eq!(ExchangeId::parse("BINANCE"), Some(ExchangeId::Binance));
        assert_eq!(ExchangeId::parse("gate.io"), Some(ExchangeId::Gate));
        assert_eq!(ExchangeId::parse("unknown"), None);
    }

    #[test]
    fn symbol_clone_is_cheap() {
        let s = Symbol::new("BTC_USDT");
        let s2 = s.clone();
        assert_eq!(s, s2);
        // Arc pointer equality
        assert!(Arc::ptr_eq(&s.0, &s2.0));
        assert_eq!(s.as_str(), "BTC_USDT");
    }

    #[test]
    fn symbol_equality_works_across_independent_allocs() {
        let a = Symbol::new("BTC_USDT");
        let b = Symbol::new(String::from("BTC_USDT"));
        assert_eq!(a, b);
    }

    #[test]
    fn price_validity() {
        assert!(Price(100.0).is_valid());
        assert!(!Price(0.0).is_valid());
        assert!(!Price(-1.0).is_valid());
        assert!(!Price(f64::NAN).is_valid());
        assert!(!Price(f64::INFINITY).is_valid());
    }

    #[test]
    fn bookticker_mid_and_spread() {
        let b = BookTicker {
            exchange: ExchangeId::Gate,
            symbol: "BTC_USDT".into(),
            bid_price: Price(100.0),
            ask_price: Price(100.1),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: 0,
            server_time_ms: 0,
        };
        assert!(b.is_valid());
        assert!((b.mid() - 100.05).abs() < 1e-9);
        // spread bp = 0.1 / 100.05 * 10_000 ≈ 9.995
        assert!((b.spread_bp() - 9.995).abs() < 0.01);
    }

    #[test]
    fn bookticker_invalid_when_ask_below_bid() {
        let b = BookTicker {
            exchange: ExchangeId::Gate,
            symbol: "BTC_USDT".into(),
            bid_price: Price(100.0),
            ask_price: Price(99.0),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: 0,
            server_time_ms: 0,
        };
        assert!(!b.is_valid());
    }

    #[test]
    fn trade_side_sign() {
        let buy = Trade {
            exchange: ExchangeId::Gate,
            symbol: "BTC_USDT".into(),
            price: Price(100.0),
            size: Size(1.0),
            trade_id: 1,
            create_time_s: 0,
            create_time_ms: 0,
            server_time_ms: 0,
            is_internal: false,
        };
        assert_eq!(buy.side(), Side::Buy);

        let sell = Trade {
            size: Size(-1.0),
            ..buy.clone()
        };
        assert_eq!(sell.side(), Side::Sell);
    }

    #[test]
    fn market_event_accessors() {
        let bt = BookTicker {
            exchange: ExchangeId::Binance,
            symbol: "ETH_USDT".into(),
            bid_price: Price(1.0),
            ask_price: Price(1.1),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: 0,
            server_time_ms: 12345,
        };
        let ev = MarketEvent::BookTicker(bt);
        assert_eq!(ev.exchange(), ExchangeId::Binance);
        assert_eq!(ev.symbol().as_str(), "ETH_USDT");
        assert_eq!(ev.server_time_ms(), 12345);
        assert_eq!(ev.type_str(), "bookticker");
    }

    #[test]
    fn side_flip() {
        assert_eq!(Side::Buy.flip(), Side::Sell);
        assert_eq!(Side::Sell.flip(), Side::Buy);
    }
}

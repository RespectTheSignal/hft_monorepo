//! ZMQ 토픽 네이밍 규약.
//!
//! # 형식
//! `{exchange}_{msg_type}_{symbol}`
//!
//! 예:
//! - `gate_bookticker_BTC_USDT`
//! - `binance_trade_ETH_USDT`
//! - `gate_webbookticker_SOL_USDT`
//!
//! Symbol 자체에 `_` 가 포함되므로 파싱은 **prefix match** 기반.
//! 구독자는 `{exchange}_{msg_type}_` prefix 로만 SUB 해 여러 심볼을 동시에 받는다.

use hft_types::{ExchangeId, MarketEvent, Symbol};

// ─────────────────────────────────────────────────────────────────────────────
// 메시지 타입 상수 — hft-types::MarketEvent::type_str() 와 일치해야 함
// ─────────────────────────────────────────────────────────────────────────────

/// bookTicker 토픽 타입.
pub const MSG_BOOKTICKER: &str = "bookticker";
/// Trade 토픽 타입.
pub const MSG_TRADE: &str = "trade";
/// Gate 전용 Web orderbook bookTicker 타입.
pub const MSG_WEBBOOKTICKER: &str = "webbookticker";

/// 토픽 구분자.
pub const TOPIC_SEP: char = '_';

// ─────────────────────────────────────────────────────────────────────────────
// Builder
// ─────────────────────────────────────────────────────────────────────────────

/// 토픽 문자열 생성 유틸.
pub struct TopicBuilder;

impl TopicBuilder {
    /// `{exchange}_{msg_type}_{symbol}` 형식의 토픽.
    pub fn build(exchange: ExchangeId, msg_type: &str, symbol: &str) -> String {
        let mut s =
            String::with_capacity(exchange.as_str().len() + msg_type.len() + symbol.len() + 2);
        s.push_str(exchange.as_str());
        s.push(TOPIC_SEP);
        s.push_str(msg_type);
        s.push(TOPIC_SEP);
        s.push_str(symbol);
        s
    }

    /// BookTicker 토픽.
    pub fn bookticker(exchange: ExchangeId, symbol: &str) -> String {
        Self::build(exchange, MSG_BOOKTICKER, symbol)
    }

    /// Trade 토픽.
    pub fn trade(exchange: ExchangeId, symbol: &str) -> String {
        Self::build(exchange, MSG_TRADE, symbol)
    }

    /// Gate Web orderbook 토픽.
    pub fn webbookticker(exchange: ExchangeId, symbol: &str) -> String {
        Self::build(exchange, MSG_WEBBOOKTICKER, symbol)
    }

    /// `MarketEvent` 로부터 토픽 자동 생성 — publisher aggregator 에서 유용.
    pub fn from_event(ev: &MarketEvent) -> String {
        Self::build(ev.exchange(), ev.type_str(), ev.symbol().as_str())
    }

    /// 특정 exchange + msg_type 의 모든 심볼을 SUB 하기 위한 prefix.
    /// 끝에 `_` 포함 → 심볼 앞의 구분자까지 매치.
    pub fn prefix(exchange: ExchangeId, msg_type: &str) -> String {
        let mut s = String::with_capacity(exchange.as_str().len() + msg_type.len() + 2);
        s.push_str(exchange.as_str());
        s.push(TOPIC_SEP);
        s.push_str(msg_type);
        s.push(TOPIC_SEP);
        s
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 파싱 (subscriber 가 받은 토픽을 분석)
// ─────────────────────────────────────────────────────────────────────────────

/// 파싱된 토픽 구성요소.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTopic {
    /// 거래소.
    pub exchange: ExchangeId,
    /// 메시지 타입 (`bookticker` / `trade` / `webbookticker`).
    pub msg_type: String,
    /// 심볼 (Arc<str>).
    pub symbol: Symbol,
}

/// 토픽을 `{exchange}_{msg_type}_{symbol}` 로 파싱.
///
/// symbol 은 `_` 를 포함할 수 있으므로 앞쪽 두 segment 만 split 하고
/// 나머지 전체를 symbol 로 취급한다.
pub fn parse_topic(topic: &str) -> Option<ParsedTopic> {
    // exchange 분리
    let (exchange_str, rest) = topic.split_once(TOPIC_SEP)?;
    // msg_type 분리
    let (msg_type, symbol_str) = rest.split_once(TOPIC_SEP)?;

    if symbol_str.is_empty() {
        return None;
    }

    let exchange = ExchangeId::parse(exchange_str)?;
    Some(ParsedTopic {
        exchange,
        msg_type: msg_type.to_owned(),
        symbol: Symbol::new(symbol_str),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// 테스트
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_types::{BookTicker, ExchangeId, Price, Size};

    #[test]
    fn builder_formats() {
        assert_eq!(
            TopicBuilder::bookticker(ExchangeId::Gate, "BTC_USDT"),
            "gate_bookticker_BTC_USDT"
        );
        assert_eq!(
            TopicBuilder::trade(ExchangeId::Binance, "ETH_USDT"),
            "binance_trade_ETH_USDT"
        );
        assert_eq!(
            TopicBuilder::webbookticker(ExchangeId::Gate, "SOL_USDT"),
            "gate_webbookticker_SOL_USDT"
        );
    }

    #[test]
    fn prefix_ends_with_sep() {
        assert_eq!(
            TopicBuilder::prefix(ExchangeId::Gate, MSG_BOOKTICKER),
            "gate_bookticker_"
        );
    }

    #[test]
    fn parse_roundtrip() {
        let t = TopicBuilder::bookticker(ExchangeId::Gate, "BTC_USDT");
        let p = parse_topic(&t).expect("parse");
        assert_eq!(p.exchange, ExchangeId::Gate);
        assert_eq!(p.msg_type, "bookticker");
        assert_eq!(p.symbol.as_str(), "BTC_USDT");
    }

    #[test]
    fn parse_preserves_underscores_in_symbol() {
        // 가상의 3-segment symbol.
        let t = "bybit_trade_1000PEPE_USDT";
        let p = parse_topic(t).unwrap();
        assert_eq!(p.exchange, ExchangeId::Bybit);
        assert_eq!(p.msg_type, "trade");
        assert_eq!(p.symbol.as_str(), "1000PEPE_USDT");
    }

    #[test]
    fn parse_rejects_missing_symbol() {
        assert!(parse_topic("gate_bookticker_").is_none());
        assert!(parse_topic("gate_bookticker").is_none());
        assert!(parse_topic("").is_none());
    }

    #[test]
    fn parse_rejects_unknown_exchange() {
        assert!(parse_topic("nope_bookticker_X").is_none());
    }

    #[test]
    fn from_event_matches_builder() {
        let bt = BookTicker {
            exchange: ExchangeId::Okx,
            symbol: "DOGE_USDT".into(),
            bid_price: Price(0.1),
            ask_price: Price(0.11),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: 0,
            server_time_ms: 0,
        };
        let ev = hft_types::MarketEvent::BookTicker(bt);
        let topic = TopicBuilder::from_event(&ev);
        assert_eq!(topic, "okx_bookticker_DOGE_USDT");
    }
}

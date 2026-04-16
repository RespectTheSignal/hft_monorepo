//! 주문 도메인 타입 → transport wire 어댑터.
//!
//! Phase 2 E 에서는 전략이 생성한 [`hft_exchange_api::OrderRequest`] 를
//! 두 경로로 나눠 보낸다.
//!
//! - SHM 정상 경로: [`hft_shm::OrderFrame`]
//! - ZMQ fallback 경로: [`crate::order_wire::OrderRequestWire`]
//!
//! 이 모듈은 두 변환에서 공통으로 필요한 메타데이터, 에러 타입, 변환 함수를 제공한다.

use hft_exchange_api::{OrderRequest, OrderSide, OrderType, TimeInForce};
use hft_shm::{
    exchange_to_u8, OrderFrame, OrderKind, PlaceAuxMeta, PLACE_LEVEL_CLOSE, PLACE_LEVEL_OPEN,
};
use thiserror::Error;

use crate::order_wire::{
    OrderRequestWire, WireError, FLAG_REDUCE_ONLY, LEVEL_CLOSE, LEVEL_OPEN, ORDER_TYPE_LIMIT,
    ORDER_TYPE_MARKET, SIDE_BUY, SIDE_SELL, TIF_FOK, TIF_GTC, TIF_IOC,
};

/// ZMQ wire 에 실리는 open/close 레벨.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireLevel {
    /// 신규 포지션 open 주문.
    Open,
    /// 기존 포지션 close 주문.
    Close,
}

/// Order egress 변환에 필요한 보조 메타데이터.
///
/// `OrderRequest` 본문으로 표현되지 않는 transport 전용 필드만 담는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderEgressMeta<'a> {
    /// strategy 태그. ZMQ wire 에서는 최대 32B ASCII 로 직렬화된다.
    pub strategy_tag: &'a str,
    /// 주문 레벨(open/close).
    pub level: WireLevel,
    /// ZMQ wire 용 symbol id.
    pub symbol_id: Option<u32>,
    /// SHM frame 용 symbol idx.
    pub symbol_idx: Option<u32>,
}

/// 주문 도메인 타입 → transport wire 변환 오류.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum OrderAdaptError {
    /// 정수 contract 수량만 허용되는 경로에서 fractional qty 를 받음.
    #[error("qty must be a non-negative integer, got {qty}")]
    NonIntegerQty { qty: f64 },

    /// SHM 경로는 현재 정수 limit price 만 허용한다.
    #[error("limit price must be an integer in SHM path, got {price}")]
    NonIntegerPrice { price: f64 },

    /// Limit 주문인데 가격이 없음.
    #[error("price missing for limit order")]
    LimitPriceMissing,

    /// strategy tag 가 32B 초과.
    #[error("text tag too long: {len} bytes (max 32)")]
    TextTagTooLong { len: usize },

    /// strategy tag 가 ASCII 가 아님.
    #[error("text tag must be ASCII")]
    TextTagNonAscii,

    /// 현재 transport subset 이 지원하지 않는 주문 타입.
    #[error("unsupported order type: {0:?}")]
    UnsupportedOrderType(OrderType),

    /// 현재 transport subset 이 지원하지 않는 TIF.
    #[error("unsupported TIF: {0:?}")]
    UnsupportedTif(TimeInForce),

    /// ZMQ wire 에 필요한 symbol id 가 없음.
    #[error("symbol_id missing for ZMQ wire")]
    SymbolIdMissing,

    /// SHM frame 에 필요한 symbol idx 가 없음.
    #[error("symbol_idx missing for SHM frame")]
    SymbolIdxMissing,

    /// 하위 wire encode/decode 오류.
    #[error("wire encode failed: {0}")]
    Wire(#[from] WireError),
}

#[inline]
fn integer_qty(qty: f64) -> Result<i64, OrderAdaptError> {
    if !qty.is_finite() || qty < 0.0 || qty.fract() != 0.0 || qty > i64::MAX as f64 {
        return Err(OrderAdaptError::NonIntegerQty { qty });
    }
    Ok(qty as i64)
}

#[inline]
fn integer_price(price: f64) -> Result<i64, OrderAdaptError> {
    if !price.is_finite() || price < 0.0 || price.fract() != 0.0 || price > i64::MAX as f64 {
        return Err(OrderAdaptError::NonIntegerPrice { price });
    }
    Ok(price as i64)
}

#[inline]
fn encode_ascii_tag(tag: &str) -> Result<[u8; 32], OrderAdaptError> {
    if !tag.is_ascii() {
        return Err(OrderAdaptError::TextTagNonAscii);
    }
    if tag.len() > 32 {
        return Err(OrderAdaptError::TextTagTooLong { len: tag.len() });
    }

    let mut out = [0u8; 32];
    out[..tag.len()].copy_from_slice(tag.as_bytes());
    Ok(out)
}

#[inline]
fn side_code(side: OrderSide) -> u8 {
    match side {
        OrderSide::Buy => SIDE_BUY,
        OrderSide::Sell => SIDE_SELL,
    }
}

#[inline]
fn frame_order_type_code(order_type: OrderType) -> u8 {
    match order_type {
        OrderType::Limit => 0,
        OrderType::Market => 1,
    }
}

#[inline]
fn wire_order_type_code(order_type: OrderType) -> u8 {
    match order_type {
        OrderType::Limit => ORDER_TYPE_LIMIT,
        OrderType::Market => ORDER_TYPE_MARKET,
    }
}

#[inline]
fn tif_code(tif: TimeInForce) -> u8 {
    match tif {
        TimeInForce::Gtc => TIF_GTC,
        TimeInForce::Ioc => TIF_IOC,
        TimeInForce::Fok => TIF_FOK,
    }
}

#[inline]
fn level_code(level: WireLevel) -> u8 {
    match level {
        WireLevel::Open => LEVEL_OPEN,
        WireLevel::Close => LEVEL_CLOSE,
    }
}

#[inline]
fn place_level_code(level: WireLevel) -> u8 {
    match level {
        WireLevel::Open => PLACE_LEVEL_OPEN,
        WireLevel::Close => PLACE_LEVEL_CLOSE,
    }
}

/// 도메인 주문을 SHM `OrderFrame` 으로 변환한다.
///
/// 현재 SHM 경로는 `price: i64` 표현을 사용하므로 limit 주문은 정수 가격만 허용한다.
pub fn order_request_to_order_frame(
    req: &OrderRequest,
    meta: &OrderEgressMeta<'_>,
) -> Result<OrderFrame, OrderAdaptError> {
    let symbol_idx = meta.symbol_idx.ok_or(OrderAdaptError::SymbolIdxMissing)?;
    let size = integer_qty(req.qty)?;
    let ord_type = frame_order_type_code(req.order_type);
    let tif = tif_code(req.tif);
    let price = match req.order_type {
        OrderType::Market => 0,
        OrderType::Limit => {
            let p = req.price.ok_or(OrderAdaptError::LimitPriceMissing)?;
            integer_price(p)?
        }
    };

    Ok(OrderFrame {
        seq: 0,
        kind: OrderKind::Place as u8,
        exchange_id: exchange_to_u8(req.exchange),
        _pad1: [0; 2],
        symbol_idx,
        side: side_code(req.side),
        tif,
        ord_type,
        _pad2: [0; 1],
        price,
        size,
        client_id: req.client_seq,
        ts_ns: req.origin_ts_ns,
        aux: PlaceAuxMeta::from_parts(place_level_code(meta.level), req.reduce_only, meta.strategy_tag)
            .pack(),
        _pad3: [0; 16],
    })
}

/// 도메인 주문을 ZMQ `OrderRequestWire` 로 변환한다.
pub fn order_request_to_order_request_wire(
    req: &OrderRequest,
    meta: &OrderEgressMeta<'_>,
) -> Result<OrderRequestWire, OrderAdaptError> {
    let symbol_id = meta.symbol_id.ok_or(OrderAdaptError::SymbolIdMissing)?;
    let size = integer_qty(req.qty)?;
    let text_tag = encode_ascii_tag(meta.strategy_tag)?;
    let order_type = wire_order_type_code(req.order_type);
    let tif = tif_code(req.tif);
    let price = match req.order_type {
        OrderType::Market => 0.0,
        OrderType::Limit => req.price.ok_or(OrderAdaptError::LimitPriceMissing)?,
    };

    let mut wire = OrderRequestWire {
        exchange: u16::from(exchange_to_u8(req.exchange)),
        side: side_code(req.side),
        order_type,
        symbol_id,
        tif,
        level: level_code(meta.level),
        flags: 0,
        _pad0: 0,
        _pad1: 0,
        price,
        size,
        client_seq: req.client_seq,
        origin_ts_ns: req.origin_ts_ns,
        text_tag,
        _reserved: [0; 48],
    };
    if req.reduce_only {
        wire.flags = FLAG_REDUCE_ONLY;
    }
    Ok(wire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hft_types::{ExchangeId, Symbol};
    use std::sync::Arc;

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
            client_id: Arc::from("v8-1"),
        }
    }

    fn sample_meta() -> OrderEgressMeta<'static> {
        OrderEgressMeta {
            strategy_tag: "v8",
            level: WireLevel::Open,
            symbol_id: Some(77),
            symbol_idx: Some(88),
        }
    }

    #[test]
    fn shm_adapter_happy_path() {
        let req = sample_req(ExchangeId::Gate);
        let meta = sample_meta();
        let frame = order_request_to_order_frame(&req, &meta).expect("frame");
        let place_meta = PlaceAuxMeta::unpack(&frame.aux);

        assert_eq!(frame.seq, 0);
        assert_eq!(frame.kind, OrderKind::Place as u8);
        assert_eq!(frame.exchange_id, exchange_to_u8(ExchangeId::Gate));
        assert_eq!(frame.symbol_idx, 88);
        assert_eq!(frame.side, SIDE_BUY);
        assert_eq!(frame.tif, TIF_GTC);
        assert_eq!(frame.ord_type, 0);
        assert_eq!(frame.price, 100);
        assert_eq!(frame.size, 1);
        assert_eq!(frame.client_id, req.client_seq);
        assert_eq!(frame.ts_ns, req.origin_ts_ns);
        assert_eq!(place_meta.wire_level_code(), PLACE_LEVEL_OPEN);
        assert!(!place_meta.reduce_only());
        assert_eq!(place_meta.text_tag_str(), "v8");
    }

    #[test]
    fn shm_adapter_market_order() {
        let mut req = sample_req(ExchangeId::Gate);
        req.order_type = OrderType::Market;
        req.price = None;
        req.tif = TimeInForce::Ioc;

        let frame = order_request_to_order_frame(&req, &sample_meta()).expect("frame");
        assert_eq!(frame.ord_type, 1);
        assert_eq!(frame.price, 0);
        assert_eq!(frame.tif, TIF_IOC);
    }

    #[test]
    fn shm_adapter_packs_place_meta_into_aux() {
        let mut req = sample_req(ExchangeId::Gate);
        req.reduce_only = true;
        let mut meta = sample_meta();
        meta.level = WireLevel::Close;
        meta.strategy_tag = "v7";

        let frame = order_request_to_order_frame(&req, &meta).expect("frame");
        let place_meta = PlaceAuxMeta::unpack(&frame.aux);
        assert_eq!(place_meta.wire_level_code(), PLACE_LEVEL_CLOSE);
        assert!(place_meta.reduce_only());
        assert_eq!(place_meta.text_tag_str(), "v7");
    }

    #[test]
    fn shm_adapter_rejects_non_integer_qty() {
        let mut req = sample_req(ExchangeId::Gate);
        req.qty = 1.5;

        assert!(matches!(
            order_request_to_order_frame(&req, &sample_meta()),
            Err(OrderAdaptError::NonIntegerQty { qty }) if qty == 1.5
        ));
    }

    #[test]
    fn shm_adapter_rejects_non_integer_limit_price() {
        let mut req = sample_req(ExchangeId::Gate);
        req.price = Some(100.5);

        assert!(matches!(
            order_request_to_order_frame(&req, &sample_meta()),
            Err(OrderAdaptError::NonIntegerPrice { price }) if price == 100.5
        ));
    }

    #[test]
    fn shm_adapter_rejects_missing_symbol_idx() {
        let mut meta = sample_meta();
        meta.symbol_idx = None;

        assert!(matches!(
            order_request_to_order_frame(&sample_req(ExchangeId::Gate), &meta),
            Err(OrderAdaptError::SymbolIdxMissing)
        ));
    }

    #[test]
    fn shm_adapter_rejects_limit_without_price() {
        let mut req = sample_req(ExchangeId::Gate);
        req.price = None;

        assert!(matches!(
            order_request_to_order_frame(&req, &sample_meta()),
            Err(OrderAdaptError::LimitPriceMissing)
        ));
    }

    #[test]
    fn zmq_adapter_happy_path() {
        let req = sample_req(ExchangeId::Gate);
        let meta = sample_meta();
        let wire = order_request_to_order_request_wire(&req, &meta).expect("wire");

        assert_eq!(wire.exchange, u16::from(exchange_to_u8(ExchangeId::Gate)));
        assert_eq!(wire.symbol_id, 77);
        assert_eq!(wire.side, SIDE_BUY);
        assert_eq!(wire.order_type, ORDER_TYPE_LIMIT);
        assert_eq!(wire.tif, TIF_GTC);
        assert_eq!(wire.level, LEVEL_OPEN);
        assert_eq!(wire.flags, 0);
        assert_eq!(wire.price, 100.0);
        assert_eq!(wire.size, 1);
        assert_eq!(wire.client_seq, req.client_seq);
        assert_eq!(wire.origin_ts_ns, req.origin_ts_ns);
        assert_eq!(&wire.text_tag[..2], b"v8");

        let mut buf = [0u8; crate::order_wire::ORDER_REQUEST_WIRE_SIZE];
        wire.encode(&mut buf);
        let decoded = OrderRequestWire::decode(&buf).expect("decode");
        assert_eq!(decoded, wire);
    }

    #[test]
    fn zmq_adapter_market_no_price_check() {
        let mut req = sample_req(ExchangeId::Gate);
        req.order_type = OrderType::Market;
        req.price = None;

        let wire = order_request_to_order_request_wire(&req, &sample_meta()).expect("wire");
        assert_eq!(wire.order_type, ORDER_TYPE_MARKET);
        assert_eq!(wire.price, 0.0);
    }

    #[test]
    fn zmq_adapter_fractional_price_ok() {
        let mut req = sample_req(ExchangeId::Gate);
        req.price = Some(100.5);

        let wire = order_request_to_order_request_wire(&req, &sample_meta()).expect("wire");
        assert_eq!(wire.price, 100.5);
    }

    #[test]
    fn zmq_adapter_rejects_missing_symbol_id() {
        let mut meta = sample_meta();
        meta.symbol_id = None;

        assert!(matches!(
            order_request_to_order_request_wire(&sample_req(ExchangeId::Gate), &meta),
            Err(OrderAdaptError::SymbolIdMissing)
        ));
    }

    #[test]
    fn zmq_adapter_rejects_non_ascii_tag() {
        let mut meta = sample_meta();
        meta.strategy_tag = "v7_한글";

        assert!(matches!(
            order_request_to_order_request_wire(&sample_req(ExchangeId::Gate), &meta),
            Err(OrderAdaptError::TextTagNonAscii)
        ));
    }

    #[test]
    fn zmq_adapter_rejects_too_long_tag() {
        let mut meta = sample_meta();
        meta.strategy_tag = "abcdefghijklmnopqrstuvwxyzABCDEFG";

        assert!(matches!(
            order_request_to_order_request_wire(&sample_req(ExchangeId::Gate), &meta),
            Err(OrderAdaptError::TextTagTooLong { len: 33 })
        ));
    }

    #[test]
    fn zmq_adapter_reduce_only_flag() {
        let mut req = sample_req(ExchangeId::Gate);
        req.reduce_only = true;
        let meta = sample_meta();
        let wire = order_request_to_order_request_wire(&req, &meta).expect("wire");
        assert_eq!(wire.flags, FLAG_REDUCE_ONLY);
    }

    #[test]
    fn zmq_adapter_level_close() {
        let mut meta = sample_meta();
        meta.level = WireLevel::Close;

        let wire = order_request_to_order_request_wire(&sample_req(ExchangeId::Gate), &meta)
            .expect("wire");
        assert_eq!(wire.level, LEVEL_CLOSE);
    }

    #[test]
    fn current_domain_enum_subset_maps_without_error() {
        let order_types = [OrderType::Limit, OrderType::Market];
        let tifs = [TimeInForce::Gtc, TimeInForce::Ioc, TimeInForce::Fok];

        for order_type in order_types {
            for tif in tifs {
                let mut req = sample_req(ExchangeId::Gate);
                req.order_type = order_type;
                req.tif = tif;
                req.price = if matches!(order_type, OrderType::Limit) {
                    Some(100.0)
                } else {
                    None
                };

                assert!(order_request_to_order_frame(&req, &sample_meta()).is_ok());
                assert!(order_request_to_order_request_wire(&req, &sample_meta()).is_ok());
            }
        }
    }

    #[test]
    fn both_adapters_exchange_id_consistency() {
        let exchanges = [
            ExchangeId::Binance,
            ExchangeId::Gate,
            ExchangeId::Bybit,
            ExchangeId::Bitget,
            ExchangeId::Okx,
        ];

        for exchange in exchanges {
            let req = sample_req(exchange);
            let frame = order_request_to_order_frame(&req, &sample_meta()).expect("frame");
            let wire = order_request_to_order_request_wire(&req, &sample_meta()).expect("wire");

            assert_eq!(u16::from(frame.exchange_id), wire.exchange);
        }
    }
}

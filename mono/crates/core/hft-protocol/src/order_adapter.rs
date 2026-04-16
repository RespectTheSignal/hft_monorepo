//! 주문 도메인 타입 → transport wire 어댑터.
//!
//! Phase 2 E 에서는 전략이 생성한 [`hft_exchange_api::OrderRequest`] 를
//! 두 경로로 나눠 보낸다.
//!
//! - SHM 정상 경로: [`hft_shm::OrderFrame`]
//! - ZMQ fallback 경로: [`crate::order_wire::OrderRequestWire`]
//!
//! 이 모듈은 두 변환에서 공통으로 필요한 메타데이터와 에러 타입을 정의한다.
//! 실제 변환 함수는 Step 4a 다음 커밋에서 추가한다.

use hft_exchange_api::{OrderType, TimeInForce};
use thiserror::Error;

use crate::order_wire::WireError;

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
/// `OrderRequest` 본문에는 transport 전용 필드가 없어 caller 가 함께 제공해야 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderEgressMeta<'a> {
    /// 전략 내부 단조 증가 시퀀스.
    pub client_seq: u64,
    /// strategy 태그. ZMQ wire 에서는 최대 32B ASCII 로 직렬화된다.
    pub strategy_tag: &'a str,
    /// 주문 레벨(open/close).
    pub level: WireLevel,
    /// reduce_only 비트. 현재 SHM frame 에서는 사용하지 않는다.
    pub reduce_only: bool,
    /// transport 진입 시각 (UTC epoch ns).
    pub origin_ts_ns: u64,
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

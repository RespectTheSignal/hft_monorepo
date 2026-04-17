//! 주문 wire 포맷 (128B 고정, little-endian).
//!
//! # 설계 원칙
//! - market data wire 와 동일하게 **offset 기반 수동 인코딩**을 사용한다.
//! - SHM order frame 과는 별개 포맷이며, ZMQ/TCP egress 용으로 설계한다.
//! - padding / reserved 영역은 항상 0 으로 유지해 layout drift 를 빠르게 잡는다.

use byteorder::{ByteOrder, LittleEndian};
use thiserror::Error;

/// OrderRequestWire 고정 크기.
pub const ORDER_REQUEST_WIRE_SIZE: usize = 128;
/// OrderResultWire 고정 크기.
pub const ORDER_RESULT_WIRE_SIZE: usize = 128;

/// 유효한 exchange 코드 최대값. SHM order ring 과 동일한 매핑을 사용한다.
pub const EXCHANGE_ID_MAX: u16 = 5;

/// side code: Buy.
pub const SIDE_BUY: u8 = 0;
/// side code: Sell.
pub const SIDE_SELL: u8 = 1;

/// order_type code: Limit.
pub const ORDER_TYPE_LIMIT: u8 = 0;
/// order_type code: Market.
pub const ORDER_TYPE_MARKET: u8 = 1;
/// order_type code: PostOnly.
pub const ORDER_TYPE_POST_ONLY: u8 = 2;
/// order_type code: FOK 전용 타입.
pub const ORDER_TYPE_FOK: u8 = 3;
/// order_type code: IOC 전용 타입.
pub const ORDER_TYPE_IOC: u8 = 4;
/// order_type code: StopLimit.
pub const ORDER_TYPE_STOP_LIMIT: u8 = 5;
/// order_type code: StopMarket.
pub const ORDER_TYPE_STOP_MARKET: u8 = 6;

/// tif code: Good Till Cancel.
pub const TIF_GTC: u8 = 0;
/// tif code: Immediate Or Cancel.
pub const TIF_IOC: u8 = 1;
/// tif code: Fill Or Kill.
pub const TIF_FOK: u8 = 2;
/// tif code: GTX/PostOnly.
pub const TIF_GTX: u8 = 3;

/// level code: Open.
pub const LEVEL_OPEN: u8 = 0;
/// level code: Close.
pub const LEVEL_CLOSE: u8 = 1;

/// flags bit: reduce_only.
pub const FLAG_REDUCE_ONLY: u8 = 0b0000_0001;

/// status code: Submitted.
pub const STATUS_SUBMITTED: u8 = 0;
/// status code: Accepted.
pub const STATUS_ACCEPTED: u8 = 1;
/// status code: PartiallyFilled.
pub const STATUS_PARTIALLY_FILLED: u8 = 2;
/// status code: Filled.
pub const STATUS_FILLED: u8 = 3;
/// status code: Cancelled.
pub const STATUS_CANCELLED: u8 = 4;
/// status code: Rejected.
pub const STATUS_REJECTED: u8 = 5;
/// status code: Expired.
pub const STATUS_EXPIRED: u8 = 6;
/// status code: Heartbeat (gateway liveness signal, not a real order result).
pub const STATUS_HEARTBEAT: u8 = 255;

/// request.exchange offset.
pub const OFFSET_EXCHANGE: usize = 0;
/// request.side offset.
pub const OFFSET_SIDE: usize = 2;
/// request.order_type offset.
pub const OFFSET_ORDER_TYPE: usize = 3;
/// request.symbol_id offset.
pub const OFFSET_SYMBOL_ID: usize = 4;
/// request.tif offset.
pub const OFFSET_TIF: usize = 8;
/// request.level offset.
pub const OFFSET_LEVEL: usize = 9;
/// request.flags offset.
pub const OFFSET_FLAGS: usize = 10;
/// request._pad0 offset.
pub const OFFSET_PAD0: usize = 11;
/// request._pad1 offset.
pub const OFFSET_PAD1: usize = 12;
/// request.price offset.
pub const OFFSET_PRICE: usize = 16;
/// request.size offset.
pub const OFFSET_SIZE: usize = 24;
/// request.client_seq offset.
pub const OFFSET_CLIENT_SEQ: usize = 32;
/// request.origin_ts_ns offset.
pub const OFFSET_ORIGIN_TS_NS: usize = 40;
/// request.text_tag offset.
pub const OFFSET_TEXT_TAG: usize = 48;
/// request._reserved offset.
pub const OFFSET_RESERVED: usize = 80;

/// result.client_seq offset.
pub const RESULT_OFFSET_CLIENT_SEQ: usize = 0;
/// result.gateway_ts_ns offset.
pub const RESULT_OFFSET_GATEWAY_TS_NS: usize = 8;
/// result.filled_size offset.
pub const RESULT_OFFSET_FILLED_SIZE: usize = 16;
/// result.reject_code offset.
pub const RESULT_OFFSET_REJECT_CODE: usize = 24;
/// result.status offset.
pub const RESULT_OFFSET_STATUS: usize = 28;
/// result._pad0 offset.
pub const RESULT_OFFSET_PAD0: usize = 29;
/// result.exchange_order_id offset.
pub const RESULT_OFFSET_EXCHANGE_ORDER_ID: usize = 32;
/// result.text_tag offset.
pub const RESULT_OFFSET_TEXT_TAG: usize = 80;
/// result._reserved offset.
pub const RESULT_OFFSET_RESERVED: usize = 112;

/// 주문 요청 wire decode 에러.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WireError {
    /// 유효하지 않은 exchange id.
    #[error("invalid exchange id: {0}")]
    InvalidExchangeId(u16),
    /// 유효하지 않은 side code.
    #[error("invalid side: {0}")]
    InvalidSide(u8),
    /// 유효하지 않은 order_type code.
    #[error("invalid order_type: {0}")]
    InvalidOrderType(u8),
    /// 유효하지 않은 tif code.
    #[error("invalid tif: {0}")]
    InvalidTif(u8),
    /// 유효하지 않은 level code.
    #[error("invalid level: {0}")]
    InvalidLevel(u8),
    /// 유효하지 않은 status code.
    #[error("invalid status: {0}")]
    InvalidStatus(u8),
    /// padding / reserved 바이트가 0 이 아님.
    #[error("non-zero padding in {field} at offset {offset}")]
    NonZeroPadding { field: &'static str, offset: usize },
}

/// strategy → gateway 요청 wire.
///
/// `text_tag` 는 strategy 태그 전용 32B ASCII 슬롯이다.
/// `flags` 는 현재 bit0 (`reduce_only`) 만 사용한다.
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrderRequestWire {
    /// 거래소 코드. SHM symbol table 과 동일한 숫자 매핑을 사용한다.
    pub exchange: u16,
    /// 주문 방향. 0=Buy, 1=Sell.
    pub side: u8,
    /// 주문 타입 코드. Limit/Market 외 확장 타입을 예약한다.
    pub order_type: u8,
    /// Symbol table index.
    pub symbol_id: u32,
    /// Time-in-force 코드.
    pub tif: u8,
    /// Open/Close 구분.
    pub level: u8,
    /// 플래그 비트셋. 현재는 bit0=reduce_only 만 사용한다.
    pub flags: u8,
    /// 8B 정렬 유지를 위한 1B padding. encode 시 항상 0.
    pub _pad0: u8,
    /// 가격 필드를 8B 정렬하기 위한 4B padding. encode 시 항상 0.
    pub _pad1: u32,
    /// 주문 가격. Market 이면 0.0 을 허용한다.
    pub price: f64,
    /// 거래소 raw lot 단위 수량.
    pub size: i64,
    /// strategy 내부 단조 증가 primary key.
    pub client_seq: u64,
    /// strategy 발행 시각. UTC epoch ns 기준.
    pub origin_ts_ns: u64,
    /// strategy 태그. ASCII + zero padding 32B.
    pub text_tag: [u8; 32],
    /// 향후 확장을 위한 예약 영역. encode 시 항상 0.
    pub _reserved: [u8; 48],
}

impl Default for OrderRequestWire {
    fn default() -> Self {
        Self {
            exchange: 0,
            side: SIDE_BUY,
            order_type: ORDER_TYPE_LIMIT,
            symbol_id: 0,
            tif: TIF_GTC,
            level: LEVEL_OPEN,
            flags: 0,
            _pad0: 0,
            _pad1: 0,
            price: 0.0,
            size: 0,
            client_seq: 0,
            origin_ts_ns: 0,
            text_tag: [0; 32],
            _reserved: [0; 48],
        }
    }
}

impl OrderRequestWire {
    /// reduce_only 플래그가 켜져 있는지 반환한다.
    #[inline]
    pub const fn reduce_only(&self) -> bool {
        (self.flags & FLAG_REDUCE_ONLY) != 0
    }

    /// reduce_only bit 만 갱신한다. 다른 reserved bit 는 유지하지 않는다.
    #[inline]
    pub fn set_reduce_only(&mut self, enabled: bool) {
        self.flags = if enabled { FLAG_REDUCE_ONLY } else { 0 };
    }

    /// wire buffer 에 little-endian 으로 인코드한다.
    pub fn encode(&self, buf: &mut [u8; ORDER_REQUEST_WIRE_SIZE]) {
        buf.fill(0);

        LittleEndian::write_u16(&mut buf[OFFSET_EXCHANGE..OFFSET_SIDE], self.exchange);
        buf[OFFSET_SIDE] = self.side;
        buf[OFFSET_ORDER_TYPE] = self.order_type;
        LittleEndian::write_u32(&mut buf[OFFSET_SYMBOL_ID..OFFSET_TIF], self.symbol_id);
        buf[OFFSET_TIF] = self.tif;
        buf[OFFSET_LEVEL] = self.level;
        // reserved bit 는 항상 0 으로 정규화한다.
        buf[OFFSET_FLAGS] = self.flags & FLAG_REDUCE_ONLY;
        LittleEndian::write_f64(&mut buf[OFFSET_PRICE..OFFSET_SIZE], self.price);
        LittleEndian::write_i64(&mut buf[OFFSET_SIZE..OFFSET_CLIENT_SEQ], self.size);
        LittleEndian::write_u64(
            &mut buf[OFFSET_CLIENT_SEQ..OFFSET_ORIGIN_TS_NS],
            self.client_seq,
        );
        LittleEndian::write_u64(
            &mut buf[OFFSET_ORIGIN_TS_NS..OFFSET_TEXT_TAG],
            self.origin_ts_ns,
        );
        write_zero_padded_field(&mut buf[OFFSET_TEXT_TAG..OFFSET_RESERVED], &self.text_tag);
    }

    /// 128B wire buffer 를 decode 한다.
    pub fn decode(buf: &[u8; ORDER_REQUEST_WIRE_SIZE]) -> Result<Self, WireError> {
        let exchange = LittleEndian::read_u16(&buf[OFFSET_EXCHANGE..OFFSET_SIDE]);
        if !is_valid_exchange_id(exchange) {
            return Err(WireError::InvalidExchangeId(exchange));
        }

        let side = buf[OFFSET_SIDE];
        if !is_valid_side(side) {
            return Err(WireError::InvalidSide(side));
        }

        let order_type = buf[OFFSET_ORDER_TYPE];
        if !is_valid_order_type(order_type) {
            return Err(WireError::InvalidOrderType(order_type));
        }

        let tif = buf[OFFSET_TIF];
        if !is_valid_tif(tif) {
            return Err(WireError::InvalidTif(tif));
        }

        let level = buf[OFFSET_LEVEL];
        if !is_valid_level(level) {
            return Err(WireError::InvalidLevel(level));
        }

        let flags = buf[OFFSET_FLAGS];
        if flags & !FLAG_REDUCE_ONLY != 0 {
            return Err(WireError::NonZeroPadding {
                field: "flags",
                offset: OFFSET_FLAGS,
            });
        }

        ensure_zero(&buf[OFFSET_PAD0..OFFSET_PAD0 + 1], "_pad0", OFFSET_PAD0)?;
        ensure_zero(&buf[OFFSET_PAD1..OFFSET_PRICE], "_pad1", OFFSET_PAD1)?;
        ensure_zero(
            &buf[OFFSET_RESERVED..ORDER_REQUEST_WIRE_SIZE],
            "_reserved",
            OFFSET_RESERVED,
        )?;

        Ok(Self {
            exchange,
            side,
            order_type,
            symbol_id: LittleEndian::read_u32(&buf[OFFSET_SYMBOL_ID..OFFSET_TIF]),
            tif,
            level,
            flags,
            _pad0: 0,
            _pad1: 0,
            price: LittleEndian::read_f64(&buf[OFFSET_PRICE..OFFSET_SIZE]),
            size: LittleEndian::read_i64(&buf[OFFSET_SIZE..OFFSET_CLIENT_SEQ]),
            client_seq: LittleEndian::read_u64(&buf[OFFSET_CLIENT_SEQ..OFFSET_ORIGIN_TS_NS]),
            origin_ts_ns: LittleEndian::read_u64(&buf[OFFSET_ORIGIN_TS_NS..OFFSET_TEXT_TAG]),
            text_tag: read_fixed(&buf[OFFSET_TEXT_TAG..OFFSET_RESERVED]),
            _reserved: [0; 48],
        })
    }
}

/// gateway → strategy 결과 wire.
///
/// `exchange_order_id` 는 거래소 원문 ID 를 ASCII zero-padded 48B 로 보관한다.
/// `text_tag` 는 request 에서 온 태그를 그대로 echo 하는 슬롯이다.
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrderResultWire {
    /// 원 요청의 client_seq echo.
    pub client_seq: u64,
    /// gateway 가 결과를 기록한 시각. UTC epoch ns 기준.
    pub gateway_ts_ns: u64,
    /// 누적 체결 수량.
    pub filled_size: i64,
    /// 거래소 raw reject code. 0 이면 reject 아님.
    pub reject_code: u32,
    /// 제출/접수/거절 등 상태 코드.
    pub status: u8,
    /// 32B 정렬 유지를 위한 padding. encode 시 항상 0.
    pub _pad0: [u8; 3],
    /// 거래소 주문 ID. ASCII + zero padding 48B.
    pub exchange_order_id: [u8; 48],
    /// strategy 태그 echo 슬롯. ASCII + zero padding 32B.
    pub text_tag: [u8; 32],
    /// 향후 확장을 위한 예약 영역. encode 시 항상 0.
    pub _reserved: [u8; 16],
}

impl Default for OrderResultWire {
    fn default() -> Self {
        Self {
            client_seq: 0,
            gateway_ts_ns: 0,
            filled_size: 0,
            reject_code: 0,
            status: STATUS_SUBMITTED,
            _pad0: [0; 3],
            exchange_order_id: [0; 48],
            text_tag: [0; 32],
            _reserved: [0; 16],
        }
    }
}

impl OrderResultWire {
    /// wire buffer 에 little-endian 으로 인코드한다.
    pub fn encode(&self, buf: &mut [u8; ORDER_RESULT_WIRE_SIZE]) {
        buf.fill(0);

        LittleEndian::write_u64(
            &mut buf[RESULT_OFFSET_CLIENT_SEQ..RESULT_OFFSET_GATEWAY_TS_NS],
            self.client_seq,
        );
        LittleEndian::write_u64(
            &mut buf[RESULT_OFFSET_GATEWAY_TS_NS..RESULT_OFFSET_FILLED_SIZE],
            self.gateway_ts_ns,
        );
        LittleEndian::write_i64(
            &mut buf[RESULT_OFFSET_FILLED_SIZE..RESULT_OFFSET_REJECT_CODE],
            self.filled_size,
        );
        LittleEndian::write_u32(
            &mut buf[RESULT_OFFSET_REJECT_CODE..RESULT_OFFSET_STATUS],
            self.reject_code,
        );
        buf[RESULT_OFFSET_STATUS] = self.status;
        write_zero_padded_field(
            &mut buf[RESULT_OFFSET_EXCHANGE_ORDER_ID..RESULT_OFFSET_TEXT_TAG],
            &self.exchange_order_id,
        );
        write_zero_padded_field(
            &mut buf[RESULT_OFFSET_TEXT_TAG..RESULT_OFFSET_RESERVED],
            &self.text_tag,
        );
    }

    /// 128B wire buffer 를 decode 한다.
    pub fn decode(buf: &[u8; ORDER_RESULT_WIRE_SIZE]) -> Result<Self, WireError> {
        let status = buf[RESULT_OFFSET_STATUS];
        if !is_valid_status(status) {
            return Err(WireError::InvalidStatus(status));
        }

        ensure_zero(
            &buf[RESULT_OFFSET_PAD0..RESULT_OFFSET_EXCHANGE_ORDER_ID],
            "_pad0",
            RESULT_OFFSET_PAD0,
        )?;
        ensure_zero(
            &buf[RESULT_OFFSET_RESERVED..ORDER_RESULT_WIRE_SIZE],
            "_reserved",
            RESULT_OFFSET_RESERVED,
        )?;

        Ok(Self {
            client_seq: LittleEndian::read_u64(
                &buf[RESULT_OFFSET_CLIENT_SEQ..RESULT_OFFSET_GATEWAY_TS_NS],
            ),
            gateway_ts_ns: LittleEndian::read_u64(
                &buf[RESULT_OFFSET_GATEWAY_TS_NS..RESULT_OFFSET_FILLED_SIZE],
            ),
            filled_size: LittleEndian::read_i64(
                &buf[RESULT_OFFSET_FILLED_SIZE..RESULT_OFFSET_REJECT_CODE],
            ),
            reject_code: LittleEndian::read_u32(
                &buf[RESULT_OFFSET_REJECT_CODE..RESULT_OFFSET_STATUS],
            ),
            status,
            _pad0: [0; 3],
            exchange_order_id: read_fixed(
                &buf[RESULT_OFFSET_EXCHANGE_ORDER_ID..RESULT_OFFSET_TEXT_TAG],
            ),
            text_tag: read_fixed(&buf[RESULT_OFFSET_TEXT_TAG..RESULT_OFFSET_RESERVED]),
            _reserved: [0; 16],
        })
    }
}

const _: () = assert!(std::mem::size_of::<OrderRequestWire>() == ORDER_REQUEST_WIRE_SIZE);
const _: () = assert!(std::mem::align_of::<OrderRequestWire>() == 64);
const _: () = assert!(std::mem::size_of::<OrderResultWire>() == ORDER_RESULT_WIRE_SIZE);
const _: () = assert!(std::mem::align_of::<OrderResultWire>() == 64);

#[inline]
fn is_valid_exchange_id(v: u16) -> bool {
    (1..=EXCHANGE_ID_MAX).contains(&v)
}

#[inline]
fn is_valid_side(v: u8) -> bool {
    matches!(v, SIDE_BUY | SIDE_SELL)
}

#[inline]
fn is_valid_order_type(v: u8) -> bool {
    matches!(
        v,
        ORDER_TYPE_LIMIT
            | ORDER_TYPE_MARKET
            | ORDER_TYPE_POST_ONLY
            | ORDER_TYPE_FOK
            | ORDER_TYPE_IOC
            | ORDER_TYPE_STOP_LIMIT
            | ORDER_TYPE_STOP_MARKET
    )
}

#[inline]
fn is_valid_tif(v: u8) -> bool {
    matches!(v, TIF_GTC | TIF_IOC | TIF_FOK | TIF_GTX)
}

#[inline]
fn is_valid_level(v: u8) -> bool {
    matches!(v, LEVEL_OPEN | LEVEL_CLOSE)
}

#[inline]
fn is_valid_status(v: u8) -> bool {
    matches!(
        v,
        STATUS_SUBMITTED
            | STATUS_ACCEPTED
            | STATUS_PARTIALLY_FILLED
            | STATUS_FILLED
            | STATUS_CANCELLED
            | STATUS_REJECTED
            | STATUS_EXPIRED
            | STATUS_HEARTBEAT
    )
}

/// heartbeat 용 result wire 생성 helper.
/// `gateway_ts_ns` 만 유의미하고 나머지는 0 / default.
pub fn build_heartbeat_wire(gateway_ts_ns: u64) -> OrderResultWire {
    OrderResultWire {
        client_seq: 0,
        gateway_ts_ns,
        filled_size: 0,
        reject_code: 0,
        status: STATUS_HEARTBEAT,
        _pad0: [0; 3],
        exchange_order_id: [0; 48],
        text_tag: [0; 32],
        _reserved: [0; 16],
    }
}

/// 128B result wire buffer 의 status 가 heartbeat 인지 빠르게 판별.
/// decode 하지 않고 offset 만 본다.
#[inline]
pub fn is_heartbeat_wire(buf: &[u8; ORDER_RESULT_WIRE_SIZE]) -> bool {
    buf[RESULT_OFFSET_STATUS] == STATUS_HEARTBEAT
}

fn ensure_zero(bytes: &[u8], field: &'static str, offset: usize) -> Result<(), WireError> {
    if bytes.iter().any(|byte| *byte != 0) {
        return Err(WireError::NonZeroPadding { field, offset });
    }
    Ok(())
}

fn write_zero_padded_field(dst: &mut [u8], src: &[u8]) {
    dst.fill(0);
    let used = src.iter().position(|byte| *byte == 0).unwrap_or(src.len());
    let len = used.min(dst.len());
    dst[..len].copy_from_slice(&src[..len]);
}

fn read_fixed<const N: usize>(src: &[u8]) -> [u8; N] {
    debug_assert_eq!(src.len(), N, "fixed-size field length mismatch");
    let mut out = [0u8; N];
    out.copy_from_slice(src);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    const DESIGN_REQUEST_WIRE_BYTES: [u8; ORDER_REQUEST_WIRE_SIZE] = [
        0x01, 0x00, 0x00, 0x00, 0xEF, 0xBE, 0xAD, 0xDE, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x33, 0x33, 0x33, 0x33, 0x03, 0xF3, 0xEF, 0x40, 0x40, 0x42, 0x0F, 0x00, 0x00, 0x00,
        0x00, 0x00, 0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01, 0x00, 0x68, 0x92, 0x2B, 0x24,
        0x13, 0x80, 0x18, 0x76, 0x37, 0x5F, 0x6D, 0x61, 0x5F, 0x63, 0x72, 0x6F, 0x73, 0x73, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    const DESIGN_RESULT_WIRE_BYTES: [u8; ORDER_RESULT_WIRE_SIZE] = [
        0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01, 0x00, 0xCD, 0x5F, 0x49, 0x24, 0x13, 0x80,
        0x18, 0x3F, 0x42, 0x0F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x45, 0x58, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x76, 0x37, 0x5F, 0x6D, 0x61, 0x5F, 0x63, 0x72, 0x6F, 0x73,
        0x73, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    fn design_request_wire() -> OrderRequestWire {
        let mut text_tag = [0u8; 32];
        text_tag[..11].copy_from_slice(b"v7_ma_cross");
        OrderRequestWire {
            exchange: 1,
            side: SIDE_BUY,
            order_type: ORDER_TYPE_LIMIT,
            symbol_id: 0xDEAD_BEEF,
            tif: TIF_GTC,
            level: LEVEL_OPEN,
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

    fn design_result_wire() -> OrderResultWire {
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

    fn random_ascii_field<const N: usize>(rng: &mut StdRng) -> [u8; N] {
        let mut out = [0u8; N];
        let len = rng.gen_range(0..N);
        for byte in &mut out[..len] {
            *byte = rng.gen_range(b'a'..=b'z');
        }
        out
    }

    fn random_request_wire(rng: &mut StdRng) -> OrderRequestWire {
        OrderRequestWire {
            exchange: rng.gen_range(1..=EXCHANGE_ID_MAX),
            side: if rng.gen_bool(0.5) {
                SIDE_BUY
            } else {
                SIDE_SELL
            },
            order_type: rng.gen_range(ORDER_TYPE_LIMIT..=ORDER_TYPE_STOP_MARKET),
            symbol_id: rng.r#gen(),
            tif: rng.gen_range(TIF_GTC..=TIF_GTX),
            level: if rng.gen_bool(0.5) {
                LEVEL_OPEN
            } else {
                LEVEL_CLOSE
            },
            flags: if rng.gen_bool(0.5) {
                FLAG_REDUCE_ONLY
            } else {
                0
            },
            _pad0: 0,
            _pad1: 0,
            price: rng.gen_range(1_000.0..100_000.0),
            size: rng.gen_range(1_i64..10_000_000_i64),
            client_seq: rng.r#gen(),
            origin_ts_ns: rng.r#gen(),
            text_tag: random_ascii_field(rng),
            _reserved: [0; 48],
        }
    }

    fn random_result_wire(rng: &mut StdRng) -> OrderResultWire {
        OrderResultWire {
            client_seq: rng.r#gen(),
            gateway_ts_ns: rng.r#gen(),
            filled_size: rng.gen_range(-5_000_000_i64..5_000_000_i64),
            reject_code: rng.r#gen(),
            status: rng.gen_range(STATUS_SUBMITTED..=STATUS_EXPIRED),
            _pad0: [0; 3],
            exchange_order_id: random_ascii_field(rng),
            text_tag: random_ascii_field(rng),
            _reserved: [0; 16],
        }
    }

    #[test]
    fn roundtrip_order_request_wire_random_100() {
        let mut rng = StdRng::seed_from_u64(0xA11C_E123_0000_0001);
        for _ in 0..100 {
            let wire = random_request_wire(&mut rng);
            let mut buf = [0xAA; ORDER_REQUEST_WIRE_SIZE];
            wire.encode(&mut buf);
            let decoded = OrderRequestWire::decode(&buf).expect("request decode");
            assert_eq!(decoded, wire);
        }
    }

    #[test]
    fn roundtrip_order_result_wire_random_100() {
        let mut rng = StdRng::seed_from_u64(0xA11C_E123_0000_0002);
        for _ in 0..100 {
            let wire = random_result_wire(&mut rng);
            let mut buf = [0xAA; ORDER_RESULT_WIRE_SIZE];
            wire.encode(&mut buf);
            let decoded = OrderResultWire::decode(&buf).expect("result decode");
            assert_eq!(decoded, wire);
        }
    }

    #[test]
    fn order_request_wire_layout_offsets() {
        assert_eq!(OFFSET_EXCHANGE, 0);
        assert_eq!(OFFSET_SIDE, 2);
        assert_eq!(OFFSET_ORDER_TYPE, 3);
        assert_eq!(OFFSET_SYMBOL_ID, 4);
        assert_eq!(OFFSET_TIF, 8);
        assert_eq!(OFFSET_LEVEL, 9);
        assert_eq!(OFFSET_FLAGS, 10);
        assert_eq!(OFFSET_PAD0, 11);
        assert_eq!(OFFSET_PAD1, 12);
        assert_eq!(OFFSET_PRICE, 16);
        assert_eq!(OFFSET_SIZE, 24);
        assert_eq!(OFFSET_CLIENT_SEQ, 32);
        assert_eq!(OFFSET_ORIGIN_TS_NS, 40);
        assert_eq!(OFFSET_TEXT_TAG, 48);
        assert_eq!(OFFSET_RESERVED, 80);
    }

    #[test]
    fn order_request_wire_size_is_128() {
        assert_eq!(std::mem::size_of::<OrderRequestWire>(), 128);
    }

    #[test]
    fn order_result_wire_size_is_128() {
        assert_eq!(std::mem::size_of::<OrderResultWire>(), 128);
    }

    #[test]
    fn order_request_wire_padding_is_zero_after_encode() {
        let mut buf = [0xAA; ORDER_REQUEST_WIRE_SIZE];
        design_request_wire().encode(&mut buf);

        assert_eq!(buf[OFFSET_PAD0], 0);
        assert!(buf[OFFSET_PAD1..OFFSET_PRICE].iter().all(|byte| *byte == 0));
        assert!(buf[OFFSET_RESERVED..ORDER_REQUEST_WIRE_SIZE]
            .iter()
            .all(|byte| *byte == 0));
    }

    #[test]
    fn order_request_wire_decode_rejects_nonzero_pad() {
        let mut buf = [0u8; ORDER_REQUEST_WIRE_SIZE];
        design_request_wire().encode(&mut buf);
        buf[OFFSET_PAD0] = 0xFF;

        assert_eq!(
            OrderRequestWire::decode(&buf),
            Err(WireError::NonZeroPadding {
                field: "_pad0",
                offset: OFFSET_PAD0,
            })
        );
    }

    #[test]
    fn order_request_wire_decode_rejects_invalid_enum() {
        let mut buf = [0u8; ORDER_REQUEST_WIRE_SIZE];

        design_request_wire().encode(&mut buf);
        buf[OFFSET_SIDE] = 0xFF;
        assert_eq!(
            OrderRequestWire::decode(&buf),
            Err(WireError::InvalidSide(0xFF))
        );

        design_request_wire().encode(&mut buf);
        buf[OFFSET_ORDER_TYPE] = 0xFF;
        assert_eq!(
            OrderRequestWire::decode(&buf),
            Err(WireError::InvalidOrderType(0xFF))
        );

        design_request_wire().encode(&mut buf);
        buf[OFFSET_TIF] = 0xFF;
        assert_eq!(
            OrderRequestWire::decode(&buf),
            Err(WireError::InvalidTif(0xFF))
        );

        design_request_wire().encode(&mut buf);
        buf[OFFSET_LEVEL] = 0xFF;
        assert_eq!(
            OrderRequestWire::decode(&buf),
            Err(WireError::InvalidLevel(0xFF))
        );
    }

    #[test]
    fn order_request_wire_flags_reduce_only_bit() {
        let mut buf = [0u8; ORDER_REQUEST_WIRE_SIZE];
        let mut wire = design_request_wire();
        wire.flags = FLAG_REDUCE_ONLY;
        wire.encode(&mut buf);
        let decoded = OrderRequestWire::decode(&buf).expect("decode ok");
        assert!(decoded.reduce_only());

        wire.flags = 0;
        wire.encode(&mut buf);
        buf[OFFSET_FLAGS] = 0b0000_0010;
        assert_eq!(
            OrderRequestWire::decode(&buf),
            Err(WireError::NonZeroPadding {
                field: "flags",
                offset: OFFSET_FLAGS,
            })
        );
    }

    #[test]
    fn order_request_wire_text_tag_zero_padding() {
        let mut wire = design_request_wire();
        let mut tag = [0xAA; 32];
        tag[..3].copy_from_slice(b"v7\0");
        wire.text_tag = tag;

        let mut buf = [0xAA; ORDER_REQUEST_WIRE_SIZE];
        wire.encode(&mut buf);

        assert_eq!(buf[OFFSET_TEXT_TAG], b'v');
        assert_eq!(buf[OFFSET_TEXT_TAG + 1], b'7');
        assert!(buf[OFFSET_TEXT_TAG + 2..OFFSET_RESERVED]
            .iter()
            .all(|byte| *byte == 0));
    }

    #[test]
    fn design_invariance_order_request_wire() {
        let wire = design_request_wire();
        let mut buf = [0u8; ORDER_REQUEST_WIRE_SIZE];
        wire.encode(&mut buf);
        assert_eq!(buf, DESIGN_REQUEST_WIRE_BYTES);

        let decoded =
            OrderRequestWire::decode(&DESIGN_REQUEST_WIRE_BYTES).expect("design request decode");
        assert_eq!(decoded, wire);
    }

    #[test]
    fn design_invariance_order_result_wire() {
        let wire = design_result_wire();
        let mut buf = [0u8; ORDER_RESULT_WIRE_SIZE];
        wire.encode(&mut buf);
        assert_eq!(buf, DESIGN_RESULT_WIRE_BYTES);

        let decoded =
            OrderResultWire::decode(&DESIGN_RESULT_WIRE_BYTES).expect("design result decode");
        assert_eq!(decoded, wire);
    }

    #[test]
    fn heartbeat_wire_roundtrip() {
        let wire = build_heartbeat_wire(999_000_000);
        let mut buf = [0u8; ORDER_RESULT_WIRE_SIZE];
        wire.encode(&mut buf);
        assert!(is_heartbeat_wire(&buf));
        let decoded = OrderResultWire::decode(&buf).expect("heartbeat decode");
        assert_eq!(decoded.status, STATUS_HEARTBEAT);
        assert_eq!(decoded.gateway_ts_ns, 999_000_000);
        assert_eq!(decoded.client_seq, 0);
    }

    #[test]
    fn non_heartbeat_wire_is_not_heartbeat() {
        let wire = design_result_wire();
        let mut buf = [0u8; ORDER_RESULT_WIRE_SIZE];
        wire.encode(&mut buf);
        assert!(!is_heartbeat_wire(&buf));
    }
}

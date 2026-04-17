//! SHM 영역의 바이너리 레이아웃 정의.
//!
//! 모든 구조체는 `#[repr(C)]` + explicit alignment + compile-time size assertion.
//! 다른 언어(Python ctypes / C) 에서 같은 메모리에 접근할 것이므로 필드 순서
//! / 패딩을 **절대 바꾸지 않는다**. version bump 필요 시 `V2 → V3` 로 별도 정의.
//!
//! ## 공통 규약
//! - u64 timestamp 는 `CLOCK_MONOTONIC` 또는 `CLOCK_REALTIME_COARSE` 의 ns.
//! - Price / Size 는 `hft-types::{Price, Size}::raw()` 의 scaled i64.
//! - 문자열 symbol 은 **쓰지 않는다** — `symbol_idx: u32` 로 간접 참조.

use std::sync::atomic::{AtomicU32, AtomicU64};

/// x86_64 / aarch64 L1 cacheline 바이트.
pub const CACHE_LINE: usize = 64;

/// 모든 SHM frame 의 공통 크기 (128B). cache line 2개에 맞춰 잡은 상한.
pub const FRAME_SIZE: usize = 128;
/// `OrderFrame.aux` 전체 바이트 크기.
pub const ORDER_AUX_BYTES: usize = 40;
/// Place 메타 text_tag 바이트 길이.
pub const PLACE_AUX_TEXT_TAG_LEN: usize = 32;
/// `PlaceAuxMeta` 총 바이트 크기.
pub const PLACE_AUX_META_SIZE: usize = ORDER_AUX_BYTES;
/// Place 메타 level code: Open.
pub const PLACE_LEVEL_OPEN: u8 = 0;
/// Place 메타 level code: Close.
pub const PLACE_LEVEL_CLOSE: u8 = 1;
/// Place 메타 flags bit: reduce_only.
pub const PLACE_FLAG_REDUCE_ONLY: u8 = 0b0000_0001;

// ─────────────────────────────────────────────────────────────────────────────
// Magic constants (big-endian ASCII + version byte)
// ─────────────────────────────────────────────────────────────────────────────

/// Quote slot 헤더 magic = b"SHM_QT\\x00\\x02" (big-endian u64).
pub const QUOTE_MAGIC: u64 = u64::from_be_bytes(*b"SHM_QT\x00\x02");
/// Trade ring 헤더 magic = b"SHM_TRD\\x02".
pub const TRADE_MAGIC: u64 = u64::from_be_bytes(*b"SHM_TRD\x02");
/// Order ring 헤더 magic = b"SHM_ORD\\x02".
pub const ORDER_MAGIC: u64 = u64::from_be_bytes(*b"SHM_ORD\x02");
/// Symbol table 헤더 magic = b"SHM_SYM\\x02".
pub const SYMTAB_MAGIC: u64 = u64::from_be_bytes(*b"SHM_SYM\x02");

/// SHM layout protocol version. 모든 영역이 공유.
pub const SHM_VERSION: u32 = 2;

// ─────────────────────────────────────────────────────────────────────────────
// Quote slot
// ─────────────────────────────────────────────────────────────────────────────

/// Quote slot 헤더 (128B). writer 는 생성 시 1회만 기록, 이후 read-only.
#[repr(C, align(64))]
pub struct QuoteSlotHeader {
    /// [`QUOTE_MAGIC`].
    pub magic: u64,
    /// [`SHM_VERSION`].
    pub version: u32,
    /// slot 수 (capacity). power of 2 필수 아님 — 인덱스 범위 체크만.
    pub slot_count: u32,
    /// 개별 slot 바이트 (= [`FRAME_SIZE`]). 버전 검증용.
    pub element_size: u32,
    /// writer process id (디버그 / stale 판단용).
    pub writer_pid: u32,
    /// 생성 시각 ns (`CLOCK_REALTIME`).
    pub created_ns: u64,
    /// reserved to fill 128B.
    pub _reserved: [u8; 96],
}

const _: () = assert!(std::mem::size_of::<QuoteSlotHeader>() == 128);
const _: () = assert!(std::mem::align_of::<QuoteSlotHeader>() == 64);

/// Quote slot 한 칸 (128B). seqlock 패턴: `seq` 가 홀수면 writer 쓰는 중.
#[repr(C, align(64))]
pub struct QuoteSlot {
    /// seqlock sequence. even=stable, odd=writing. Acquire/Release.
    pub seq: AtomicU64,
    /// [`hft_types::ExchangeId`] as u8. Unknown 은 이론상 없음.
    pub exchange_id: u8,
    /// pad to align next field to 4B.
    pub _pad1: [u8; 3],
    /// symbol table index. u32 max.
    pub symbol_idx: u32,
    /// best bid price (Price::raw).
    pub bid_price: i64,
    /// best bid size (Size::raw).
    pub bid_size: i64,
    /// best ask price.
    pub ask_price: i64,
    /// best ask size.
    pub ask_size: i64,
    /// 거래소가 찍은 event time ns.
    pub event_ns: u64,
    /// WS 수신 시각 ns.
    pub recv_ns: u64,
    /// publish (SHM write 직전) 시각 ns.
    pub pub_ns: u64,
    /// flags (bit0 = `is_internal` 등). 향후 확장.
    pub flags: u32,
    /// 여유 pad.
    pub _pad2: [u8; 28],
}

const _: () = assert!(std::mem::size_of::<QuoteSlot>() == 128);
const _: () = assert!(std::mem::align_of::<QuoteSlot>() == 64);

/// Writer 가 슬롯에 기록할 페이로드 (atomic / seq 제외).
/// `publish()` 는 이를 받아서 seq 프로토콜을 직접 처리한다.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuoteUpdate {
    /// `hft_types::ExchangeId as u8`.
    pub exchange_id: u8,
    /// best bid price raw.
    pub bid_price: i64,
    /// best bid size raw.
    pub bid_size: i64,
    /// best ask price raw.
    pub ask_price: i64,
    /// best ask size raw.
    pub ask_size: i64,
    /// exchange event ns.
    pub event_ns: u64,
    /// WS recv ns.
    pub recv_ns: u64,
    /// 직전 publish ns.
    pub pub_ns: u64,
}

/// Reader 가 torn-read 없이 읽어간 스냅샷.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuoteSnapshot {
    /// slot 의 seq (짝수).
    pub seq: u64,
    /// exchange id.
    pub exchange_id: u8,
    /// symbol idx.
    pub symbol_idx: u32,
    /// bid px.
    pub bid_price: i64,
    /// bid sz.
    pub bid_size: i64,
    /// ask px.
    pub ask_price: i64,
    /// ask sz.
    pub ask_size: i64,
    /// event ns.
    pub event_ns: u64,
    /// recv ns.
    pub recv_ns: u64,
    /// pub ns.
    pub pub_ns: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Trade ring (SPMC)
// ─────────────────────────────────────────────────────────────────────────────

/// Trade ring 헤더 (128B). `writer_seq` 가 핫 atomic 이라 별 cacheline 으로 격리.
#[repr(C, align(64))]
pub struct TradeRingHeader {
    /// [`TRADE_MAGIC`].
    pub magic: u64,
    /// [`SHM_VERSION`].
    pub version: u32,
    /// `capacity - 1` (power-of-2-1). mask.
    pub capacity_mask: u32,
    /// element 바이트 = [`FRAME_SIZE`].
    pub element_size: u32,
    /// writer pid.
    pub writer_pid: u32,
    /// 생성 ns.
    pub created_ns: u64,
    /// pad up to 64B.
    pub _pad_a: [u8; 32],
    /// writer 가 다음에 쓸 seq (monotonic). reader 는 이걸 상한으로 사용.
    pub writer_seq: AtomicU64,
    /// 누적 drop (lap) 카운터. writer 만 증가.
    pub drops: AtomicU64,
    /// pad to 128B.
    pub _pad_b: [u8; 48],
}

const _: () = assert!(std::mem::size_of::<TradeRingHeader>() == 128);
const _: () = assert!(std::mem::align_of::<TradeRingHeader>() == 64);

/// Trade ring 한 프레임 (128B). `seq` 가 핫 atomic.
#[repr(C, align(64))]
pub struct TradeFrame {
    /// writer 가 기록한 seq. 0 = unused, `u64::MAX` = writer in-progress sentinel.
    pub seq: AtomicU64,
    /// exchange id.
    pub exchange_id: u8,
    /// pad.
    pub _pad1: [u8; 3],
    /// symbol idx.
    pub symbol_idx: u32,
    /// price raw.
    pub price: i64,
    /// size raw. negative = sell.
    pub size: i64,
    /// 거래소 tradeId (원문 숫자 우선, 비숫자면 ahash fallback).
    pub trade_id: i64,
    /// event ns.
    pub event_ns: u64,
    /// recv ns.
    pub recv_ns: u64,
    /// pub ns.
    pub pub_ns: u64,
    /// flags. bit0=is_internal (block trade 등).
    pub flags: u32,
    /// pad.
    pub _pad2: [u8; 28],
}

const _: () = assert!(std::mem::size_of::<TradeFrame>() == 128);
const _: () = assert!(std::mem::align_of::<TradeFrame>() == 64);

impl Default for TradeFrame {
    fn default() -> Self {
        Self {
            seq: AtomicU64::new(0),
            exchange_id: 0,
            _pad1: [0; 3],
            symbol_idx: 0,
            price: 0,
            size: 0,
            trade_id: 0,
            event_ns: 0,
            recv_ns: 0,
            pub_ns: 0,
            flags: 0,
            _pad2: [0; 28],
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Order ring (SPSC)
// ─────────────────────────────────────────────────────────────────────────────

/// Order ring 헤더 (192B). metadata / head / tail 을 각각 다른 cacheline 에 둔다.
#[repr(C, align(64))]
pub struct OrderRingHeader {
    /// [`ORDER_MAGIC`].
    pub magic: u64,
    /// version.
    pub version: u32,
    /// capacity - 1.
    pub capacity_mask: u32,
    /// element 바이트.
    pub element_size: u32,
    /// writer pid (Python).
    pub writer_pid: u32,
    /// 생성 ns.
    pub created_ns: u64,
    /// pad.
    pub _pad_a: [u8; 32],
    /// writer cursor (Python push).
    pub head: AtomicU64,
    /// pad between head and tail (false sharing 방지).
    pub _pad_b: [u8; 56],
    /// reader cursor (Rust consume).
    pub tail: AtomicU64,
    /// pad tail to 64B.
    pub _pad_c: [u8; 56],
}

// 64B metadata + 64B head lane + 64B tail lane.
const _: () = assert!(std::mem::size_of::<OrderRingHeader>() == 192);
const _: () = assert!(std::mem::align_of::<OrderRingHeader>() == 64);

/// Order 종류.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    /// new order place.
    Place = 0,
    /// cancel existing.
    Cancel = 1,
}

/// `OrderFrame.aux` 의 Place 분기 메타데이터.
///
/// `OrderFrame.kind` 가 [`OrderKind::Place`] 인 경우에만 이 레이아웃으로 해석한다.
/// Cancel 분기에서는 기존처럼 `exchange_order_id` ASCII payload 로 해석한다.
///
/// 바이트 레이아웃 (총 40B):
///
/// ```text
/// 0      1      2..34               34..40
/// +------+------+-------------------+---------+
/// | level| flags| text_tag[32]      | pad[6]  |
/// +------+------+-------------------+---------+
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceAuxMeta {
    /// 0 = Open, 1 = Close.
    pub level: u8,
    /// bit0 = reduce_only, bit1..7 reserved.
    pub flags: u8,
    /// UTF-8 태그 32B. NUL padding.
    pub text_tag: [u8; PLACE_AUX_TEXT_TAG_LEN],
    /// trailing reserved/padding.
    pub _pad: [u8; 6],
}

const _: () = assert!(std::mem::size_of::<PlaceAuxMeta>() == PLACE_AUX_META_SIZE);

impl Default for PlaceAuxMeta {
    fn default() -> Self {
        Self {
            level: PLACE_LEVEL_OPEN,
            flags: 0,
            text_tag: [0; PLACE_AUX_TEXT_TAG_LEN],
            _pad: [0; 6],
        }
    }
}

impl PlaceAuxMeta {
    /// Open/Close level code + reduce_only + text_tag 로 Place aux 메타를 만든다.
    ///
    /// `text_tag` 는 32B 이내 UTF-8 prefix 만 보존하고, 남는 바이트는 0 으로 채운다.
    pub fn from_parts(level: u8, reduce_only: bool, text_tag: &str) -> Self {
        let mut out = Self {
            level,
            flags: if reduce_only {
                PLACE_FLAG_REDUCE_ONLY
            } else {
                0
            },
            ..Self::default()
        };

        let mut written = 0usize;
        for ch in text_tag.chars() {
            let mut buf = [0u8; 4];
            let bytes = ch.encode_utf8(&mut buf).as_bytes();
            if written + bytes.len() > PLACE_AUX_TEXT_TAG_LEN {
                break;
            }
            out.text_tag[written..written + bytes.len()].copy_from_slice(bytes);
            written += bytes.len();
        }

        out
    }

    /// 현재 meta 를 little-endian `[u64; 5]` 로 packing 한다.
    pub fn pack(&self) -> [u64; 5] {
        let mut bytes = [0u8; ORDER_AUX_BYTES];
        bytes[0] = self.level;
        bytes[1] = self.flags & PLACE_FLAG_REDUCE_ONLY;
        bytes[2..34].copy_from_slice(&self.text_tag);

        let mut out = [0u64; 5];
        for (i, chunk) in bytes.chunks_exact(8).enumerate() {
            out[i] = u64::from_le_bytes(chunk.try_into().expect("8-byte chunk"));
        }
        out
    }

    /// little-endian `[u64; 5]` 를 Place 메타로 풀어낸다.
    pub fn unpack(aux: &[u64; 5]) -> Self {
        let mut bytes = [0u8; ORDER_AUX_BYTES];
        for (i, word) in aux.iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }

        let mut text_tag = [0u8; PLACE_AUX_TEXT_TAG_LEN];
        text_tag.copy_from_slice(&bytes[2..34]);
        let mut pad = [0u8; 6];
        pad.copy_from_slice(&bytes[34..40]);

        Self {
            level: bytes[0],
            flags: bytes[1],
            text_tag,
            _pad: pad,
        }
    }

    /// level code 를 그대로 돌려준다.
    #[inline]
    pub const fn wire_level_code(&self) -> u8 {
        self.level
    }

    /// reduce_only 비트를 해석한다.
    #[inline]
    pub const fn reduce_only(&self) -> bool {
        (self.flags & PLACE_FLAG_REDUCE_ONLY) != 0
    }

    /// NUL-trimmed UTF-8 태그 문자열.
    ///
    /// invalid UTF-8 인 경우 빈 문자열을 반환한다.
    pub fn text_tag_str(&self) -> &str {
        let len = self
            .text_tag
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(PLACE_AUX_TEXT_TAG_LEN);
        std::str::from_utf8(&self.text_tag[..len]).unwrap_or("")
    }
}

/// Order frame (128B). Python ctypes 도 이 레이아웃 공유.
#[repr(C, align(64))]
#[derive(Debug, Clone, Copy)]
pub struct OrderFrame {
    /// monotonic seq. 0 = empty (writer never writes 0).
    pub seq: u64,
    /// [`OrderKind`] as u8.
    pub kind: u8,
    /// exchange id.
    pub exchange_id: u8,
    /// pad.
    pub _pad1: [u8; 2],
    /// symbol idx.
    pub symbol_idx: u32,
    /// side: 0=Buy, 1=Sell.
    pub side: u8,
    /// tif: 0=GTC, 1=IOC, 2=FOK, 3=POSTONLY, 4=DAY.
    pub tif: u8,
    /// ord_type: 0=Limit, 1=Market.
    pub ord_type: u8,
    /// pad.
    pub _pad2: [u8; 1],
    /// price raw. Market 일 때 0.
    pub price: i64,
    /// size raw.
    pub size: i64,
    /// 내부 client_id. Python 이 발행.
    pub client_id: u64,
    /// Python timestamp ns.
    pub ts_ns: u64,
    /// 5 × u64 payload slot (cancel 대상 exchange_order_id 등).
    pub aux: [u64; 5],
    /// trailing pad up to 128B.
    pub _pad3: [u8; 16],
}

const _: () = assert!(std::mem::size_of::<OrderFrame>() == 128);
const _: () = assert!(std::mem::align_of::<OrderFrame>() == 64);

impl Default for OrderFrame {
    fn default() -> Self {
        Self {
            seq: 0,
            kind: OrderKind::Place as u8,
            exchange_id: 0,
            _pad1: [0; 2],
            symbol_idx: 0,
            side: 0,
            tif: 0,
            ord_type: 0,
            _pad2: [0; 1],
            price: 0,
            size: 0,
            client_id: 0,
            ts_ns: 0,
            aux: [0; 5],
            _pad3: [0; 16],
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Symbol table
// ─────────────────────────────────────────────────────────────────────────────

/// Symbol table 헤더 (64B).
#[repr(C, align(64))]
pub struct SymbolTableHeader {
    /// [`SYMTAB_MAGIC`].
    pub magic: u64,
    /// version.
    pub version: u32,
    /// 최대 symbol 수.
    pub capacity: u32,
    /// 현재 등록된 수 (단조 증가, append-only).
    pub count: AtomicU32,
    /// pad.
    pub _pad_a: [u8; 4],
    /// 생성 ns.
    pub created_ns: u64,
    /// 추가 여유.
    pub _pad_b: [u8; 32],
}

const _: () = assert!(std::mem::size_of::<SymbolTableHeader>() == 64);
const _: () = assert!(std::mem::align_of::<SymbolTableHeader>() == 64);

/// 한 symbol entry (64B).
#[repr(C, align(64))]
pub struct SymbolEntry {
    /// exchange id.
    pub exchange_id: u8,
    /// pad.
    pub _pad1: [u8; 3],
    /// name 바이트 길이 (<= [`SYMBOL_MAX_LEN`]).
    pub name_len: u32,
    /// name UTF-8 (null padding 포함).
    pub name: [u8; SYMBOL_MAX_LEN],
}

const _: () = assert!(std::mem::size_of::<SymbolEntry>() == 64);
const _: () = assert!(std::mem::align_of::<SymbolEntry>() == 64);

/// Symbol name 최대 바이트 (SymbolEntry 의 name 배열 크기).
pub const SYMBOL_MAX_LEN: usize = 56;

#[cfg(test)]
mod tests {
    use super::*;

    const DESIGN_PLACE_AUX_CLOSE_REDUCE_V6: [u8; ORDER_AUX_BYTES] = [
        0x01, 0x01, 0x76, 0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    fn aux_as_bytes(aux: &[u64; 5]) -> [u8; ORDER_AUX_BYTES] {
        let mut out = [0u8; ORDER_AUX_BYTES];
        for (i, word) in aux.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    #[test]
    fn place_aux_pack_unpack_roundtrip() {
        let cases = [
            PlaceAuxMeta::from_parts(PLACE_LEVEL_OPEN, false, ""),
            PlaceAuxMeta::from_parts(PLACE_LEVEL_OPEN, true, "v6"),
            PlaceAuxMeta::from_parts(PLACE_LEVEL_CLOSE, false, "v7_ma_cross"),
            PlaceAuxMeta::from_parts(PLACE_LEVEL_CLOSE, true, "OKX-close-long-tag"),
        ];

        for case in cases {
            let unpacked = PlaceAuxMeta::unpack(&case.pack());
            assert_eq!(unpacked.level, case.level);
            assert_eq!(unpacked.reduce_only(), case.reduce_only());
            assert_eq!(unpacked.text_tag_str(), case.text_tag_str());
            assert_eq!(unpacked._pad, [0; 6]);
            assert_eq!(unpacked.flags & !PLACE_FLAG_REDUCE_ONLY, 0);
        }
    }

    #[test]
    fn place_aux_design_invariance_close_reduce_v6() {
        let meta = PlaceAuxMeta::from_parts(PLACE_LEVEL_CLOSE, true, "v6");
        assert_eq!(aux_as_bytes(&meta.pack()), DESIGN_PLACE_AUX_CLOSE_REDUCE_V6);
        let unpacked = PlaceAuxMeta::unpack(&meta.pack());
        assert_eq!(unpacked.level, PLACE_LEVEL_CLOSE);
        assert!(unpacked.reduce_only());
        assert_eq!(unpacked.text_tag_str(), "v6");
    }

    #[test]
    fn place_aux_truncates_text_tag_at_char_boundary() {
        let long = "abcdefghijklmnopqrstuvwxyz0123456789";
        let meta = PlaceAuxMeta::from_parts(PLACE_LEVEL_OPEN, false, long);
        assert_eq!(meta.text_tag_str(), "abcdefghijklmnopqrstuvwxyz012345");
        assert_eq!(meta.text_tag[31], b'5');
    }

    #[test]
    fn place_aux_pack_zeroes_reserved_bits_and_pad() {
        let mut meta = PlaceAuxMeta::from_parts(PLACE_LEVEL_CLOSE, true, "v8");
        meta.flags = 0xff;
        meta._pad = [9; 6];
        let unpacked = PlaceAuxMeta::unpack(&meta.pack());
        assert_eq!(unpacked.flags & !PLACE_FLAG_REDUCE_ONLY, 0);
        assert_eq!(unpacked._pad, [0; 6]);
    }
}

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
    /// writer 가 기록한 seq. 0 = unused.
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

/// Order ring 헤더 (128B). head/tail 는 서로 다른 cacheline 에 둔다.
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

// 128B 가 아니라 256B 인 이유: head/tail 를 각각 별 cacheline 에.
const _: () = assert!(std::mem::size_of::<OrderRingHeader>() == 256);
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

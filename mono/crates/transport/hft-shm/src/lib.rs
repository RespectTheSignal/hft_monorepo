//! hft-shm — Intra-host shared memory transport for sub-μs latency.
//!
//! ## 개요
//!
//! 이 크레이트는 **동일 호스트 안의** publisher / subscriber / strategy /
//! order-gateway 사이에서 ZMQ TCP(12-30μs) 대신 `tmpfs` (`/dev/shm`) 기반
//! mmap 을 써서 **50-300ns 대역의 p50 지연**을 제공한다.
//!
//! 세 개의 영역으로 나뉜다:
//!
//! 1. **Quote slot** ([`quote_slot`]) — BookTicker (best bid/ask) 의 *최신값만*
//!    symbol 별 slot 에 seqlock 으로 덮어쓴다. History 없음.
//! 2. **Trade ring** ([`spmc_ring`]) — Trade 이벤트의 SPMC broadcast ring.
//!    writer 1개, reader N개. writer 는 reader 를 절대 기다리지 않으며,
//!    lap 감지되면 drop counter 만 증가.
//! 3. **Order ring** ([`spsc_ring`]) — Python strategy → Rust order-gateway 주문
//!    전달을 위한 SPSC Lamport queue.
//!
//! 보조로 [`symbol_table`] 이 `(ExchangeId, symbol)` → `symbol_idx: u32` 매핑을
//! 제공해 핫패스에서 문자열 비교를 제거한다.
//!
//! ## 설계 원칙
//!
//! - **Zero syscall on hot path**: 쓰기/읽기 모두 atomic load/store 만. wakeup 은
//!   busy-poll 또는 외부 eventfd.
//! - **Cache-line (64B) aware**: 모든 핫 atomic 필드는 64B 로 padding 분리.
//! - **Huge page first**: 가능한 영역은 2MB huge page 로 매핑해 TLB miss 감소.
//! - **Legacy parity**: Price/Size 는 `hft-types::{Price,Size}::raw()` 의 i64
//!   scaled 표현을 그대로 Copy — serializer 가 필요 없음.
//! - **No `unsafe` leaks across API**: 내부는 raw pointer 를 쓰지만 공개 API 는
//!   `&self` / `&mut self` 로 래핑되어 있고 `Send`/`Sync` 는 문서화된 invariant
//!   (writer 가 유일) 를 지키는 한에서만 구현.
//!
//! ## 공개 API 요약
//!
//! ```no_run
//! use hft_shm::*;
//! # fn main() -> anyhow::Result<()> {
//! // quote slot
//! let q_w = QuoteSlotWriter::create("/dev/shm/hft_quotes_v2", 10_000)?;
//! q_w.publish(42, &QuoteUpdate { bid_price: 0, bid_size: 0, ask_price: 0, ask_size: 0,
//!                                event_ns: 0, recv_ns: 0, pub_ns: 0, exchange_id: 1 });
//! let q_r = QuoteSlotReader::open("/dev/shm/hft_quotes_v2")?;
//! let _snap = q_r.read(42);
//!
//! // trade ring
//! let t_w = TradeRingWriter::create("/dev/shm/hft_trades_v2", 1 << 20)?;
//! t_w.publish(&TradeFrame::default());
//! let mut t_r = TradeRingReader::open("/dev/shm/hft_trades_v2")?;
//! while let Some(_f) = t_r.try_consume() {}
//!
//! // order ring
//! let o_w = OrderRingWriter::create("/dev/shm/hft_orders_v2", 16_384)?;
//! o_w.publish(&OrderFrame::default());
//! let mut o_r = OrderRingReader::open("/dev/shm/hft_orders_v2")?;
//! while let Some(_o) = o_r.try_consume() {}
//!
//! // symbol table
//! let sym = SymbolTable::open_or_create("/dev/shm/hft_symtab_v2", 16_384)?;
//! let _idx = sym.get_or_intern(hft_types::ExchangeId::Gate, "BTC_USDT")?;
//! # Ok(()) }
//! ```
//!
//! ## Multi-VM 레이어 (v2)
//!
//! [`region`] 모듈은 위 세 원시 ring 들을 **하나의 파일/PCI BAR 안에서
//! 오프셋으로** 묶어 주는 상위 추상이다. ivshmem 기반 multi-VM 토폴로지에서
//! publisher 1개 + gateway 1개 + strategy N개가 **한 장의 shared file** 위에서
//! 각자의 sub-region 을 마운트하도록 한다. [`MultiOrderRingReader`] 는
//! gateway 쪽 N×SPSC 팬-인 구현이다.
//!
//! ```no_run
//! use hft_shm::*;
//! # fn main() -> anyhow::Result<()> {
//! let spec = LayoutSpec { quote_slot_count: 10_000, trade_ring_capacity: 1 << 20,
//!                         symtab_capacity: 16_384, order_ring_capacity: 16_384,
//!                         n_max: 16 };
//! let sr = SharedRegion::create_or_attach(
//!     Backing::DevShm { path: "/dev/shm/hft_v2".into() }, spec, Role::Publisher)?;
//! let _q = QuoteSlotWriter::from_region(sr.sub_region(SubKind::Quote)?, spec.quote_slot_count)?;
//! # Ok(()) }
//! ```
//!
//! 자세한 설계는 `docs/architecture/SHM_DESIGN.md` 와 `docs/architecture/MULTI_VM_TOPOLOGY.md`,
//! 그리고 `docs/adr/ADR-0003-shm-multi-vm.md` 참고.

#![deny(rust_2018_idioms)]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod error;
pub mod layout;
pub mod mmap;
pub mod symbol_table;
pub mod quote_slot;
pub mod spmc_ring;
pub mod spsc_ring;
pub mod region;
pub mod multi_order_ring;

/// CLOCK_REALTIME ns (wall clock). 헤더의 `created_ns` 에 사용.
pub(crate) fn now_realtime_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub use error::{ShmError, ShmResult};
pub use layout::{
    OrderFrame, OrderKind, QuoteSlot, QuoteSnapshot, QuoteUpdate, TradeFrame, CACHE_LINE,
    FRAME_SIZE,
};
pub use quote_slot::{QuoteSlotReader, QuoteSlotWriter};
pub use spmc_ring::{TradeRingReader, TradeRingWriter};
pub use spsc_ring::{OrderRingReader, OrderRingWriter};
pub use symbol_table::{exchange_from_u8, exchange_to_u8, SymbolTable, SymbolTableStats};
pub use region::{
    layout_digest, Backing, LayoutOffsets, LayoutSpec, RegionHeader, Role, SharedRegion, SubKind,
    MAX_ORDER_RINGS, SHARED_REGION_MAGIC, SUBREGION_ALIGN,
};
pub use multi_order_ring::MultiOrderRingReader;

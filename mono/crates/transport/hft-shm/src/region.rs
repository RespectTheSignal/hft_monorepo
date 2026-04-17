//! 단일 공유 region 안의 다영역 레이아웃 관리.
//!
//! Multi-VM 토폴로지 (`MULTI_VM_TOPOLOGY.md`) 의 **핵심 중간층**. 이 모듈은:
//!
//! 1. 한 개의 mmap 파일 (hugetlbfs / dev_shm / ivshmem PCI BAR) 을 열고,
//! 2. 그 안에 [`RegionHeader`] 를 작성 / 검증한 뒤,
//! 3. 각 sub-region (quote slots, trade ring, symbol table, N_MAX 개의 order
//!    ring) 을 [`ShmRegion::sub_view`] 로 잘라 개별 writer/reader 크레이트에
//!    전달한다.
//!
//! ## 레이아웃
//!
//! ```text
//! +--------------------- offset 0 --------------------+
//! | RegionHeader (4KB aligned)                         |
//! +--------------------- offset Q --------------------+
//! | QuoteSlotHeader + QuoteSlot × slot_count           |
//! +--------------------- offset T --------------------+
//! | TradeRingHeader + TradeFrame × trade_cap           |
//! +--------------------- offset S --------------------+
//! | SymbolTableHeader + SymbolEntry × symtab_cap       |
//! +--------------------- offset O --------------------+
//! | OrderRingHeader + OrderFrame × order_cap  (ring 0) |
//! +----------------------------------------------------+
//! | OrderRingHeader + OrderFrame × order_cap  (ring 1) |
//! +----------------------------------------------------+
//! |  ...                                               |
//! +--------------------- offset O + N×S ---------------+
//! ```
//!
//! 모든 sub-region 시작점은 **4KB (PAGE_SIZE) 로 정렬**. 이는:
//! - hugepage 경계가 pod 단위로 맞도록,
//! - 각 sub-region 에 대해 reader/writer 쪽에서 `as_ptr()` 이 해당 헤더 타입의
//!   align (64B) 을 충분히 만족하도록,
//! - 디버깅 시 offset 을 눈으로 읽기 쉽도록.
//!
//! ## Wire digest
//!
//! SharedRegion 을 만드는 publisher/infra 는 [`LayoutSpec`] 으로부터 SHA-256
//! 기반 [`layout_digest`] 를 계산해 헤더에 기록한다. 읽는 쪽은 자신의 spec 으로
//! 동일하게 계산한 digest 와 비교해 일치할 때만 진행. 한 바이트라도 달라지면
//! 즉시 [`ShmError::LayoutMismatch`] — 이 인프라는 **rolling upgrade 불가**,
//! layout 변경 = 전체 infra reboot 이벤트.
//!
//! ## 동시성
//!
//! - RegionHeader 는 publisher 가 최초 1회 기록, 이후 reader 들은 read-only.
//!   publisher 재시작 시에도 magic/digest 가 같으면 writer_pid 만 갱신.
//! - 각 sub-region 은 기존 writer/reader 규약을 그대로 따름 (seqlock, SPMC,
//!   SPSC). 경계 침범은 sub_view 가 물리적으로 차단.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tracing::{debug, info};

use crate::error::{ShmError, ShmResult};
use crate::layout::{
    OrderFrame, OrderRingHeader, QuoteSlot, QuoteSlotHeader, SymbolEntry, SymbolTableHeader,
    TradeFrame, TradeRingHeader, SHM_VERSION,
};
use crate::mmap::ShmRegion;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// SharedRegion 헤더 magic = "HFTSHMV2" (big-endian ASCII).
pub const SHARED_REGION_MAGIC: u64 = u64::from_be_bytes(*b"HFTSHMV2");

/// 모든 sub-region 시작 바이트 정렬 (4KB). hugepage / PAGE_SIZE 경계에 안전.
pub const SUBREGION_ALIGN: usize = 4096;

/// N_MAX 상한. 2026-04-15 기준 운영 기대값 64 (넉넉히).
pub const MAX_ORDER_RINGS: u32 = 256;

// ─────────────────────────────────────────────────────────────────────────────
// Backing
// ─────────────────────────────────────────────────────────────────────────────

/// SharedRegion 이 기대하는 mmap 백엔드 종류.
#[derive(Debug, Clone)]
pub enum Backing {
    /// host 측 tmpfs (`/dev/shm/..`). Phase-2 Track-C 와 호환. 단일 호스트 전용.
    DevShm {
        /// 파일 경로 (`/dev/shm/<name>`).
        path: PathBuf,
    },
    /// host 측 hugetlbfs (`/dev/hugepages/..`). ivshmem backing 에 사용.
    Hugetlbfs {
        /// 파일 경로 (`/dev/hugepages/<name>`).
        path: PathBuf,
    },
    /// guest VM 측 PCI BAR. 크기 ftruncate 불가.
    PciBar {
        /// `/sys/bus/pci/devices/<bdf>/resource2`.
        path: PathBuf,
    },
}

impl Backing {
    /// 공통 접근자.
    pub fn path(&self) -> &Path {
        match self {
            Backing::DevShm { path } | Backing::Hugetlbfs { path } | Backing::PciBar { path } => {
                path.as_path()
            }
        }
    }

    /// Device (PCI BAR) 인가? — writer 가 될 수 없고, 크기 확장 불가.
    pub fn is_device(&self) -> bool {
        matches!(self, Backing::PciBar { .. })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Role
// ─────────────────────────────────────────────────────────────────────────────

/// SharedRegion 을 여는 호출자의 역할.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Publisher — MD/Symbol writer, order ring reader 권한 없음.
    Publisher,
    /// Order gateway — N 개 order ring reader + MD reader.
    OrderGateway,
    /// Strategy VM — 자기 vm_id 의 order ring writer + MD reader.
    Strategy {
        /// [0, N_MAX) 범위의 VM 인덱스.
        vm_id: u32,
    },
    /// Read-only (모니터링 / 테스트). 아무 것도 쓰지 않음.
    ReadOnly,
}

impl Role {
    /// 이 역할이 **전체 region 레이아웃을 생성/초기화** 할 수 있는지.
    pub fn is_writer(&self) -> bool {
        matches!(self, Role::Publisher)
    }

    /// 헤더에 남기는 진단용 역할 코드.
    pub const fn header_code(self) -> u32 {
        match self {
            Role::Publisher => 1,
            Role::OrderGateway => 2,
            Role::Strategy { .. } => 3,
            Role::ReadOnly => 4,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LayoutSpec
// ─────────────────────────────────────────────────────────────────────────────

/// SharedRegion 의 **불변 레이아웃 파라미터**. Publisher 가 생성 시 제공하고,
/// 다른 VM 들은 자기 spec 으로 digest 를 재계산해 일치 여부를 검증한다.
///
/// 필드 순서 / 의미는 wire 의 일부 — 한 번 배포한 값은 digest 가 다른 한 못 바꾼다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutSpec {
    /// Quote slot 개수. 보통 10,000 (전 거래소 × 전 symbol 여유 포함).
    pub quote_slot_count: u32,
    /// Trade ring capacity (power-of-two). 예: 1 << 20.
    pub trade_ring_capacity: u64,
    /// Symbol table capacity. 예: 16_384.
    pub symtab_capacity: u32,
    /// 각 order ring 의 capacity (power-of-two). 예: 16_384.
    pub order_ring_capacity: u64,
    /// Order ring 개수 = 전략 VM 수 상한. boot 시 고정. 1..=MAX_ORDER_RINGS.
    pub n_max: u32,
}

impl LayoutSpec {
    /// 간단한 sanity 체크.
    pub fn validate(&self) -> ShmResult<()> {
        if self.quote_slot_count == 0 {
            return Err(ShmError::InvalidCapacity(0));
        }
        if self.trade_ring_capacity == 0 || !self.trade_ring_capacity.is_power_of_two() {
            return Err(ShmError::InvalidCapacity(self.trade_ring_capacity as usize));
        }
        if self.order_ring_capacity == 0 || !self.order_ring_capacity.is_power_of_two() {
            return Err(ShmError::InvalidCapacity(self.order_ring_capacity as usize));
        }
        if self.symtab_capacity == 0 {
            return Err(ShmError::InvalidCapacity(0));
        }
        if self.n_max == 0 || self.n_max > MAX_ORDER_RINGS {
            return Err(ShmError::Other(format!(
                "n_max {} out of [1, {}]",
                self.n_max, MAX_ORDER_RINGS
            )));
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// RegionHeader — 단일 공유 파일의 최상위 메타데이터 (4KB reserved)
// ─────────────────────────────────────────────────────────────────────────────

/// SharedRegion 헤더. 실제 저장 바이트는 [`RegionHeader`] 이후 4KB 까지 reserved.
#[repr(C, align(64))]
pub struct RegionHeader {
    /// [`SHARED_REGION_MAGIC`].
    pub magic: u64,
    /// [`SHM_VERSION`].
    pub version: u32,
    /// 초기 role (publisher pid 가 기록). 진단용.
    pub writer_role: u32,
    /// 초기화 시점의 publisher PID.
    pub writer_pid: AtomicU32,
    /// pad to 24B boundary.
    pub _pad_a: [u8; 4],
    /// 생성 시각 ns (`CLOCK_REALTIME`).
    pub created_ns: u64,

    /// Publisher liveness heartbeat — wall-clock ns. Publisher 가 publish 마다
    /// `touch_heartbeat` 로 store. Gateway/모니터가 `heartbeat_age_ns` 로
    /// stale 여부를 본다. `MULTI_VM_TOPOLOGY.md §6` "5초 정체 시 alert".
    pub pub_heartbeat_ns: AtomicU64,
    /// Monotonic heartbeat tick — touch 마다 +1. wall-clock 역주행 / drift 에
    /// 면역. `heartbeat_ns` 보조 채널.
    pub pub_seq: AtomicU64,

    // Spec 필드 (layout_digest 에 모두 포함).
    /// Quote slot 개수.
    pub quote_slot_count: u32,
    /// Trade ring capacity.
    pub trade_ring_capacity: u64,
    /// Symbol table capacity.
    pub symtab_capacity: u32,
    /// Order ring capacity.
    pub order_ring_capacity: u64,
    /// Order ring 개수 (N_MAX).
    pub n_max: u32,
    /// pad.
    pub _pad_b: [u8; 4],

    // 오프셋 — 재계산 가능하지만 명시적으로 저장해 디버깅 용이성 ↑.
    /// Quote sub-region 시작 오프셋 (bytes).
    pub offset_quote: u64,
    /// Trade sub-region 시작 오프셋.
    pub offset_trade: u64,
    /// Symbol table sub-region 시작 오프셋.
    pub offset_symtab: u64,
    /// 첫 번째 order ring 시작 오프셋.
    pub offset_orders_base: u64,
    /// Order ring 1개당 stride (bytes).
    pub order_ring_stride: u64,

    /// 총 region 바이트.
    pub total_bytes: u64,

    /// SHA-256 wire digest — 위 모든 숫자 + 개별 구조체 크기의 해시.
    pub layout_digest: [u8; 32],

    /// 나머지를 0 패딩으로 채워 헤더를 정확히 4KB 로 만든다.
    /// heartbeat 필드 2개 (pub_heartbeat_ns + pub_seq = 16B) 추가로 -16.
    pub _reserved: [u8; 3920],
}

const _: () = assert!(std::mem::size_of::<RegionHeader>() == SUBREGION_ALIGN);
const _: () = assert!(std::mem::align_of::<RegionHeader>() == 64);

// ─────────────────────────────────────────────────────────────────────────────
// Offset / size 계산
// ─────────────────────────────────────────────────────────────────────────────

/// SUBREGION_ALIGN 의 배수로 올림.
fn align_up(v: usize) -> usize {
    (v + SUBREGION_ALIGN - 1) & !(SUBREGION_ALIGN - 1)
}

/// sub-region 바이트 계산 결과.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutOffsets {
    /// quote sub-region 시작.
    pub offset_quote: usize,
    /// quote sub-region 바이트 (aligned up).
    pub size_quote: usize,
    /// trade sub-region 시작.
    pub offset_trade: usize,
    /// trade sub-region 바이트 (aligned up).
    pub size_trade: usize,
    /// symtab sub-region 시작.
    pub offset_symtab: usize,
    /// symtab sub-region 바이트 (aligned up).
    pub size_symtab: usize,
    /// 첫 번째 order ring 시작.
    pub offset_orders_base: usize,
    /// order ring 1개당 stride (aligned up).
    pub order_ring_stride: usize,
    /// 전체 region 총 바이트.
    pub total_bytes: usize,
}

impl LayoutOffsets {
    /// `spec` 으로부터 결정적으로 offset/size 계산.
    pub fn compute(spec: &LayoutSpec) -> ShmResult<Self> {
        spec.validate()?;
        let header_size = SUBREGION_ALIGN; // 헤더는 4KB 로 고정.

        // Quote sub-region: QuoteSlotHeader + N × QuoteSlot, 4KB align.
        let quote_raw = std::mem::size_of::<QuoteSlotHeader>()
            .checked_add(
                (spec.quote_slot_count as usize)
                    .checked_mul(std::mem::size_of::<QuoteSlot>())
                    .ok_or(ShmError::SizeOverflow {
                        capacity: spec.quote_slot_count as usize,
                        element_size: std::mem::size_of::<QuoteSlot>(),
                    })?,
            )
            .ok_or(ShmError::SizeOverflow {
                capacity: spec.quote_slot_count as usize,
                element_size: std::mem::size_of::<QuoteSlot>(),
            })?;
        let size_quote = align_up(quote_raw);

        // Trade sub-region.
        let trade_raw = std::mem::size_of::<TradeRingHeader>()
            .checked_add(
                (spec.trade_ring_capacity as usize)
                    .checked_mul(std::mem::size_of::<TradeFrame>())
                    .ok_or(ShmError::SizeOverflow {
                        capacity: spec.trade_ring_capacity as usize,
                        element_size: std::mem::size_of::<TradeFrame>(),
                    })?,
            )
            .ok_or(ShmError::SizeOverflow {
                capacity: spec.trade_ring_capacity as usize,
                element_size: std::mem::size_of::<TradeFrame>(),
            })?;
        let size_trade = align_up(trade_raw);

        // Symbol table.
        let symtab_raw = std::mem::size_of::<SymbolTableHeader>()
            .checked_add(
                (spec.symtab_capacity as usize)
                    .checked_mul(std::mem::size_of::<SymbolEntry>())
                    .ok_or(ShmError::SizeOverflow {
                        capacity: spec.symtab_capacity as usize,
                        element_size: std::mem::size_of::<SymbolEntry>(),
                    })?,
            )
            .ok_or(ShmError::SizeOverflow {
                capacity: spec.symtab_capacity as usize,
                element_size: std::mem::size_of::<SymbolEntry>(),
            })?;
        let size_symtab = align_up(symtab_raw);

        // 각 order ring 크기 (stride).
        let order_raw = std::mem::size_of::<OrderRingHeader>()
            .checked_add(
                (spec.order_ring_capacity as usize)
                    .checked_mul(std::mem::size_of::<OrderFrame>())
                    .ok_or(ShmError::SizeOverflow {
                        capacity: spec.order_ring_capacity as usize,
                        element_size: std::mem::size_of::<OrderFrame>(),
                    })?,
            )
            .ok_or(ShmError::SizeOverflow {
                capacity: spec.order_ring_capacity as usize,
                element_size: std::mem::size_of::<OrderFrame>(),
            })?;
        let order_ring_stride = align_up(order_raw);

        // offsets.
        let offset_quote = header_size;
        let offset_trade = offset_quote
            .checked_add(size_quote)
            .ok_or_else(|| ShmError::Other("offset overflow at trade".into()))?;
        let offset_symtab = offset_trade
            .checked_add(size_trade)
            .ok_or_else(|| ShmError::Other("offset overflow at symtab".into()))?;
        let offset_orders_base = offset_symtab
            .checked_add(size_symtab)
            .ok_or_else(|| ShmError::Other("offset overflow at orders base".into()))?;
        let orders_total = order_ring_stride
            .checked_mul(spec.n_max as usize)
            .ok_or_else(|| ShmError::Other("orders total overflow".into()))?;
        let total_bytes = offset_orders_base
            .checked_add(orders_total)
            .ok_or_else(|| ShmError::Other("total region overflow".into()))?;

        Ok(Self {
            offset_quote,
            size_quote,
            offset_trade,
            size_trade,
            offset_symtab,
            size_symtab,
            offset_orders_base,
            order_ring_stride,
            total_bytes,
        })
    }

    /// `vm_id` 번째 order ring 의 (offset, len).
    pub fn order_ring_range(&self, vm_id: u32, n_max: u32) -> ShmResult<(usize, usize)> {
        if vm_id >= n_max {
            return Err(ShmError::Other(format!(
                "vm_id {} >= n_max {}",
                vm_id, n_max
            )));
        }
        let offset = self
            .offset_orders_base
            .checked_add(
                (vm_id as usize)
                    .checked_mul(self.order_ring_stride)
                    .ok_or_else(|| ShmError::Other("order ring offset overflow".into()))?,
            )
            .ok_or_else(|| ShmError::Other("order ring offset overflow".into()))?;
        Ok((offset, self.order_ring_stride))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Layout digest — SHA-256 over all wire-relevant parameters
// ─────────────────────────────────────────────────────────────────────────────

/// `spec` 과 모든 sub-region struct 크기를 결정적으로 해시해 32B 반환.
///
/// 규칙: 여기 들어간 바이트가 하나라도 바뀌면 **wire 호환이 깨진 것**.
/// 구조체 필드 추가/삭제는 반드시 struct size 를 통해 digest 에 반영되어야 하며,
/// 필드 재배열은 size 가 같아도 해시가 동일할 수 있으므로 별도 assertion 으로
/// 커버한다 (layout.rs 의 `const _: () = assert!(size == ..)` 가 그 역할).
pub fn layout_digest(spec: &LayoutSpec, offsets: &LayoutOffsets) -> [u8; 32] {
    let mut h = Sha256::new();
    // 1. 고정 상수.
    h.update(SHARED_REGION_MAGIC.to_le_bytes());
    h.update(SHM_VERSION.to_le_bytes());
    // 2. Spec.
    h.update(spec.quote_slot_count.to_le_bytes());
    h.update(spec.trade_ring_capacity.to_le_bytes());
    h.update(spec.symtab_capacity.to_le_bytes());
    h.update(spec.order_ring_capacity.to_le_bytes());
    h.update(spec.n_max.to_le_bytes());
    // 3. Offsets — 구조체 사이즈와 alignment 정책의 함수라 자동 반영되지만,
    //    publisher/reader 간 버그로 계산이 달라지는 경우를 빨리 잡기 위해 포함.
    h.update((offsets.offset_quote as u64).to_le_bytes());
    h.update((offsets.size_quote as u64).to_le_bytes());
    h.update((offsets.offset_trade as u64).to_le_bytes());
    h.update((offsets.size_trade as u64).to_le_bytes());
    h.update((offsets.offset_symtab as u64).to_le_bytes());
    h.update((offsets.size_symtab as u64).to_le_bytes());
    h.update((offsets.offset_orders_base as u64).to_le_bytes());
    h.update((offsets.order_ring_stride as u64).to_le_bytes());
    h.update((offsets.total_bytes as u64).to_le_bytes());
    // 4. Struct sizes — 레이아웃이 조용히 바뀌었을 때 digest 에서 즉시 드러나게.
    h.update((std::mem::size_of::<RegionHeader>() as u64).to_le_bytes());
    h.update((std::mem::size_of::<QuoteSlotHeader>() as u64).to_le_bytes());
    h.update((std::mem::size_of::<QuoteSlot>() as u64).to_le_bytes());
    h.update((std::mem::size_of::<TradeRingHeader>() as u64).to_le_bytes());
    h.update((std::mem::size_of::<TradeFrame>() as u64).to_le_bytes());
    h.update((std::mem::size_of::<SymbolTableHeader>() as u64).to_le_bytes());
    h.update((std::mem::size_of::<SymbolEntry>() as u64).to_le_bytes());
    h.update((std::mem::size_of::<OrderRingHeader>() as u64).to_le_bytes());
    h.update((std::mem::size_of::<OrderFrame>() as u64).to_le_bytes());
    let out = h.finalize();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out);
    buf
}

// ─────────────────────────────────────────────────────────────────────────────
// Kind — 어느 sub-region 을 요청하는가
// ─────────────────────────────────────────────────────────────────────────────

/// SharedRegion 이 내어주는 sub-region 지정자.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubKind {
    /// Quote slots (SPMC seqlock).
    Quote,
    /// Trade ring (SPMC broadcast).
    Trade,
    /// Symbol table (append-only).
    Symtab,
    /// `vm_id` 번째 order ring (SPSC).
    OrderRing {
        /// 대상 VM.
        vm_id: u32,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// SharedRegion
// ─────────────────────────────────────────────────────────────────────────────

/// 단일 파일 전체를 잡고 있는 상위 region 핸들.
///
/// Publisher 는 [`SharedRegion::create_or_attach`] 로 생성 (필요 시 ftruncate +
/// header 초기화), Strategy / OrderGateway 는 [`SharedRegion::open_view`] 로
/// 기존 매핑에 연결 + digest 검증.
pub struct SharedRegion {
    /// 전체 파일을 잡은 최상위 region (Arc 로 공유). 모든 sub-region 은 이 위에
    /// 만들어진 view.
    parent: Arc<ShmRegion>,
    /// 사용자가 요청한 spec (publisher 측) 또는 header 에서 읽은 spec (reader 측).
    spec: LayoutSpec,
    /// 계산된 offsets.
    offsets: LayoutOffsets,
    /// 최초 생성 여부 (publisher 가 header 를 쓴 경우 true).
    was_created: bool,
    /// 이 핸들이 수행하는 역할.
    role: Role,
}

impl SharedRegion {
    /// Publisher (또는 테스트) 가 생성. device backing 은 쓰기 불가.
    pub fn create_or_attach(backing: Backing, spec: LayoutSpec, role: Role) -> ShmResult<Self> {
        spec.validate()?;
        if backing.is_device() {
            return Err(ShmError::Unsupported(
                "cannot create_or_attach on device backing (PCI BAR) — use open_view",
            ));
        }
        let offsets = LayoutOffsets::compute(&spec)?;
        let digest = layout_digest(&spec, &offsets);
        let region = ShmRegion::create_or_attach(backing.path(), offsets.total_bytes, true)?;

        // Header 초기화 또는 검증.
        let was_created = unsafe { init_or_validate_header(&region, &spec, &offsets, &digest)? };
        if was_created {
            info!(
                path = %backing.path().display(),
                total = offsets.total_bytes,
                n_max = spec.n_max,
                "SharedRegion: fresh layout initialized"
            );
        } else {
            info!(
                path = %backing.path().display(),
                total = offsets.total_bytes,
                "SharedRegion: attached to existing layout (digest verified)"
            );
        }
        Ok(Self {
            parent: Arc::new(region),
            spec,
            offsets,
            was_created,
            role,
        })
    }

    /// Reader (Strategy / OrderGateway / ReadOnly) 가 기존 region 에 연결.
    ///
    /// `expected_spec` 을 제공해 digest 를 비교한다. 일치하지 않으면 에러.
    pub fn open_view(backing: Backing, expected_spec: LayoutSpec, role: Role) -> ShmResult<Self> {
        expected_spec.validate()?;
        let offsets = LayoutOffsets::compute(&expected_spec)?;
        let expected_digest = layout_digest(&expected_spec, &offsets);

        let region = match &backing {
            Backing::DevShm { path } | Backing::Hugetlbfs { path } => {
                ShmRegion::attach_existing(path)?
            }
            Backing::PciBar { path } => ShmRegion::open_device(path)?,
        };
        if region.len() < offsets.total_bytes {
            return Err(ShmError::Other(format!(
                "region at {:?} is {} bytes; expected at least {}",
                backing.path(),
                region.len(),
                offsets.total_bytes
            )));
        }
        // 헤더 검증.
        unsafe { validate_header(&region, &expected_spec, &offsets, &expected_digest)? };
        debug!(
            path = %backing.path().display(),
            role = ?role,
            "SharedRegion opened as viewer"
        );
        Ok(Self {
            parent: Arc::new(region),
            spec: expected_spec,
            offsets,
            was_created: false,
            role,
        })
    }

    /// 이 프로세스가 header 를 방금 초기화했는지.
    pub fn was_created(&self) -> bool {
        self.was_created
    }

    /// 현재 spec.
    pub fn spec(&self) -> &LayoutSpec {
        &self.spec
    }

    /// offsets.
    pub fn offsets(&self) -> &LayoutOffsets {
        &self.offsets
    }

    /// role.
    pub fn role(&self) -> Role {
        self.role
    }

    /// parent region Arc — 테스트 / 진단용.
    pub fn parent(&self) -> &Arc<ShmRegion> {
        &self.parent
    }

    /// 총 region 바이트.
    pub fn total_bytes(&self) -> usize {
        self.parent.len()
    }

    /// RegionHeader 에 대한 read-only 참조. 내부 atomic 필드는 `&self` 로 접근 가능.
    ///
    /// # 안전
    /// Publisher 의 최초 1회 write 이후 헤더는 불변이며 atomic 필드만 갱신된다.
    /// 따라서 `&RegionHeader` 를 노출해도 data race 가 없다.
    fn header(&self) -> &RegionHeader {
        // SAFETY: region 은 total_bytes >= size_of::<RegionHeader>() 를 만족하도록
        // create/attach 에서 검증됨. 헤더는 4KB 로 정렬되고 parent.as_ptr() 는
        // page 경계.
        unsafe { &*(self.parent.as_ptr() as *const RegionHeader) }
    }

    /// Publisher 가 호출 — 현재 wall-clock `now_ns` 로 heartbeat 를 갱신하고
    /// seq 를 +1. publisher 가 아닌 role 에서 호출 시 debug log + 무시.
    ///
    /// `&self` 만 필요 (atomic store). 비용: u64 store × 2 ≈ 3ns.
    pub fn touch_heartbeat(&self, now_ns: u64) {
        if !matches!(self.role, Role::Publisher) {
            // Tight hot path 호출자도 있을 수 있어 warn 대신 debug.
            debug!(role = ?self.role, "touch_heartbeat from non-publisher role — ignored");
            return;
        }
        let hdr = self.header();
        // Relaxed 면 충분 — 관측자 쪽에서 wall-clock 을 "대략" 읽으면 됨.
        // pub_seq 를 Release 로 올려 stale-reader 에게 순서 보장 (seq 증가 ⇒
        // 그 이전의 heartbeat_ns store 는 이미 완료).
        hdr.pub_heartbeat_ns.store(now_ns, Ordering::Relaxed);
        hdr.pub_seq.fetch_add(1, Ordering::Release);
    }

    /// 마지막 heartbeat 의 wall-clock ns 를 읽는다. publisher 가 아직 touch 안
    /// 했으면 0.
    pub fn heartbeat_ns(&self) -> u64 {
        // Acquire 로 pub_seq 를 먼저 읽어 해당 시점의 ns 를 가져오는 순서 보장.
        // 단, writer 가 ns 를 먼저 store 하고 seq 를 +1 하므로, seq 를 보고
        // ns 를 읽으면 항상 그 seq 에 대응하는 또는 그 이후의 ns 를 얻는다.
        let _seq = self.header().pub_seq.load(Ordering::Acquire);
        self.header().pub_heartbeat_ns.load(Ordering::Relaxed)
    }

    /// 마지막 heartbeat tick (단조 증가). wall-clock 역주행에 면역.
    pub fn heartbeat_seq(&self) -> u64 {
        self.header().pub_seq.load(Ordering::Acquire)
    }

    /// `now_ns` 기준 heartbeat 경과 ns. publisher 가 아직 touch 안 했거나
    /// `now_ns <= heartbeat_ns` 이면 `None`.
    pub fn heartbeat_age_ns(&self, now_ns: u64) -> Option<u64> {
        let last = self.heartbeat_ns();
        if last == 0 {
            return None;
        }
        now_ns.checked_sub(last)
    }

    /// sub-region 을 view 로 잘라 반환. 각 writer/reader 크레이트가 받아서 자체
    /// 초기화/검증 로직을 돌린다.
    pub fn sub_region(&self, kind: SubKind) -> ShmResult<ShmRegion> {
        let (offset, len) = match kind {
            SubKind::Quote => (self.offsets.offset_quote, self.offsets.size_quote),
            SubKind::Trade => (self.offsets.offset_trade, self.offsets.size_trade),
            SubKind::Symtab => (self.offsets.offset_symtab, self.offsets.size_symtab),
            SubKind::OrderRing { vm_id } => {
                if vm_id >= self.spec.n_max {
                    return Err(ShmError::Other(format!(
                        "vm_id {} >= n_max {}",
                        vm_id, self.spec.n_max
                    )));
                }
                self.offsets.order_ring_range(vm_id, self.spec.n_max)?
            }
        };
        ShmRegion::sub_view(self.parent.clone(), offset, len)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Header 초기화 / 검증 — 내부 unsafe helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Header 가 비어있으면 초기화, 아니면 검증. 초기화했으면 true.
///
/// # 안전
/// `region` 은 최소 `offsets.total_bytes` 만큼 매핑되어 있어야 함.
unsafe fn init_or_validate_header(
    region: &ShmRegion,
    spec: &LayoutSpec,
    offsets: &LayoutOffsets,
    digest: &[u8; 32],
) -> ShmResult<bool> {
    // SAFETY: region.raw_base() 는 유효 매핑의 첫 바이트. RegionHeader 는 4KB
    // 으로 align 된 구조이고 매핑 시작점은 page 경계에 있으므로 alignment OK.
    let ptr = region.raw_base() as *mut RegionHeader;
    let magic = unsafe { std::ptr::read(std::ptr::addr_of!((*ptr).magic)) };
    if magic == 0 {
        // fresh init.
        let header = RegionHeader {
            magic: SHARED_REGION_MAGIC,
            version: SHM_VERSION,
            writer_role: Role::Publisher.header_code(), // 최초 기록자는 언제나 publisher.
            writer_pid: AtomicU32::new(std::process::id()),
            _pad_a: [0; 4],
            created_ns: crate::now_realtime_ns(),
            pub_heartbeat_ns: AtomicU64::new(0),
            pub_seq: AtomicU64::new(0),
            quote_slot_count: spec.quote_slot_count,
            trade_ring_capacity: spec.trade_ring_capacity,
            symtab_capacity: spec.symtab_capacity,
            order_ring_capacity: spec.order_ring_capacity,
            n_max: spec.n_max,
            _pad_b: [0; 4],
            offset_quote: offsets.offset_quote as u64,
            offset_trade: offsets.offset_trade as u64,
            offset_symtab: offsets.offset_symtab as u64,
            offset_orders_base: offsets.offset_orders_base as u64,
            order_ring_stride: offsets.order_ring_stride as u64,
            total_bytes: offsets.total_bytes as u64,
            layout_digest: *digest,
            _reserved: [0; 3920],
        };
        // SAFETY: ptr 가 유효한 RegionHeader 쓰기 영역.
        unsafe { std::ptr::write(ptr, header) };
        Ok(true)
    } else {
        unsafe { validate_header(region, spec, offsets, digest)? };
        // writer_pid 갱신 (이어받기).
        let pid = unsafe { &(*ptr).writer_pid };
        pid.store(std::process::id(), Ordering::Release);
        Ok(false)
    }
}

/// Header 의 필드가 spec 과 일치하는지, digest 가 같은지 확인.
unsafe fn validate_header(
    region: &ShmRegion,
    spec: &LayoutSpec,
    offsets: &LayoutOffsets,
    expected_digest: &[u8; 32],
) -> ShmResult<()> {
    // SAFETY: caller 가 region 크기 보장.
    let ptr = region.as_ptr() as *const RegionHeader;
    let hdr = unsafe { &*ptr };
    if hdr.magic != SHARED_REGION_MAGIC {
        return Err(ShmError::BadMagic {
            expected: SHARED_REGION_MAGIC,
            actual: hdr.magic,
        });
    }
    if hdr.version != SHM_VERSION {
        return Err(ShmError::BadVersion {
            expected: SHM_VERSION,
            actual: hdr.version,
        });
    }
    if hdr.quote_slot_count != spec.quote_slot_count
        || hdr.trade_ring_capacity != spec.trade_ring_capacity
        || hdr.symtab_capacity != spec.symtab_capacity
        || hdr.order_ring_capacity != spec.order_ring_capacity
        || hdr.n_max != spec.n_max
    {
        return Err(ShmError::Other(format!(
            "spec mismatch: file has (q={}, t={}, s={}, o={}, n={}) but expected (q={}, t={}, s={}, o={}, n={})",
            hdr.quote_slot_count,
            hdr.trade_ring_capacity,
            hdr.symtab_capacity,
            hdr.order_ring_capacity,
            hdr.n_max,
            spec.quote_slot_count,
            spec.trade_ring_capacity,
            spec.symtab_capacity,
            spec.order_ring_capacity,
            spec.n_max,
        )));
    }
    if hdr.offset_quote != offsets.offset_quote as u64
        || hdr.offset_trade != offsets.offset_trade as u64
        || hdr.offset_symtab != offsets.offset_symtab as u64
        || hdr.offset_orders_base != offsets.offset_orders_base as u64
        || hdr.order_ring_stride != offsets.order_ring_stride as u64
        || hdr.total_bytes != offsets.total_bytes as u64
    {
        return Err(ShmError::Other(
            "offset mismatch between header and recomputed layout — \
             code/config divergence, reboot required"
                .into(),
        ));
    }
    if hdr.layout_digest != *expected_digest {
        return Err(ShmError::Other(format!(
            "layout digest mismatch: file={} expected={}",
            hex_short(&hdr.layout_digest),
            hex_short(expected_digest),
        )));
    }
    Ok(())
}

/// digest 의 앞 8바이트를 hex 로. 로그 출력용.
fn hex_short(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(16);
    for byte in &b[..8] {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn spec() -> LayoutSpec {
        LayoutSpec {
            quote_slot_count: 32,
            trade_ring_capacity: 64,
            symtab_capacity: 16,
            order_ring_capacity: 16,
            n_max: 4,
        }
    }

    #[test]
    fn offsets_are_page_aligned_and_non_overlapping() {
        let s = spec();
        let o = LayoutOffsets::compute(&s).unwrap();
        assert_eq!(o.offset_quote % SUBREGION_ALIGN, 0);
        assert_eq!(o.offset_trade % SUBREGION_ALIGN, 0);
        assert_eq!(o.offset_symtab % SUBREGION_ALIGN, 0);
        assert_eq!(o.offset_orders_base % SUBREGION_ALIGN, 0);
        assert_eq!(o.order_ring_stride % SUBREGION_ALIGN, 0);
        // non-overlap.
        assert!(o.offset_quote + o.size_quote <= o.offset_trade);
        assert!(o.offset_trade + o.size_trade <= o.offset_symtab);
        assert!(o.offset_symtab + o.size_symtab <= o.offset_orders_base);
        let last = o.offset_orders_base + o.order_ring_stride * (s.n_max as usize);
        assert_eq!(last, o.total_bytes);
    }

    #[test]
    fn digest_is_deterministic() {
        let s = spec();
        let o = LayoutOffsets::compute(&s).unwrap();
        let d1 = layout_digest(&s, &o);
        let d2 = layout_digest(&s, &o);
        assert_eq!(d1, d2);
    }

    #[test]
    fn digest_changes_when_spec_changes() {
        let s1 = spec();
        let mut s2 = s1;
        s2.n_max = s1.n_max + 1;
        let o1 = LayoutOffsets::compute(&s1).unwrap();
        let o2 = LayoutOffsets::compute(&s2).unwrap();
        assert_ne!(layout_digest(&s1, &o1), layout_digest(&s2, &o2));
    }

    #[test]
    fn invalid_spec_rejected() {
        let mut s = spec();
        s.n_max = 0;
        assert!(s.validate().is_err());
        s = spec();
        s.trade_ring_capacity = 7;
        assert!(s.validate().is_err());
        s = spec();
        s.n_max = MAX_ORDER_RINGS + 1;
        assert!(s.validate().is_err());
    }

    #[test]
    fn create_then_open_roundtrip() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("r1");
        let s = spec();
        let r1 =
            SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
                .unwrap();
        assert!(r1.was_created());
        assert_eq!(
            r1.total_bytes(),
            LayoutOffsets::compute(&s).unwrap().total_bytes
        );

        let r2 = SharedRegion::open_view(
            Backing::DevShm { path: p.clone() },
            s,
            Role::Strategy { vm_id: 0 },
        )
        .unwrap();
        assert!(!r2.was_created());
        assert_eq!(r2.spec(), r1.spec());
    }

    #[test]
    fn open_with_wrong_spec_fails_digest() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("r2");
        let s = spec();
        let _r1 =
            SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
                .unwrap();
        // expected_spec 이 다름 → digest mismatch.
        let mut wrong = s;
        wrong.n_max = s.n_max + 1;
        // n_max 가 다르면 total_bytes 도 달라 region.len() < expected 가 먼저 터질 수도.
        let err =
            SharedRegion::open_view(Backing::DevShm { path: p.clone() }, wrong, Role::ReadOnly);
        assert!(err.is_err());
    }

    #[test]
    fn sub_region_views_are_correct_size_and_offset() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("r3");
        let s = spec();
        let sr =
            SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
                .unwrap();
        let o = sr.offsets();
        let q = sr.sub_region(SubKind::Quote).unwrap();
        let t = sr.sub_region(SubKind::Trade).unwrap();
        let sm = sr.sub_region(SubKind::Symtab).unwrap();
        assert_eq!(q.len(), o.size_quote);
        assert_eq!(t.len(), o.size_trade);
        assert_eq!(sm.len(), o.size_symtab);
        // 각 view 의 raw ptr 는 parent base + offset.
        let base = sr.parent().as_ptr() as usize;
        assert_eq!(q.as_ptr() as usize - base, o.offset_quote);
        assert_eq!(t.as_ptr() as usize - base, o.offset_trade);
        assert_eq!(sm.as_ptr() as usize - base, o.offset_symtab);
        // 모든 order ring 이 일정 stride 로 떨어진다.
        for i in 0..s.n_max {
            let or = sr.sub_region(SubKind::OrderRing { vm_id: i }).unwrap();
            assert_eq!(or.len(), o.order_ring_stride);
            let expected = o.offset_orders_base + (i as usize) * o.order_ring_stride;
            assert_eq!(or.as_ptr() as usize - base, expected);
        }
    }

    #[test]
    fn sub_region_vm_id_out_of_range() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("r4");
        let s = spec();
        let sr =
            SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
                .unwrap();
        assert!(sr
            .sub_region(SubKind::OrderRing { vm_id: s.n_max })
            .is_err());
    }

    #[test]
    fn second_create_with_same_spec_ok() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("r5");
        let s = spec();
        let r1 =
            SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
                .unwrap();
        drop(r1);
        // 재시작.
        let r2 =
            SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
                .unwrap();
        assert!(!r2.was_created()); // 이미 초기화되어 있었음.
    }
}

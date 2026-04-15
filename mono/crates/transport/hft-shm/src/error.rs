//! hft-shm 공통 에러 타입.

use std::io;
use std::path::PathBuf;

/// SHM 연산 전반에서 발생할 수 있는 에러.
#[derive(Debug, thiserror::Error)]
pub enum ShmError {
    /// OS 수준 I/O 에러 (mmap, open, ftruncate, mlock, madvise 등).
    #[error("io: {0}")]
    Io(#[from] io::Error),

    /// 경로 관련 에러 (존재하지 않거나 권한 부족 등, 상위 컨텍스트 포함).
    #[error("path error on {path:?}: {source}")]
    Path {
        /// 문제 경로.
        path: PathBuf,
        /// 원인.
        #[source]
        source: io::Error,
    },

    /// 비합법적 capacity — 0 이거나 2 의 거듭제곱이 아님.
    #[error("invalid capacity: {0} (must be power-of-two and > 0)")]
    InvalidCapacity(usize),

    /// 파일 크기 overflow.
    #[error("size overflow for capacity {capacity} × element_size {element_size}")]
    SizeOverflow {
        /// 요청 capacity.
        capacity: usize,
        /// element 바이트.
        element_size: usize,
    },

    /// 헤더 magic 불일치 — 영역이 우리 소유가 아니거나 손상됨.
    #[error("magic mismatch: expected 0x{expected:016x}, got 0x{actual:016x}")]
    BadMagic {
        /// 기대 magic.
        expected: u64,
        /// 실제 읽힌 magic.
        actual: u64,
    },

    /// 버전 불일치. 운영 중 rolling upgrade 에서 걸릴 수 있음.
    #[error("version mismatch: expected {expected}, got {actual}")]
    BadVersion {
        /// 기대 버전.
        expected: u32,
        /// 실제 버전.
        actual: u32,
    },

    /// slot/element 크기 불일치 — 헤더와 코드 struct 가 서로 다름.
    #[error("element size mismatch: expected {expected}, got {actual}")]
    ElementSizeMismatch {
        /// 기대 바이트.
        expected: u32,
        /// 실제 바이트.
        actual: u32,
    },

    /// 접근 인덱스 초과.
    #[error("slot index {index} out of range (slot_count={slot_count})")]
    IndexOutOfRange {
        /// 요청한 인덱스.
        index: u32,
        /// slot 수.
        slot_count: u32,
    },

    /// symbol table capacity 초과.
    #[error("symbol table full: capacity={capacity}")]
    SymbolTableFull {
        /// 총 capacity.
        capacity: u32,
    },

    /// symbol 이름이 지원 길이 초과.
    #[error("symbol name too long: {len} > {limit}")]
    SymbolTooLong {
        /// 실제 길이.
        len: usize,
        /// 허용 한도.
        limit: usize,
    },

    /// 필수 플랫폼 기능 (Linux mlock/hugepage 등) 누락.
    #[error("platform feature unsupported: {0}")]
    Unsupported(&'static str),

    /// race 방지용 alignment 검증 실패.
    #[error("alignment violation: {0}")]
    Alignment(&'static str),

    /// 기타 (컨텍스트 포함).
    #[error("{0}")]
    Other(String),
}

/// SHM 작업 공통 Result.
pub type ShmResult<T> = Result<T, ShmError>;

impl ShmError {
    /// io 에러를 path 컨텍스트와 함께 감싼다.
    pub fn path(path: impl Into<PathBuf>, source: io::Error) -> Self {
        ShmError::Path {
            path: path.into(),
            source,
        }
    }
}

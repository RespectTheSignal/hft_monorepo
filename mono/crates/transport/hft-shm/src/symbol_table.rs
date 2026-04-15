//! Symbol table — `(ExchangeId, &str)` → `symbol_idx: u32`.
//!
//! **Append-only** 구조. 삭제 없음. 재시작 시에도 기존 인덱스를 유지한다.
//!
//! ## 왜 SHM 에 두는가
//! - Python strategy 와 Rust publisher / subscriber / order-gateway 가 모두 같은
//!   "symbol → index" 매핑을 공유해야 한다. DB/gRPC 왕복은 hot path 에 부담.
//! - 변경 빈도가 거의 0 (서비스 기동 시 1회 또는 새 심볼 상장 시 append) 이라 write
//!   race 는 mutex 1개로 충분.
//!
//! ## 동시성
//! - 프로세스 간 write 동시성을 막기 위해 **서버 프로세스가 writer 권한을 독점**
//!   (관례적으로 publisher). reader 는 `count` atomic 만 보면 됨.
//! - writer 가 여러 프로세스일 경우를 대비해 `get_or_intern` 은 프로세스-내
//!   parking_lot Mutex 로 직렬화 + count atomic CAS.

use std::path::Path;
use std::sync::atomic::Ordering;

use hft_types::ExchangeId;
use parking_lot::Mutex;

use crate::error::{ShmError, ShmResult};
use crate::layout::{
    SymbolEntry, SymbolTableHeader, SHM_VERSION, SYMBOL_MAX_LEN, SYMTAB_MAGIC,
};
use crate::mmap::ShmRegion;

/// 통계 — 모니터링용.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SymbolTableStats {
    /// capacity (고정).
    pub capacity: u32,
    /// 현재 등록된 symbol 수.
    pub count: u32,
}

/// `/dev/shm/hft_symtab_v2` 인스턴스.
///
/// 하나의 [`SymbolTable`] 는 동시에 read + writer 역할을 한다. 실제 write 는
/// `get_or_intern` 에서만 발생하며, 프로세스 내부에서 mutex 로 직렬화된다.
pub struct SymbolTable {
    region: ShmRegion,
    capacity: u32,
    // process-local write 직렬화. cross-process writer 는 운영 규약으로 금지.
    writer_lock: Mutex<()>,
}

impl SymbolTable {
    /// 기존 table 에 연결하거나 새로 생성.
    pub fn open_or_create(path: &Path, capacity: u32) -> ShmResult<Self> {
        if capacity == 0 {
            return Err(ShmError::InvalidCapacity(0));
        }
        let total = Self::total_bytes(capacity)?;
        let region = ShmRegion::create_or_attach(path, total, true)?;
        Self::from_region(region, capacity)
    }

    /// SharedRegion sub-view 위에서 symbol table 을 마운트 (writer/reader 공용).
    pub fn from_region(region: ShmRegion, capacity: u32) -> ShmResult<Self> {
        if capacity == 0 {
            return Err(ShmError::InvalidCapacity(0));
        }
        let elem = std::mem::size_of::<SymbolEntry>();
        let needed = Self::total_bytes(capacity)?;
        if region.len() < needed {
            return Err(ShmError::Other(format!(
                "symtab region too small: {} < {}",
                region.len(),
                needed
            )));
        }
        // 헤더 초기화/검증. SAFETY: 매핑 size 는 위에서 확보됨.
        let cap = unsafe {
            let hdr_ptr = region.as_ptr() as *mut SymbolTableHeader;
            if (*hdr_ptr).magic == 0 {
                std::ptr::write(
                    hdr_ptr,
                    SymbolTableHeader {
                        magic: SYMTAB_MAGIC,
                        version: SHM_VERSION,
                        capacity,
                        count: std::sync::atomic::AtomicU32::new(0),
                        _pad_a: [0; 4],
                        created_ns: crate::now_realtime_ns(),
                        _pad_b: [0; 32],
                    },
                );
                let entries =
                    (hdr_ptr as *mut u8).add(std::mem::size_of::<SymbolTableHeader>());
                std::ptr::write_bytes(entries, 0, capacity as usize * elem);
                capacity
            } else {
                validate_header(&*hdr_ptr)?;
                if (*hdr_ptr).capacity != capacity {
                    return Err(ShmError::Other(format!(
                        "symbol table capacity mismatch: expected {}, got {}",
                        capacity,
                        (*hdr_ptr).capacity
                    )));
                }
                (*hdr_ptr).capacity
            }
        };
        Ok(Self {
            region,
            capacity: cap,
            writer_lock: Mutex::new(()),
        })
    }

    /// read-only 로 기존 table 열기. capacity 는 헤더에서 읽음.
    pub fn open(path: &Path) -> ShmResult<Self> {
        let region = ShmRegion::attach_existing(path)?;
        Self::open_from_region(region)
    }

    /// SharedRegion sub-view 위에서 heaer 를 읽어 reader 로 마운트. capacity 는
    /// 헤더에서 결정.
    pub fn open_from_region(region: ShmRegion) -> ShmResult<Self> {
        if region.len() < std::mem::size_of::<SymbolTableHeader>() {
            return Err(ShmError::Other(format!(
                "symtab region too small for header: {}",
                region.len()
            )));
        }
        let capacity = unsafe {
            let hdr = &*(region.as_ptr() as *const SymbolTableHeader);
            validate_header(hdr)?;
            hdr.capacity
        };
        Ok(Self {
            region,
            capacity,
            writer_lock: Mutex::new(()),
        })
    }

    /// `header + capacity × entry` 바이트.
    pub fn total_bytes(capacity: u32) -> ShmResult<usize> {
        if capacity == 0 {
            return Err(ShmError::InvalidCapacity(0));
        }
        let hdr = std::mem::size_of::<SymbolTableHeader>();
        let elem = std::mem::size_of::<SymbolEntry>();
        hdr.checked_add(
            (capacity as usize)
                .checked_mul(elem)
                .ok_or(ShmError::SizeOverflow {
                    capacity: capacity as usize,
                    element_size: elem,
                })?,
        )
        .ok_or(ShmError::SizeOverflow {
            capacity: capacity as usize,
            element_size: elem,
        })
    }

    /// `(exchange, name)` 에 대한 기존 인덱스를 찾거나, 없으면 append.
    ///
    /// 프로세스 내부에서 mutex 로 직렬화 — table 등록은 hot path 가 아니므로 OK.
    pub fn get_or_intern(&self, exchange: ExchangeId, name: &str) -> ShmResult<u32> {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > SYMBOL_MAX_LEN {
            return Err(ShmError::SymbolTooLong {
                len: name_bytes.len(),
                limit: SYMBOL_MAX_LEN,
            });
        }
        // 우선 락 없이 빠르게 linear search.
        if let Some(idx) = self.find(exchange, name_bytes) {
            return Ok(idx);
        }
        // miss → mutex + 재조회 + append.
        let _g = self.writer_lock.lock();
        if let Some(idx) = self.find(exchange, name_bytes) {
            return Ok(idx);
        }
        unsafe {
            let hdr_ptr = self.region.raw_base() as *mut SymbolTableHeader;
            let count_atomic = &(*hdr_ptr).count;
            let cur = count_atomic.load(Ordering::Acquire);
            if cur >= self.capacity {
                return Err(ShmError::SymbolTableFull {
                    capacity: self.capacity,
                });
            }
            let entry = self.entry_mut_ptr(cur);
            // 본문 기록.
            std::ptr::addr_of_mut!((*entry).exchange_id).write(exchange_to_u8(exchange));
            std::ptr::addr_of_mut!((*entry).name_len).write(name_bytes.len() as u32);
            let mut buf = [0u8; SYMBOL_MAX_LEN];
            buf[..name_bytes.len()].copy_from_slice(name_bytes);
            std::ptr::addr_of_mut!((*entry).name).write(buf);
            // count publish — reader 가 이 시점 이후로 entry 를 보게 함.
            count_atomic.store(cur + 1, Ordering::Release);
            Ok(cur)
        }
    }

    /// 이미 등록된 symbol 만 조회. 없으면 None.
    pub fn lookup(&self, exchange: ExchangeId, name: &str) -> Option<u32> {
        self.find(exchange, name.as_bytes())
    }

    /// 역조회 — idx → (exchange, name).
    pub fn resolve(&self, idx: u32) -> Option<(ExchangeId, String)> {
        let count = self.count();
        if idx >= count {
            return None;
        }
        unsafe {
            let entry = self.entry_ptr(idx);
            let ex_raw = std::ptr::read(std::ptr::addr_of!((*entry).exchange_id));
            let name_len = std::ptr::read(std::ptr::addr_of!((*entry).name_len)) as usize;
            let name_bytes = std::ptr::addr_of!((*entry).name) as *const u8;
            let slice = std::slice::from_raw_parts(name_bytes, name_len.min(SYMBOL_MAX_LEN));
            let name = std::str::from_utf8(slice).ok()?.to_owned();
            let ex = exchange_from_u8(ex_raw)?;
            Some((ex, name))
        }
    }

    /// 현재 count. Acquire load.
    pub fn count(&self) -> u32 {
        unsafe {
            (*(self.region.as_ptr() as *const SymbolTableHeader))
                .count
                .load(Ordering::Acquire)
        }
    }

    /// capacity.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// 통계.
    pub fn stats(&self) -> SymbolTableStats {
        SymbolTableStats {
            capacity: self.capacity,
            count: self.count(),
        }
    }

    /// 파일 경로.
    pub fn path(&self) -> &Path {
        self.region.path()
    }

    fn find(&self, exchange: ExchangeId, name_bytes: &[u8]) -> Option<u32> {
        let count = self.count();
        let ex_u8 = exchange_to_u8(exchange);
        for i in 0..count {
            unsafe {
                let entry = self.entry_ptr(i);
                let e_ex = std::ptr::read(std::ptr::addr_of!((*entry).exchange_id));
                if e_ex != ex_u8 {
                    continue;
                }
                let e_len = std::ptr::read(std::ptr::addr_of!((*entry).name_len)) as usize;
                if e_len != name_bytes.len() {
                    continue;
                }
                let name_ptr = std::ptr::addr_of!((*entry).name) as *const u8;
                let slice = std::slice::from_raw_parts(name_ptr, e_len.min(SYMBOL_MAX_LEN));
                if slice == name_bytes {
                    return Some(i);
                }
            }
        }
        None
    }

    unsafe fn entry_ptr(&self, idx: u32) -> *const SymbolEntry {
        let base = self.region.as_ptr();
        let entries =
            unsafe { base.add(std::mem::size_of::<SymbolTableHeader>()) } as *const SymbolEntry;
        unsafe { entries.add(idx as usize) }
    }

    unsafe fn entry_mut_ptr(&self, idx: u32) -> *mut SymbolEntry {
        let base = self.region.raw_base();
        let entries =
            unsafe { base.add(std::mem::size_of::<SymbolTableHeader>()) } as *mut SymbolEntry;
        unsafe { entries.add(idx as usize) }
    }
}

fn validate_header(hdr: &SymbolTableHeader) -> ShmResult<()> {
    if hdr.magic != SYMTAB_MAGIC {
        return Err(ShmError::BadMagic {
            expected: SYMTAB_MAGIC,
            actual: hdr.magic,
        });
    }
    if hdr.version != SHM_VERSION {
        return Err(ShmError::BadVersion {
            expected: SHM_VERSION,
            actual: hdr.version,
        });
    }
    Ok(())
}

/// ExchangeId 의 SHM 와이어 코드. 절대 순서를 바꾸지 말 것 (저장된 파일 깨짐).
///
/// `ExchangeId` enum 자체엔 `#[repr(u8)]` 이 없어 `as u8` 는 불안정. 이 함수가
/// 유일한 ground-truth — publisher/subscriber/strategy 가 모두 이것을 거친다.
pub fn exchange_to_u8(v: ExchangeId) -> u8 {
    match v {
        ExchangeId::Binance => 1,
        ExchangeId::Gate => 2,
        ExchangeId::Bybit => 3,
        ExchangeId::Bitget => 4,
        ExchangeId::Okx => 5,
    }
}

/// 역변환. 알 수 없는 값이면 None.
pub fn exchange_from_u8(v: u8) -> Option<ExchangeId> {
    match v {
        1 => Some(ExchangeId::Binance),
        2 => Some(ExchangeId::Gate),
        3 => Some(ExchangeId::Bybit),
        4 => Some(ExchangeId::Bitget),
        5 => Some(ExchangeId::Okx),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn intern_and_lookup_roundtrip() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("s1");
        let t = SymbolTable::open_or_create(&p, 64).unwrap();
        let i0 = t.get_or_intern(ExchangeId::Gate, "BTC_USDT").unwrap();
        let i1 = t.get_or_intern(ExchangeId::Gate, "ETH_USDT").unwrap();
        let i2 = t.get_or_intern(ExchangeId::Binance, "BTCUSDT").unwrap();
        assert_eq!(i0, 0);
        assert_eq!(i1, 1);
        assert_eq!(i2, 2);
        // 재질의 시 같은 인덱스.
        assert_eq!(t.lookup(ExchangeId::Gate, "BTC_USDT"), Some(0));
        assert_eq!(t.lookup(ExchangeId::Gate, "ETH_USDT"), Some(1));
        assert_eq!(t.lookup(ExchangeId::Binance, "BTCUSDT"), Some(2));
        // resolve.
        let (ex, name) = t.resolve(0).unwrap();
        assert_eq!(ex, ExchangeId::Gate);
        assert_eq!(name, "BTC_USDT");
    }

    #[test]
    fn full_returns_error() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("s2");
        let t = SymbolTable::open_or_create(&p, 2).unwrap();
        t.get_or_intern(ExchangeId::Gate, "A").unwrap();
        t.get_or_intern(ExchangeId::Gate, "B").unwrap();
        let e = t.get_or_intern(ExchangeId::Gate, "C");
        assert!(matches!(e, Err(ShmError::SymbolTableFull { .. })));
    }

    #[test]
    fn long_symbol_rejected() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("s3");
        let t = SymbolTable::open_or_create(&p, 8).unwrap();
        let long = "X".repeat(SYMBOL_MAX_LEN + 1);
        let e = t.get_or_intern(ExchangeId::Gate, &long);
        assert!(matches!(e, Err(ShmError::SymbolTooLong { .. })));
    }

    #[test]
    fn reopen_preserves_entries() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("s4");
        {
            let t = SymbolTable::open_or_create(&p, 16).unwrap();
            t.get_or_intern(ExchangeId::Gate, "BTC_USDT").unwrap();
            t.get_or_intern(ExchangeId::Binance, "ETHUSDT").unwrap();
        }
        let t2 = SymbolTable::open(&p).unwrap();
        assert_eq!(t2.count(), 2);
        assert_eq!(t2.lookup(ExchangeId::Gate, "BTC_USDT"), Some(0));
        assert_eq!(t2.lookup(ExchangeId::Binance, "ETHUSDT"), Some(1));
    }

    #[test]
    fn different_exchanges_get_different_slots() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("s5");
        let t = SymbolTable::open_or_create(&p, 8).unwrap();
        let a = t.get_or_intern(ExchangeId::Gate, "BTC").unwrap();
        let b = t.get_or_intern(ExchangeId::Binance, "BTC").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn stats_report_capacity_and_count() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("s6");
        let t = SymbolTable::open_or_create(&p, 32).unwrap();
        t.get_or_intern(ExchangeId::Gate, "X").unwrap();
        let s = t.stats();
        assert_eq!(s.capacity, 32);
        assert_eq!(s.count, 1);
    }
}

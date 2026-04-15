//! Quote slot — per-symbol seqlock 기반 BookTicker 공유.
//!
//! ## 프로토콜 (seqlock)
//!
//! **Writer**:
//! 1. `seq0 = seq.load(Relaxed)` — 현재 seq.
//! 2. `seq.store(seq0 | 1, Release)` — 홀수로 진입 (writing).
//! 3. 본문 필드 (`bid_price`, ...) 를 raw pointer 로 store.
//! 4. `seq.store(seq0 + 2, Release)` — 다음 짝수로 완료.
//!
//! seq 가 홀수인 동안 reader 는 torn read 로 판단하고 재시도.
//!
//! **Reader**:
//! 1. `s1 = seq.load(Acquire)`. 홀수면 writer 쓰는 중 → spin.
//! 2. 본문 필드들을 읽음.
//! 3. `s2 = seq.load(Acquire)`. `s1 != s2` 이면 중간에 writer 가 시작 → 재시도.
//! 4. 일치하면 torn-read 없는 스냅샷.
//!
//! ## 성능
//! - 모든 필드 접근은 cacheline 내 — 128B slot 이 1 cacheline(64B) × 2.
//! - writer 의 seq store 는 Release, reader 의 seq load 는 Acquire →
//!   x86 에선 단순 mov (mfence 없음).

use std::hint;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{ShmError, ShmResult};
use crate::layout::{
    QuoteSlot, QuoteSlotHeader, QuoteSnapshot, QuoteUpdate, QUOTE_MAGIC, SHM_VERSION,
};
use crate::mmap::ShmRegion;

/// reader 가 torn-read 복구를 위해 재시도할 최대 횟수 (지수 백오프 없이 spin).
/// 실전에서 writer 가 완료까지 ~수 ns 면 충분하지만, writer 프로세스가 죽은
/// 상태에서 slot 이 영원히 홀수로 남을 수 있어 상한을 둔다.
const MAX_SEQLOCK_RETRIES: u32 = 128;

/// Writer — publisher 가 단독 소유. `publish(slot_idx, update)` 만 부른다.
pub struct QuoteSlotWriter {
    region: ShmRegion,
    slot_count: u32,
}

/// Reader — strategy / order-gateway 등이 보유. `read(slot_idx)` 가 Option<Snapshot>.
pub struct QuoteSlotReader {
    region: ShmRegion,
    slot_count: u32,
}

impl QuoteSlotWriter {
    /// `/dev/shm/<...>` 에 quote slot 영역을 생성하거나 기존 영역에 재연결 (재시작 안전).
    pub fn create(path: &Path, slot_count: u32) -> ShmResult<Self> {
        let total = Self::total_bytes(slot_count)?;
        let region = ShmRegion::create_or_attach(path, total, true)?;
        Self::from_region(region, slot_count)
    }

    /// 이미 매핑된 [`ShmRegion`] (보통 [`crate::region::SharedRegion`] 의 sub-view)
    /// 위에 quote 헤더를 초기화/검증하고 writer 로 마운트.
    ///
    /// region.len() 는 최소한 헤더 + slot_count × slot 바이트 이상이어야 한다.
    pub fn from_region(region: ShmRegion, slot_count: u32) -> ShmResult<Self> {
        if slot_count == 0 {
            return Err(ShmError::InvalidCapacity(0));
        }
        let elem = std::mem::size_of::<QuoteSlot>();
        let needed = Self::total_bytes(slot_count)?;
        if region.len() < needed {
            return Err(ShmError::Other(format!(
                "quote region too small: {} < {}",
                region.len(),
                needed
            )));
        }

        // 헤더 초기화 (기존 파일 재사용 시에도 magic/버전 확인 후 writer_pid 갱신).
        // SAFETY: `region` 은 needed 바이트 유효. ptr 는 정렬된 QuoteSlotHeader 로 변환.
        unsafe {
            let base = region.as_ptr() as *mut QuoteSlotHeader;
            debug_assert!((base as usize) % std::mem::align_of::<QuoteSlotHeader>() == 0);
            if (*base).magic == 0 {
                std::ptr::write(
                    base,
                    QuoteSlotHeader {
                        magic: QUOTE_MAGIC,
                        version: SHM_VERSION,
                        slot_count,
                        element_size: elem as u32,
                        writer_pid: std::process::id(),
                        created_ns: crate::now_realtime_ns(),
                        _reserved: [0; 96],
                    },
                );
                let slots_ptr = (base as *mut u8).add(std::mem::size_of::<QuoteSlotHeader>());
                std::ptr::write_bytes(slots_ptr, 0, slot_count as usize * elem);
            } else {
                validate_header(&*base, slot_count, elem)?;
                std::ptr::addr_of_mut!((*base).writer_pid).write(std::process::id());
            }
        }

        Ok(Self { region, slot_count })
    }

    /// `header + slot_count × slot` 바이트.
    pub fn total_bytes(slot_count: u32) -> ShmResult<usize> {
        if slot_count == 0 {
            return Err(ShmError::InvalidCapacity(0));
        }
        let elem = std::mem::size_of::<QuoteSlot>();
        let hdr = std::mem::size_of::<QuoteSlotHeader>();
        hdr.checked_add(
            (slot_count as usize)
                .checked_mul(elem)
                .ok_or(ShmError::SizeOverflow {
                    capacity: slot_count as usize,
                    element_size: elem,
                })?,
        )
        .ok_or(ShmError::SizeOverflow {
            capacity: slot_count as usize,
            element_size: elem,
        })
    }

    /// publish 한 건. `slot_idx` 가 범위 밖이면 에러.
    ///
    /// **단일 writer 가정** — 여러 쓰레드에서 동시에 같은 slot 을 쓰면 결과 미정의.
    pub fn publish(&self, slot_idx: u32, update: &QuoteUpdate) -> ShmResult<()> {
        if slot_idx >= self.slot_count {
            return Err(ShmError::IndexOutOfRange {
                index: slot_idx,
                slot_count: self.slot_count,
            });
        }
        // SAFETY: slot_ptr 은 헤더 이후 slot_idx 번째 slot. 정렬 보장.
        unsafe {
            let slot = self.slot_mut_ptr(slot_idx);
            // seq atomic 은 `&AtomicU64` 참조로 다룬다 — AtomicU64 는 UnsafeCell 기반.
            let seq_atomic: &AtomicU64 = &(*slot).seq;
            let seq0 = seq_atomic.load(Ordering::Relaxed);
            // seq0 이 짝수/홀수 어느 쪽이든, writing 은 항상 "쓰기 중" 을 뜻하는
            // 홀수 값으로 맞춘다. 홀수였다면 이전 publish 가 중단된 상태로 보고
            // 같은 홀수 marker 를 다시 사용해 현재 writer 가 이어서 덮어쓴다.
            let writing = (seq0 | 1).wrapping_add(2).wrapping_sub(2);
            // overwrite marker 가 payload store 보다 먼저 보이도록 phase-1 은
            // SeqCst 로 고정한다. AArch64 에서도 torn-read 가능성을 줄인다.
            seq_atomic.store(writing, Ordering::SeqCst);

            // 필드 기록. atomic 으로 쓸 필요 없음 (seq 가 보호).
            std::ptr::addr_of_mut!((*slot).exchange_id).write(update.exchange_id);
            std::ptr::addr_of_mut!((*slot).symbol_idx).write(slot_idx);
            std::ptr::addr_of_mut!((*slot).bid_price).write(update.bid_price);
            std::ptr::addr_of_mut!((*slot).bid_size).write(update.bid_size);
            std::ptr::addr_of_mut!((*slot).ask_price).write(update.ask_price);
            std::ptr::addr_of_mut!((*slot).ask_size).write(update.ask_size);
            std::ptr::addr_of_mut!((*slot).event_ns).write(update.event_ns);
            std::ptr::addr_of_mut!((*slot).recv_ns).write(update.recv_ns);
            std::ptr::addr_of_mut!((*slot).pub_ns).write(update.pub_ns);
            std::ptr::addr_of_mut!((*slot).flags).write(0);

            // 완료 = 다음 짝수.
            seq_atomic.store(writing.wrapping_add(1), Ordering::Release);
        }
        Ok(())
    }

    /// slot 수.
    pub fn slot_count(&self) -> u32 {
        self.slot_count
    }

    /// 파일 경로.
    pub fn path(&self) -> &Path {
        self.region.path()
    }

    unsafe fn slot_mut_ptr(&self, idx: u32) -> *mut QuoteSlot {
        let base = self.region.raw_base();
        // SAFETY: add offsets within our mapped region.
        let slots_ptr = unsafe { base.add(std::mem::size_of::<QuoteSlotHeader>()) } as *mut QuoteSlot;
        unsafe { slots_ptr.add(idx as usize) }
    }
}

impl QuoteSlotReader {
    /// 기존 quote slot 영역에 read-only 관점으로 연결.
    pub fn open(path: &Path) -> ShmResult<Self> {
        let region = ShmRegion::attach_existing(path)?;
        Self::from_region(region)
    }

    /// 이미 매핑된 sub-view 위에서 quote reader 를 마운트 (header 검증만 수행).
    pub fn from_region(region: ShmRegion) -> ShmResult<Self> {
        // SAFETY: region.len() 이 최소 header 크기 이상이어야 함 (아래에서 검증).
        if region.len() < std::mem::size_of::<QuoteSlotHeader>() {
            return Err(ShmError::Other(format!(
                "quote region too small for header: {}",
                region.len()
            )));
        }
        let slot_count = unsafe {
            let base = region.as_ptr() as *const QuoteSlotHeader;
            let hdr = &*base;
            validate_header(hdr, hdr.slot_count, std::mem::size_of::<QuoteSlot>())?;
            hdr.slot_count
        };
        Ok(Self { region, slot_count })
    }

    /// torn-read 없이 slot 읽기. writer 가 쓰는 동안 spin, 포기 시 None.
    pub fn read(&self, slot_idx: u32) -> Option<QuoteSnapshot> {
        if slot_idx >= self.slot_count {
            return None;
        }
        // SAFETY: 헤더 뒤 slot_idx 번째 slot 에 접근.
        unsafe {
            let slot = self.slot_ptr(slot_idx);
            let seq_atomic: &AtomicU64 = &(*slot).seq;
            for _ in 0..MAX_SEQLOCK_RETRIES {
                let s1 = seq_atomic.load(Ordering::Acquire);
                if s1 == 0 {
                    return None; // 아직 아무것도 쓰지 않음.
                }
                if s1 & 1 != 0 {
                    hint::spin_loop();
                    continue;
                }
                // 본문 읽기.
                let exchange_id = std::ptr::read(std::ptr::addr_of!((*slot).exchange_id));
                let symbol_idx = std::ptr::read(std::ptr::addr_of!((*slot).symbol_idx));
                let bid_price = std::ptr::read(std::ptr::addr_of!((*slot).bid_price));
                let bid_size = std::ptr::read(std::ptr::addr_of!((*slot).bid_size));
                let ask_price = std::ptr::read(std::ptr::addr_of!((*slot).ask_price));
                let ask_size = std::ptr::read(std::ptr::addr_of!((*slot).ask_size));
                let event_ns = std::ptr::read(std::ptr::addr_of!((*slot).event_ns));
                let recv_ns = std::ptr::read(std::ptr::addr_of!((*slot).recv_ns));
                let pub_ns = std::ptr::read(std::ptr::addr_of!((*slot).pub_ns));

                let s2 = seq_atomic.load(Ordering::Acquire);
                if s1 == s2 {
                    return Some(QuoteSnapshot {
                        seq: s1,
                        exchange_id,
                        symbol_idx,
                        bid_price,
                        bid_size,
                        ask_price,
                        ask_size,
                        event_ns,
                        recv_ns,
                        pub_ns,
                    });
                }
                // 중간에 writer 진입 → 재시도.
                hint::spin_loop();
            }
            None
        }
    }

    /// slot 수.
    pub fn slot_count(&self) -> u32 {
        self.slot_count
    }

    /// 파일 경로.
    pub fn path(&self) -> &Path {
        self.region.path()
    }

    unsafe fn slot_ptr(&self, idx: u32) -> *const QuoteSlot {
        let base = self.region.as_ptr();
        let slots_ptr = unsafe { base.add(std::mem::size_of::<QuoteSlotHeader>()) } as *const QuoteSlot;
        unsafe { slots_ptr.add(idx as usize) }
    }
}

fn validate_header(
    hdr: &QuoteSlotHeader,
    expected_slot_count: u32,
    elem_size: usize,
) -> ShmResult<()> {
    if hdr.magic != QUOTE_MAGIC {
        return Err(ShmError::BadMagic {
            expected: QUOTE_MAGIC,
            actual: hdr.magic,
        });
    }
    if hdr.version != SHM_VERSION {
        return Err(ShmError::BadVersion {
            expected: SHM_VERSION,
            actual: hdr.version,
        });
    }
    if hdr.element_size as usize != elem_size {
        return Err(ShmError::ElementSizeMismatch {
            expected: elem_size as u32,
            actual: hdr.element_size,
        });
    }
    if hdr.slot_count != expected_slot_count {
        return Err(ShmError::Other(format!(
            "slot_count mismatch: expected {}, got {}",
            expected_slot_count, hdr.slot_count
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn mk_update(seed: i64) -> QuoteUpdate {
        QuoteUpdate {
            exchange_id: 1,
            bid_price: 10_000 + seed,
            bid_size: 100 + seed,
            ask_price: 10_001 + seed,
            ask_size: 200 + seed,
            event_ns: 1_000_000 + seed as u64,
            recv_ns: 2_000_000 + seed as u64,
            pub_ns: 3_000_000 + seed as u64,
        }
    }

    #[test]
    fn writer_publishes_and_reader_sees_last_write() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("q1");
        let w = QuoteSlotWriter::create(&p, 4).unwrap();
        w.publish(2, &mk_update(42)).unwrap();
        let r = QuoteSlotReader::open(&p).unwrap();
        let s = r.read(2).unwrap();
        assert_eq!(s.bid_price, 10_042);
        assert_eq!(s.ask_size, 242);
        assert_eq!(s.symbol_idx, 2);
        assert_eq!(s.exchange_id, 1);
        assert_eq!(s.seq & 1, 0, "seq must be even after publish");
    }

    #[test]
    fn reader_returns_none_for_uninitialized_slot() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("q2");
        let _w = QuoteSlotWriter::create(&p, 4).unwrap();
        let r = QuoteSlotReader::open(&p).unwrap();
        assert!(r.read(0).is_none());
    }

    #[test]
    fn writer_rejects_out_of_range() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("q3");
        let w = QuoteSlotWriter::create(&p, 4).unwrap();
        let e = w.publish(10, &mk_update(0));
        assert!(matches!(e, Err(ShmError::IndexOutOfRange { .. })));
    }

    #[test]
    fn publish_many_then_read_latest() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("q4");
        let w = QuoteSlotWriter::create(&p, 16).unwrap();
        for i in 0..1000 {
            w.publish(7, &mk_update(i)).unwrap();
        }
        let r = QuoteSlotReader::open(&p).unwrap();
        let s = r.read(7).unwrap();
        assert_eq!(s.bid_price, 10_000 + 999);
    }

    #[test]
    fn concurrent_writer_reader_no_tear() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempdir().unwrap();
        let p = dir.path().join("q5");
        let w = Arc::new(QuoteSlotWriter::create(&p, 4).unwrap());
        let path_for_reader = p.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_w = stop.clone();
        let w_clone = w.clone();

        let writer = thread::spawn(move || {
            let mut i: i64 = 0;
            while !stop_w.load(Ordering::Relaxed) {
                // consistent: bid+1 == ask, size 동일. reader 가 그 invariant 를 본다.
                let u = QuoteUpdate {
                    exchange_id: 1,
                    bid_price: 1000 + i,
                    bid_size: 10,
                    ask_price: 1001 + i,
                    ask_size: 10,
                    event_ns: i as u64,
                    recv_ns: i as u64,
                    pub_ns: i as u64,
                };
                w_clone.publish(1, &u).unwrap();
                i = i.wrapping_add(1);
            }
        });

        let r = QuoteSlotReader::open(&path_for_reader).unwrap();
        let mut seen = 0;
        for _ in 0..200_000 {
            if let Some(s) = r.read(1) {
                // invariant: ask_price - bid_price == 1.
                assert_eq!(
                    s.ask_price - s.bid_price,
                    1,
                    "torn read detected: bid={} ask={}",
                    s.bid_price,
                    s.ask_price
                );
                seen += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        assert!(seen > 0, "reader should have read at least some snapshots");
    }

    #[test]
    fn rejects_reopen_with_wrong_magic() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("q6");
        // 빈 파일을 2048 바이트로 만들어 magic=0 상태 → create 는 통과 (초기화),
        // 그 뒤 바이트 손상 후 open 해서 magic 불일치 시 에러 확인.
        {
            let _w = QuoteSlotWriter::create(&p, 4).unwrap();
        }
        // magic 훼손.
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(&[0xFFu8; 8]).unwrap();
        }
        let e = QuoteSlotReader::open(&p);
        assert!(matches!(e, Err(ShmError::BadMagic { .. })));
    }
}

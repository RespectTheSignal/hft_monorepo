//! SPSC order ring — Lamport queue.
//!
//! Python strategy 가 producer (head 증가), Rust order-gateway 가 consumer
//! (tail 증가). 둘은 **다른 프로세스** 이므로 head / tail 은 다른 cache line 에 있음
//! (layout.rs 에서 `OrderRingHeader` 가 256B).
//!
//! 프로토콜:
//! - `head` = writer 가 다음에 채울 인덱스.
//! - `tail` = reader 가 다음에 소비할 인덱스.
//! - size = head - tail ≤ capacity.
//!
//! **Writer** (Python 또는 Rust):
//! ```text
//!   let t = tail.load(Acquire);
//!   let h = head.load(Relaxed);
//!   if h - t == cap { return false; } // full
//!   frames[h & mask] = frame; frames[h & mask].seq = h + 1;
//!   head.store(h + 1, Release);
//! ```
//!
//! **Reader** (Rust):
//! ```text
//!   let h = head.load(Acquire);
//!   let t = tail.load(Relaxed);
//!   if h == t { return None; }
//!   let f = frames[t & mask];
//!   if f.seq != t + 1 { spin; continue; }  // writer 아직 완료 전
//!   tail.store(t + 1, Release);
//!   return Some(f);
//! ```

use std::hint;
use std::path::Path;
use std::sync::atomic::Ordering;

use crate::error::{ShmError, ShmResult};
use crate::layout::{OrderFrame, OrderRingHeader, FRAME_SIZE, ORDER_MAGIC, SHM_VERSION};
use crate::mmap::ShmRegion;
use crate::spmc_ring::compute_ring_size;

const READER_SPIN_LIMIT: u32 = 1024;

/// Rust-side SPSC writer (대부분의 경우엔 unused; Python 이 writer). ack 채널 등에서 사용.
pub struct OrderRingWriter {
    region: ShmRegion,
    capacity: u64,
    capacity_mask: u64,
}

/// Rust-side SPSC reader — order-gateway 가 소유.
pub struct OrderRingReader {
    region: ShmRegion,
    capacity: u64,
    capacity_mask: u64,
}

impl OrderRingWriter {
    /// 신규 생성 또는 기존 재연결.
    pub fn create(path: &Path, capacity: u64) -> ShmResult<Self> {
        if capacity == 0 || !capacity.is_power_of_two() {
            return Err(ShmError::InvalidCapacity(capacity as usize));
        }
        let total = compute_ring_size::<OrderRingHeader, OrderFrame>(capacity)?;
        let region = ShmRegion::create_or_attach(path, total, true)?;
        Self::from_region(region, capacity)
    }

    /// SharedRegion sub-view 위에서 order ring writer 를 마운트.
    pub fn from_region(region: ShmRegion, capacity: u64) -> ShmResult<Self> {
        if capacity == 0 || !capacity.is_power_of_two() {
            return Err(ShmError::InvalidCapacity(capacity as usize));
        }
        let elem = std::mem::size_of::<OrderFrame>();
        let needed = compute_ring_size::<OrderRingHeader, OrderFrame>(capacity)?;
        if region.len() < needed {
            return Err(ShmError::Other(format!(
                "order ring region too small: {} < {}",
                region.len(),
                needed
            )));
        }
        unsafe {
            let hdr_ptr = region.as_ptr() as *mut OrderRingHeader;
            if (*hdr_ptr).magic == 0 {
                std::ptr::write(
                    hdr_ptr,
                    OrderRingHeader {
                        magic: ORDER_MAGIC,
                        version: SHM_VERSION,
                        capacity_mask: (capacity - 1) as u32,
                        element_size: elem as u32,
                        writer_pid: std::process::id(),
                        created_ns: crate::now_realtime_ns(),
                        _pad_a: [0; 32],
                        head: std::sync::atomic::AtomicU64::new(0),
                        _pad_b: [0; 56],
                        tail: std::sync::atomic::AtomicU64::new(0),
                        _pad_c: [0; 56],
                    },
                );
                let frames = (hdr_ptr as *mut u8).add(std::mem::size_of::<OrderRingHeader>());
                std::ptr::write_bytes(frames, 0, capacity as usize * elem);
            } else {
                validate_header(&*hdr_ptr, capacity, elem)?;
                std::ptr::addr_of_mut!((*hdr_ptr).writer_pid).write(std::process::id());
            }
        }
        Ok(Self {
            region,
            capacity,
            capacity_mask: capacity - 1,
        })
    }

    /// publish. full 이면 false.
    pub fn publish(&self, frame: &OrderFrame) -> bool {
        unsafe {
            let hdr = &*(self.region.as_ptr() as *const OrderRingHeader);
            let t = hdr.tail.load(Ordering::Acquire);
            let h = hdr.head.load(Ordering::Relaxed);
            if h - t >= self.capacity {
                return false;
            }
            let idx = h & self.capacity_mask;
            let slot = self.frame_mut_ptr(idx);
            // 본문 복사. seq 는 마지막에.
            std::ptr::addr_of_mut!((*slot).kind).write(frame.kind);
            std::ptr::addr_of_mut!((*slot).exchange_id).write(frame.exchange_id);
            std::ptr::addr_of_mut!((*slot).symbol_idx).write(frame.symbol_idx);
            std::ptr::addr_of_mut!((*slot).side).write(frame.side);
            std::ptr::addr_of_mut!((*slot).tif).write(frame.tif);
            std::ptr::addr_of_mut!((*slot).ord_type).write(frame.ord_type);
            std::ptr::addr_of_mut!((*slot).price).write(frame.price);
            std::ptr::addr_of_mut!((*slot).size).write(frame.size);
            std::ptr::addr_of_mut!((*slot).client_id).write(frame.client_id);
            std::ptr::addr_of_mut!((*slot).ts_ns).write(frame.ts_ns);
            std::ptr::addr_of_mut!((*slot).aux).write(frame.aux);
            // seq = h + 1 (짝수/홀수 의미는 SPSC 에선 필요 없고, "commit marker" 역할).
            std::ptr::addr_of_mut!((*slot).seq).write(h + 1);

            hdr.head.store(h + 1, Ordering::Release);
            true
        }
    }

    /// capacity.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// 파일 경로.
    pub fn path(&self) -> &Path {
        self.region.path()
    }

    unsafe fn frame_mut_ptr(&self, idx: u64) -> *mut OrderFrame {
        let base = self.region.raw_base();
        let frames = unsafe { base.add(std::mem::size_of::<OrderRingHeader>()) } as *mut OrderFrame;
        unsafe { frames.add(idx as usize) }
    }
}

impl OrderRingReader {
    /// 기존 ring 열기.
    pub fn open(path: &Path) -> ShmResult<Self> {
        let region = ShmRegion::attach_existing(path)?;
        Self::from_region(region)
    }

    /// SharedRegion sub-view 위에서 order ring reader 를 마운트.
    pub fn from_region(region: ShmRegion) -> ShmResult<Self> {
        if region.len() < std::mem::size_of::<OrderRingHeader>() {
            return Err(ShmError::Other(format!(
                "order ring region too small for header: {}",
                region.len()
            )));
        }
        let capacity = unsafe {
            let hdr = &*(region.as_ptr() as *const OrderRingHeader);
            let cap = (hdr.capacity_mask as u64) + 1;
            validate_header(hdr, cap, std::mem::size_of::<OrderFrame>())?;
            cap
        };
        Ok(Self {
            region,
            capacity,
            capacity_mask: capacity - 1,
        })
    }

    /// 현재 size (head - tail). approximate — writer/reader 가 각각 독립 진전 가능.
    pub fn len(&self) -> u64 {
        unsafe {
            let hdr = &*(self.region.as_ptr() as *const OrderRingHeader);
            hdr.head
                .load(Ordering::Acquire)
                .saturating_sub(hdr.tail.load(Ordering::Acquire))
        }
    }

    /// empty 인지.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 한 주문 소비. 없으면 None.
    pub fn try_consume(&mut self) -> Option<OrderFrame> {
        unsafe {
            let hdr = &*(self.region.as_ptr() as *const OrderRingHeader);
            let h = hdr.head.load(Ordering::Acquire);
            let t = hdr.tail.load(Ordering::Relaxed);
            if h == t {
                return None;
            }
            let target_seq = t + 1;
            let idx = t & self.capacity_mask;
            let slot = self.frame_ptr(idx);
            let mut spins: u32 = 0;
            loop {
                let s = std::ptr::read(std::ptr::addr_of!((*slot).seq));
                if s == target_seq {
                    break;
                }
                if spins >= READER_SPIN_LIMIT {
                    return None;
                }
                hint::spin_loop();
                spins += 1;
            }

            let frame = read_order_body(slot, target_seq);
            hdr.tail.store(t + 1, Ordering::Release);
            Some(frame)
        }
    }

    /// 최대 `max` 건 drain.
    pub fn drain_into(&mut self, dst: &mut Vec<OrderFrame>, max: usize) -> usize {
        let mut n = 0;
        while n < max {
            match self.try_consume() {
                Some(f) => {
                    dst.push(f);
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// capacity.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// 파일 경로.
    pub fn path(&self) -> &Path {
        self.region.path()
    }

    unsafe fn frame_ptr(&self, idx: u64) -> *const OrderFrame {
        let base = self.region.as_ptr();
        let frames =
            unsafe { base.add(std::mem::size_of::<OrderRingHeader>()) } as *const OrderFrame;
        unsafe { frames.add(idx as usize) }
    }
}

unsafe fn read_order_body(slot: *const OrderFrame, seq: u64) -> OrderFrame {
    OrderFrame {
        seq,
        kind: unsafe { std::ptr::read(std::ptr::addr_of!((*slot).kind)) },
        exchange_id: unsafe { std::ptr::read(std::ptr::addr_of!((*slot).exchange_id)) },
        _pad1: [0; 2],
        symbol_idx: unsafe { std::ptr::read(std::ptr::addr_of!((*slot).symbol_idx)) },
        side: unsafe { std::ptr::read(std::ptr::addr_of!((*slot).side)) },
        tif: unsafe { std::ptr::read(std::ptr::addr_of!((*slot).tif)) },
        ord_type: unsafe { std::ptr::read(std::ptr::addr_of!((*slot).ord_type)) },
        _pad2: [0; 1],
        price: unsafe { std::ptr::read(std::ptr::addr_of!((*slot).price)) },
        size: unsafe { std::ptr::read(std::ptr::addr_of!((*slot).size)) },
        client_id: unsafe { std::ptr::read(std::ptr::addr_of!((*slot).client_id)) },
        ts_ns: unsafe { std::ptr::read(std::ptr::addr_of!((*slot).ts_ns)) },
        aux: unsafe { std::ptr::read(std::ptr::addr_of!((*slot).aux)) },
        _pad3: [0; 16],
    }
}

fn validate_header(hdr: &OrderRingHeader, expected_cap: u64, elem_size: usize) -> ShmResult<()> {
    if hdr.magic != ORDER_MAGIC {
        return Err(ShmError::BadMagic {
            expected: ORDER_MAGIC,
            actual: hdr.magic,
        });
    }
    if hdr.version != SHM_VERSION {
        return Err(ShmError::BadVersion {
            expected: SHM_VERSION,
            actual: hdr.version,
        });
    }
    if hdr.element_size as usize != elem_size || elem_size != FRAME_SIZE {
        return Err(ShmError::ElementSizeMismatch {
            expected: FRAME_SIZE as u32,
            actual: hdr.element_size,
        });
    }
    let cap = (hdr.capacity_mask as u64) + 1;
    if cap != expected_cap {
        return Err(ShmError::Other(format!(
            "capacity mismatch: expected {}, got {}",
            expected_cap, cap
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::OrderKind;
    use tempfile::tempdir;

    fn ord(i: u64) -> OrderFrame {
        OrderFrame {
            seq: 0,
            kind: OrderKind::Place as u8,
            exchange_id: 1,
            _pad1: [0; 2],
            symbol_idx: 3,
            side: 0,
            tif: 1,
            ord_type: 0,
            _pad2: [0; 1],
            price: 1_000_000 + i as i64,
            size: 1 + i as i64,
            client_id: i,
            ts_ns: i * 10,
            aux: [i; 5],
            _pad3: [0; 16],
        }
    }

    #[test]
    fn spsc_roundtrip() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("o1");
        let w = OrderRingWriter::create(&p, 16).unwrap();
        let mut r = OrderRingReader::open(&p).unwrap();
        for i in 0..8 {
            assert!(w.publish(&ord(i)));
        }
        let mut got = Vec::new();
        r.drain_into(&mut got, 100);
        assert_eq!(got.len(), 8);
        for (i, o) in got.iter().enumerate() {
            assert_eq!(o.client_id, i as u64);
        }
    }

    #[test]
    fn full_returns_false() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("o2");
        let w = OrderRingWriter::create(&p, 4).unwrap();
        for i in 0..4 {
            assert!(w.publish(&ord(i)));
        }
        assert!(!w.publish(&ord(99)));
    }

    #[test]
    fn consume_after_drain_returns_none() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("o3");
        let w = OrderRingWriter::create(&p, 4).unwrap();
        let mut r = OrderRingReader::open(&p).unwrap();
        w.publish(&ord(0));
        assert!(r.try_consume().is_some());
        assert!(r.try_consume().is_none());
    }

    #[test]
    fn power_of_two_check() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("o4");
        assert!(OrderRingWriter::create(&p, 5).is_err());
    }

    #[test]
    fn concurrent_spsc_delivers_everything() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use std::thread;

        let dir = tempdir().unwrap();
        let p = dir.path().join("o5");
        let w = Arc::new(OrderRingWriter::create(&p, 128).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let n_total = 5000;

        let w_c = w.clone();
        let producer = thread::spawn(move || {
            let mut i: u64 = 0;
            while i < n_total {
                if w_c.publish(&ord(i)) {
                    i += 1;
                } else {
                    hint::spin_loop();
                }
            }
        });

        let mut r = OrderRingReader::open(&p).unwrap();
        let mut seen = 0u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while seen < n_total && std::time::Instant::now() < deadline {
            if let Some(o) = r.try_consume() {
                assert_eq!(o.client_id, seen);
                seen += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        producer.join().unwrap();
        assert_eq!(seen, n_total);
    }
}

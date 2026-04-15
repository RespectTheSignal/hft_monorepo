//! SPMC broadcast ring — Aeron / Disruptor 스타일.
//!
//! ## 프로토콜
//!
//! **Writer (1명)**:
//! ```text
//!   let w = header.writer_seq.load(Relaxed);
//!   let slot = frames[w & mask];
//!   // 본문 쓰기
//!   slot.seq.store(w + 1, Release);          // seq 에 "commit"
//!   header.writer_seq.store(w + 1, Release); // 외부 상한 공개
//! ```
//!
//! `seq` 는 monotonic. `slot.seq == w + 1` 이면 "이 슬롯은 seq=w+1 의 프레임".
//!
//! **Reader (N명)**:
//! ```text
//!   loop {
//!       let w = header.writer_seq.load(Acquire);
//!       if r == w { return None; }                // empty
//!       let slot = frames[r & mask];
//!       let s = slot.seq.load(Acquire);
//!       if s == r + 1 {
//!           // 아직 writer 가 r 에 쓰는 중.
//!           spin();
//!           continue;
//!       }
//!       if s < r + 1 { return None; }             // race: writer_seq 는 올라갔지만 slot 은 아직
//!       // 본문 읽기
//!       let s2 = slot.seq.load(Acquire);
//!       if s2 != s { /* writer lapped 했다면 재시도 */ continue; }
//!       r += 1;
//!       return Some(frame_copy);
//!   }
//! ```
//!
//! **Lap 감지**: reader 가 `r` 을 유지하다가 writer 가 `r + capacity` 까지 진전하면,
//! `slot[r & mask].seq >= r + capacity + 1` 가 되어 있을 수 있다. 이 경우 `r` 의 원본
//! 데이터는 이미 덮여 사라졌다. reader 는 drop 카운터를 올리고 `r = writer_seq -
//! capacity + 1` 근처로 점프.
//!
//! ## 메모리 격리
//! - `writer_seq` 와 각 frame 의 `seq` 가 서로 다른 cache line. writer 만 둘 다 쓰고,
//!   reader 는 frame.seq 만 반복 load 해 writer 주소 선점을 줄인다.

use std::hint;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{ShmError, ShmResult};
use crate::layout::{TradeFrame, TradeRingHeader, FRAME_SIZE, SHM_VERSION, TRADE_MAGIC};
use crate::mmap::ShmRegion;

/// 기본 reader spin 한도. 이 값을 초과해도 writer 가 commit 안 하면 None 반환.
const READER_SPIN_LIMIT: u32 = 1024;

/// Writer (publisher aggregator) 단독 보유.
pub struct TradeRingWriter {
    region: ShmRegion,
    capacity: u64,
    capacity_mask: u64,
}

/// Reader — strategy, monitoring 등. 각 reader 가 자신의 `cursor` 관리.
pub struct TradeRingReader {
    region: ShmRegion,
    capacity: u64,
    capacity_mask: u64,
    cursor: u64,
    /// reader 가 본 drop (lap) 횟수.
    lap_drops: u64,
}

impl TradeRingWriter {
    /// 새 ring 생성 (또는 기존 재연결). `capacity` 는 power-of-two.
    pub fn create(path: &Path, capacity: u64) -> ShmResult<Self> {
        if capacity == 0 || !capacity.is_power_of_two() {
            return Err(ShmError::InvalidCapacity(capacity as usize));
        }
        let total = compute_ring_size::<TradeRingHeader, TradeFrame>(capacity)?;
        let region = ShmRegion::create_or_attach(path, total, true)?;
        Self::from_region(region, capacity)
    }

    /// SharedRegion sub-view 위에서 ring writer 를 마운트.
    pub fn from_region(region: ShmRegion, capacity: u64) -> ShmResult<Self> {
        if capacity == 0 || !capacity.is_power_of_two() {
            return Err(ShmError::InvalidCapacity(capacity as usize));
        }
        let elem = std::mem::size_of::<TradeFrame>();
        let needed = compute_ring_size::<TradeRingHeader, TradeFrame>(capacity)?;
        if region.len() < needed {
            return Err(ShmError::Other(format!(
                "trade region too small: {} < {}",
                region.len(),
                needed
            )));
        }
        // SAFETY: mmap size 가 TradeRingHeader + capacity * TradeFrame 을 덮는다.
        unsafe {
            let hdr_ptr = region.as_ptr() as *mut TradeRingHeader;
            if (*hdr_ptr).magic == 0 {
                std::ptr::write(
                    hdr_ptr,
                    TradeRingHeader {
                        magic: TRADE_MAGIC,
                        version: SHM_VERSION,
                        capacity_mask: (capacity - 1) as u32,
                        element_size: elem as u32,
                        writer_pid: std::process::id(),
                        created_ns: crate::now_realtime_ns(),
                        _pad_a: [0; 32],
                        writer_seq: AtomicU64::new(0),
                        drops: AtomicU64::new(0),
                        _pad_b: [0; 48],
                    },
                );
                let frames_ptr =
                    (hdr_ptr as *mut u8).add(std::mem::size_of::<TradeRingHeader>());
                std::ptr::write_bytes(frames_ptr, 0, capacity as usize * elem);
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

    /// 한 프레임 publish. 본문만 `frame` 에서 복사해 가고, `seq` 는 writer 가 관리.
    pub fn publish(&self, frame: &TradeFrame) {
        // SAFETY: 위에서 매핑 크기 확인됨. writer 단독 가정.
        unsafe {
            let hdr = &*(self.region.as_ptr() as *const TradeRingHeader);
            let w = hdr.writer_seq.load(Ordering::Relaxed);
            let idx = w & self.capacity_mask;
            let slot = self.frame_mut_ptr(idx);
            // 본문 기록.
            std::ptr::addr_of_mut!((*slot).exchange_id).write(frame.exchange_id);
            std::ptr::addr_of_mut!((*slot).symbol_idx).write(frame.symbol_idx);
            std::ptr::addr_of_mut!((*slot).price).write(frame.price);
            std::ptr::addr_of_mut!((*slot).size).write(frame.size);
            std::ptr::addr_of_mut!((*slot).trade_id).write(frame.trade_id);
            std::ptr::addr_of_mut!((*slot).event_ns).write(frame.event_ns);
            std::ptr::addr_of_mut!((*slot).recv_ns).write(frame.recv_ns);
            std::ptr::addr_of_mut!((*slot).pub_ns).write(frame.pub_ns);
            std::ptr::addr_of_mut!((*slot).flags).write(frame.flags);

            // slot.seq 에 commit — reader 는 이를 Acquire load 로 관측.
            (*slot).seq.store(w + 1, Ordering::Release);
            // writer_seq 를 한 칸 올림.
            hdr.writer_seq.store(w + 1, Ordering::Release);
        }
    }

    /// 현재 writer seq (다음에 쓸 슬롯의 seq-1).
    pub fn writer_seq(&self) -> u64 {
        unsafe {
            (*(self.region.as_ptr() as *const TradeRingHeader))
                .writer_seq
                .load(Ordering::Relaxed)
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

    unsafe fn frame_mut_ptr(&self, idx: u64) -> *mut TradeFrame {
        let base = self.region.raw_base();
        let frames = unsafe { base.add(std::mem::size_of::<TradeRingHeader>()) } as *mut TradeFrame;
        unsafe { frames.add(idx as usize) }
    }
}

impl TradeRingReader {
    /// 기존 ring 에 연결. 초기 cursor 는 현재 `writer_seq` (즉 "지금부터 나오는 것만 본다").
    pub fn open(path: &Path) -> ShmResult<Self> {
        let region = ShmRegion::attach_existing(path)?;
        Self::from_region(region)
    }

    /// SharedRegion sub-view 위에서 ring reader 를 마운트.
    pub fn from_region(region: ShmRegion) -> ShmResult<Self> {
        if region.len() < std::mem::size_of::<TradeRingHeader>() {
            return Err(ShmError::Other(format!(
                "trade region too small for header: {}",
                region.len()
            )));
        }
        let (capacity, cursor) = unsafe {
            let hdr = &*(region.as_ptr() as *const TradeRingHeader);
            let cap = (hdr.capacity_mask as u64) + 1;
            validate_header(hdr, cap, std::mem::size_of::<TradeFrame>())?;
            (cap, hdr.writer_seq.load(Ordering::Acquire))
        };
        Ok(Self {
            region,
            capacity,
            capacity_mask: capacity - 1,
            cursor,
            lap_drops: 0,
        })
    }

    /// 처음부터 읽고 싶을 때 cursor 를 0 으로 초기화. 주의: writer 가 이미 한 바퀴
    /// 돌았다면 lap drop 이 대량 발생한다.
    pub fn rewind_to_start(&mut self) {
        self.cursor = 0;
    }

    /// 현재 cursor.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// 누적 lap-drop.
    pub fn lap_drops(&self) -> u64 {
        self.lap_drops
    }

    /// 한 프레임 소비 시도. empty 또는 writer lag 시 `None`.
    /// lap 된 경우 내부적으로 cursor jump 후 None (다음 호출에서 재시도).
    pub fn try_consume(&mut self) -> Option<TradeFrame> {
        // SAFETY: region 은 유효 매핑.
        unsafe {
            let hdr = &*(self.region.as_ptr() as *const TradeRingHeader);
            let w = hdr.writer_seq.load(Ordering::Acquire);
            if self.cursor >= w {
                return None; // empty
            }

            // lap 감지: writer 가 cursor + capacity 를 초과하면 이미 덮였다.
            if w.saturating_sub(self.cursor) > self.capacity {
                // 드롭 보고. 가장 오래된 보존된 seq = w - capacity + 1 (대략).
                let jump = w.saturating_sub(self.capacity).saturating_add(1);
                let dropped = jump.saturating_sub(self.cursor);
                self.lap_drops = self.lap_drops.saturating_add(dropped);
                self.cursor = jump;
                return None;
            }

            let target_seq = self.cursor + 1;
            let idx = self.cursor & self.capacity_mask;
            let slot = self.frame_ptr(idx);

            let mut spins: u32 = 0;
            loop {
                let s1 = (*slot).seq.load(Ordering::Acquire);
                if s1 < target_seq {
                    // writer 가 아직 이 슬롯을 이번 seq 로 commit 안 함.
                    if spins >= READER_SPIN_LIMIT {
                        return None;
                    }
                    hint::spin_loop();
                    spins += 1;
                    continue;
                }
                if s1 > target_seq {
                    // writer 가 한 바퀴 돌았다 — 이 슬롯은 더 큰 seq 를 이미 보유.
                    // 즉 target_seq 의 데이터는 소실. jump.
                    let jump = s1.saturating_sub(self.capacity).saturating_add(1);
                    let dropped = jump.saturating_sub(self.cursor);
                    self.lap_drops = self.lap_drops.saturating_add(dropped);
                    self.cursor = jump;
                    return None;
                }
                // s1 == target_seq — 본문 읽기.
                let frame = read_frame_body(slot);
                // 다시 seq 확인해 lap 여부 재검증.
                let s2 = (*slot).seq.load(Ordering::Acquire);
                if s2 != s1 {
                    // writer 가 바로 덮었다 → 드롭.
                    if spins >= READER_SPIN_LIMIT {
                        self.lap_drops = self.lap_drops.saturating_add(1);
                        self.cursor += 1;
                        return None;
                    }
                    hint::spin_loop();
                    spins += 1;
                    continue;
                }
                self.cursor += 1;
                return Some(frame);
            }
        }
    }

    /// 한 번에 여러 프레임을 `dst` 로 드레인. 가능한 만큼 채우고 소비 개수 반환.
    pub fn drain_into(&mut self, dst: &mut Vec<TradeFrame>, max: usize) -> usize {
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

    unsafe fn frame_ptr(&self, idx: u64) -> *const TradeFrame {
        let base = self.region.as_ptr();
        let frames =
            unsafe { base.add(std::mem::size_of::<TradeRingHeader>()) } as *const TradeFrame;
        unsafe { frames.add(idx as usize) }
    }
}

/// TradeFrame 본문을 값 복사. `seq` 필드는 0 으로 세팅해 사용자가 구분 가능하도록.
unsafe fn read_frame_body(slot: *const TradeFrame) -> TradeFrame {
    let exchange_id = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).exchange_id)) };
    let symbol_idx = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).symbol_idx)) };
    let price = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).price)) };
    let size = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).size)) };
    let trade_id = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).trade_id)) };
    let event_ns = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).event_ns)) };
    let recv_ns = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).recv_ns)) };
    let pub_ns = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).pub_ns)) };
    let flags = unsafe { std::ptr::read(std::ptr::addr_of!((*slot).flags)) };
    TradeFrame {
        seq: AtomicU64::new(0),
        exchange_id,
        _pad1: [0; 3],
        symbol_idx,
        price,
        size,
        trade_id,
        event_ns,
        recv_ns,
        pub_ns,
        flags,
        _pad2: [0; 28],
    }
}

pub(crate) fn compute_ring_size<H, F>(capacity: u64) -> ShmResult<usize> {
    let hdr = std::mem::size_of::<H>();
    let elem = std::mem::size_of::<F>();
    let body = (capacity as usize).checked_mul(elem).ok_or(ShmError::SizeOverflow {
        capacity: capacity as usize,
        element_size: elem,
    })?;
    hdr.checked_add(body).ok_or(ShmError::SizeOverflow {
        capacity: capacity as usize,
        element_size: elem,
    })
}

fn validate_header(hdr: &TradeRingHeader, expected_cap: u64, elem_size: usize) -> ShmResult<()> {
    if hdr.magic != TRADE_MAGIC {
        return Err(ShmError::BadMagic {
            expected: TRADE_MAGIC,
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
    let cap = (hdr.capacity_mask as u64) + 1;
    if cap != expected_cap {
        return Err(ShmError::Other(format!(
            "capacity mismatch: expected {}, got {}",
            expected_cap, cap
        )));
    }
    if elem_size != FRAME_SIZE {
        return Err(ShmError::ElementSizeMismatch {
            expected: FRAME_SIZE as u32,
            actual: elem_size as u32,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn frame(seed: i64) -> TradeFrame {
        TradeFrame {
            seq: AtomicU64::new(0),
            exchange_id: 2,
            _pad1: [0; 3],
            symbol_idx: 5,
            price: 10_000 + seed,
            size: 100 + seed,
            trade_id: 999_000 + seed,
            event_ns: 1_000_000 + seed as u64,
            recv_ns: 1_000_001 + seed as u64,
            pub_ns: 1_000_002 + seed as u64,
            flags: 0,
            _pad2: [0; 28],
        }
    }

    #[test]
    fn writer_then_reader_delivers_in_order() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("t1");
        let w = TradeRingWriter::create(&p, 16).unwrap();
        // open reader BEFORE publishing so cursor starts at 0.
        let mut r = TradeRingReader::open(&p).unwrap();
        assert_eq!(r.cursor(), 0);
        for i in 0..10 {
            w.publish(&frame(i));
        }
        let mut got = Vec::new();
        r.drain_into(&mut got, 100);
        assert_eq!(got.len(), 10);
        for (i, f) in got.iter().enumerate() {
            assert_eq!(f.price, 10_000 + i as i64);
        }
    }

    #[test]
    fn empty_returns_none() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("t2");
        let _w = TradeRingWriter::create(&p, 8).unwrap();
        let mut r = TradeRingReader::open(&p).unwrap();
        assert!(r.try_consume().is_none());
    }

    #[test]
    fn invalid_capacity_rejected() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("t3");
        assert!(TradeRingWriter::create(&p, 6).is_err());
        assert!(TradeRingWriter::create(&p, 0).is_err());
    }

    #[test]
    fn lap_detection_reports_drops() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("t4");
        let w = TradeRingWriter::create(&p, 4).unwrap();
        let mut r = TradeRingReader::open(&p).unwrap();
        // capacity 4 인데 5개 쓰면 첫 1개는 덮여 drop.
        for i in 0..8 {
            w.publish(&frame(i));
        }
        let mut got = Vec::new();
        // 첫 try_consume 은 lap 감지로 None + cursor jump.
        r.try_consume();
        assert!(r.lap_drops() >= 1);
        r.drain_into(&mut got, 100);
        // 마지막 capacity 개 = [4,5,6,7] 이 최소한 남아있다.
        assert!(got.len() <= 4);
        for f in &got {
            assert!(f.price - 10_000 >= 4);
        }
    }

    #[test]
    fn concurrent_writer_and_reader_see_monotonic_data() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::thread;

        let dir = tempdir().unwrap();
        let p = dir.path().join("t5");
        let w = Arc::new(TradeRingWriter::create(&p, 1024).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = stop.clone();
        let w_c = w.clone();

        let writer = thread::spawn(move || {
            let mut i = 0i64;
            while !stop_w.load(Ordering::Relaxed) && i < 50_000 {
                w_c.publish(&frame(i));
                i += 1;
            }
        });

        let mut r = TradeRingReader::open(&p).unwrap();
        let mut last_price: Option<i64> = None;
        let mut seen = 0u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline && seen < 40_000 {
            if let Some(f) = r.try_consume() {
                if let Some(lp) = last_price {
                    assert!(f.price > lp, "non-monotonic: {} <= {}", f.price, lp);
                }
                last_price = Some(f.price);
                seen += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        assert!(seen > 1000, "expected many frames, got {}", seen);
    }

    #[test]
    fn multiple_readers_each_see_full_stream() {
        use std::sync::Arc;
        let dir = tempdir().unwrap();
        let p = dir.path().join("t6");
        let w = Arc::new(TradeRingWriter::create(&p, 128).unwrap());
        let mut r1 = TradeRingReader::open(&p).unwrap();
        let mut r2 = TradeRingReader::open(&p).unwrap();
        for i in 0..50 {
            w.publish(&frame(i));
        }
        let mut a = Vec::new();
        let mut b = Vec::new();
        r1.drain_into(&mut a, 100);
        r2.drain_into(&mut b, 100);
        assert_eq!(a.len(), 50);
        assert_eq!(b.len(), 50);
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.price, y.price);
            assert_eq!(x.trade_id, y.trade_id);
        }
    }
}

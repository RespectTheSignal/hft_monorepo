//! Fan-in N×SPSC order ring reader for order-gateway.
//!
//! 전략 VM 은 각자 자기 `vm_id` 의 order ring **writer** 만 쥔다. 이 파일은 그
//! 반대편 — gateway 가 **N 개의 reader** 를 한 번에 돌려가며 polling 해 주문을
//! 수확한다. "한 ring 에 두 producer" 규약은 ShmConfig + role 로 강제되므로,
//! 여기선 단순 round-robin 이면 충분.
//!
//! ## 설계 목표
//!
//! - 한 번의 `poll_batch()` 호출에서 **모든 ring 을 한 바퀴** 돌며 각자 비어있지
//!   않은 만큼 소비. 공정성 (starvation 방지) + 최소 latency.
//! - Gateway 가 주문을 "vm_id 와 함께" 볼 수 있도록 `(vm_id, OrderFrame)` pair 로 반환.
//! - 내부 상태는 `Vec<OrderRingReader>` 와 round-robin 커서만. 할당 없음.
//!
//! ## 성능
//!
//! 각 `try_consume` 은 atomic load 1~2회 + 조건분기. N=64, ring 당 평균 0건이면
//! 총 ~128 cycles. hot frame 이 있는 경우도 한 frame 이 ~40 cycles 라 gateway
//! 주 루프 예산 (μs 대) 안에 들어옴.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::{debug, warn};

use crate::error::ShmResult;
use crate::layout::OrderFrame;
use crate::region::{Role, SharedRegion, SubKind};
use crate::spsc_ring::OrderRingReader;

/// N 개의 SPSC order ring 을 묶어 단일 커서로 soak.
///
/// OrderGateway 가 단독으로 소유하며, `poll_batch` / `run_blocking` 을 통해
/// 내부 `handler` 에게 `(vm_id, frame)` 을 전달한다.
pub struct MultiOrderRingReader {
    readers: Vec<OrderRingReader>,
    /// round-robin 시작 인덱스.
    rr_start: usize,
    /// 총 N.
    n: usize,
}

impl MultiOrderRingReader {
    /// SharedRegion 에서 모든 order ring 에 대한 reader 를 만든다. SharedRegion 의
    /// role 은 [`Role::OrderGateway`] 여야 한다.
    pub fn attach(shared: &SharedRegion) -> ShmResult<Self> {
        if shared.role() != Role::OrderGateway {
            warn!(
                role = ?shared.role(),
                "MultiOrderRingReader::attach called with non-gateway role — proceeding anyway"
            );
        }
        let n = shared.spec().n_max as usize;
        let mut readers = Vec::with_capacity(n);
        for vm_id in 0..shared.spec().n_max {
            let sub = shared.sub_region(SubKind::OrderRing { vm_id })?;
            // writer 가 아직 header 를 안 썼을 수도 있음 → 비어있으면 writer 쪽에서
            // magic==0 init 을 한다. 여기서는 writer 가 초기화를 마쳤다고 가정하고
            // from_region 을 호출 — 헤더가 0 이면 BadMagic 으로 실패.
            // publisher 단독 초기화 규약상 gateway 가 attach 시점엔 이미 초기화되어
            // 있어야 정상이지만, 테스트 편의를 위해 writer 가 없는 ring 은 스킵 옵션을
            // 두고 싶을 수 있음 — 현재는 strict.
            let reader = OrderRingReader::from_region(sub).map_err(|e| {
                warn!(vm_id = vm_id, err = %e, "order ring not ready");
                e
            })?;
            readers.push(reader);
        }
        debug!(n = n, "MultiOrderRingReader attached");
        Ok(Self {
            readers,
            rr_start: 0,
            n,
        })
    }

    /// 테스트 편의용 — SharedRegion 의 writer 가 아직 각 ring 의 header 를 쓰지
    /// 않았을 수도 있는 환경에서, 쓰기 완료된 ring 만 붙인다. 실패한 ring 은
    /// 옵션으로 None 을 갖는다.
    pub fn attach_lenient(shared: &SharedRegion) -> Self {
        let n = shared.spec().n_max as usize;
        let mut readers = Vec::with_capacity(n);
        for vm_id in 0..shared.spec().n_max {
            match shared
                .sub_region(SubKind::OrderRing { vm_id })
                .and_then(OrderRingReader::from_region)
            {
                Ok(r) => readers.push(r),
                Err(e) => {
                    debug!(vm_id = vm_id, err = %e, "skipping uninitialized order ring");
                }
            }
        }
        let n = readers.len();
        Self {
            readers,
            rr_start: 0,
            n,
        }
    }

    /// ring 개수.
    pub fn len(&self) -> usize {
        self.n
    }

    /// empty (= attach 실패).
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// 모든 ring 을 한 바퀴 돌며 `handler` 를 호출. 이번 호출에서 소비한 총 건수.
    ///
    /// `handler` 는 `(vm_id, frame)` 한 건을 받는다. 반환값이 false 면 즉시 중단.
    pub fn poll_batch<F>(&mut self, max_per_ring: usize, mut handler: F) -> usize
    where
        F: FnMut(u32, OrderFrame) -> bool,
    {
        if self.n == 0 {
            return 0;
        }
        let mut consumed = 0usize;
        for step in 0..self.n {
            let idx = (self.rr_start + step) % self.n;
            let reader = &mut self.readers[idx];
            for _ in 0..max_per_ring {
                match reader.try_consume() {
                    Some(frame) => {
                        consumed += 1;
                        if !handler(idx as u32, frame) {
                            self.rr_start = (idx + 1) % self.n;
                            return consumed;
                        }
                    }
                    None => break,
                }
            }
        }
        // 다음 호출은 지금 시작한 ring 다음부터 (fairness).
        self.rr_start = (self.rr_start + 1) % self.n;
        consumed
    }

    /// 지정 ring 한 개만 poll (테스트용).
    pub fn poll_ring(&mut self, vm_id: u32) -> Option<OrderFrame> {
        self.readers
            .get_mut(vm_id as usize)
            .and_then(|r| r.try_consume())
    }

    /// 주 루프: `stop` 이 true 가 될 때까지 반복 polling.
    ///
    /// 배치가 비면 `spin_limit` 번 `spin_loop`, 그래도 비면 `park_timeout` 으로
    /// 대기 (기본 50μs). CPU 100% 고정 핫패스용이면 `park_duration = 0`.
    pub fn run_blocking<F>(
        &mut self,
        stop: &AtomicBool,
        max_per_ring: usize,
        spin_limit: u32,
        park_duration: Duration,
        mut handler: F,
    ) where
        F: FnMut(u32, OrderFrame) -> bool,
    {
        let mut spin_counter: u32 = 0;
        while !stop.load(Ordering::Acquire) {
            let n = self.poll_batch(max_per_ring, &mut handler);
            if n > 0 {
                spin_counter = 0;
                continue;
            }
            if spin_counter < spin_limit {
                std::hint::spin_loop();
                spin_counter += 1;
                continue;
            }
            // backoff — park_timeout 만. spurious wake 는 다음 iteration 의
            // poll_batch 가 0건 반환 → spin counter 재시작 → 다시 park 로 자연
            // 수렴. 보정 sleep 을 두면 정상 park 소진 시 지연 상한이 2배가 되어
            // idle-to-wake latency 가 악화됨.
            if !park_duration.is_zero() {
                std::thread::park_timeout(park_duration);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::OrderKind;
    use crate::region::{Backing, LayoutSpec, Role, SharedRegion};
    use crate::spsc_ring::OrderRingWriter;
    use tempfile::tempdir;

    fn spec(n: u32) -> LayoutSpec {
        LayoutSpec {
            quote_slot_count: 8,
            trade_ring_capacity: 16,
            symtab_capacity: 8,
            order_ring_capacity: 16,
            n_max: n,
        }
    }

    fn make_frame(seq_seed: u64, vm_id: u32) -> OrderFrame {
        OrderFrame {
            seq: 0,
            kind: OrderKind::Place as u8,
            exchange_id: 1,
            _pad1: [0; 2],
            symbol_idx: vm_id,
            side: 0,
            tif: 0,
            ord_type: 0,
            _pad2: [0; 1],
            price: 1000 + seq_seed as i64,
            size: 1,
            client_id: seq_seed,
            ts_ns: 0,
            aux: [0; 5],
            _pad3: [0; 16],
        }
    }

    #[test]
    fn fan_in_across_all_rings() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("mr1");
        let s = spec(4);
        let sr =
            SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
                .unwrap();
        // 전략 VM 4개 각각이 ring writer 를 연다.
        let mut writers = Vec::new();
        for vm_id in 0..s.n_max {
            let sub = sr.sub_region(SubKind::OrderRing { vm_id }).unwrap();
            writers.push(OrderRingWriter::from_region(sub, s.order_ring_capacity).unwrap());
        }
        // gateway 가 attach.
        let gw =
            SharedRegion::open_view(Backing::DevShm { path: p.clone() }, s, Role::OrderGateway)
                .unwrap();
        let mut m = MultiOrderRingReader::attach(&gw).unwrap();
        // 각 ring 에 3건씩.
        for (vm_id, w) in writers.iter().enumerate() {
            for i in 0..3 {
                assert!(w.publish(&make_frame(i as u64 + 10 * vm_id as u64, vm_id as u32)));
            }
        }
        let mut got: Vec<(u32, u64)> = Vec::new();
        let n = m.poll_batch(10, |vm_id, f| {
            got.push((vm_id, f.client_id));
            true
        });
        assert_eq!(n, 12);
        // 각 vm_id 당 3건씩.
        for vm_id in 0..s.n_max {
            let count = got.iter().filter(|(v, _)| *v == vm_id).count();
            assert_eq!(count, 3, "vm_id {} got {}", vm_id, count);
        }
    }

    #[test]
    fn round_robin_is_fair() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("mr2");
        let s = spec(3);
        let sr =
            SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
                .unwrap();
        let mut writers = Vec::new();
        for vm_id in 0..s.n_max {
            let sub = sr.sub_region(SubKind::OrderRing { vm_id }).unwrap();
            writers.push(OrderRingWriter::from_region(sub, s.order_ring_capacity).unwrap());
        }
        let gw =
            SharedRegion::open_view(Backing::DevShm { path: p.clone() }, s, Role::OrderGateway)
                .unwrap();
        let mut m = MultiOrderRingReader::attach(&gw).unwrap();
        // ring 0 에만 5 건 쓰고, 다른 ring 은 빔. poll 이 ring 0 을 전부 소비하고 다른
        // ring 에 대해 poll 해 봄도 수행해야 한다.
        for i in 0..5 {
            writers[0].publish(&make_frame(i, 0));
        }
        let mut got = 0;
        m.poll_batch(3, |_, _| {
            got += 1;
            true
        });
        assert_eq!(got, 3); // max_per_ring=3 제한.
                            // 한 번 더 돌면 남은 2 건.
        let mut got2 = 0;
        m.poll_batch(10, |_, _| {
            got2 += 1;
            true
        });
        assert_eq!(got2, 2);
    }

    #[test]
    fn poll_returns_zero_on_empty() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("mr3");
        let s = spec(2);
        let sr =
            SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
                .unwrap();
        for vm_id in 0..s.n_max {
            let sub = sr.sub_region(SubKind::OrderRing { vm_id }).unwrap();
            let _ = OrderRingWriter::from_region(sub, s.order_ring_capacity).unwrap();
        }
        let gw =
            SharedRegion::open_view(Backing::DevShm { path: p.clone() }, s, Role::OrderGateway)
                .unwrap();
        let mut m = MultiOrderRingReader::attach(&gw).unwrap();
        assert_eq!(m.poll_batch(10, |_, _| true), 0);
    }

    #[test]
    fn lenient_attach_tolerates_missing_rings() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("mr4");
        let s = spec(3);
        let sr =
            SharedRegion::create_or_attach(Backing::DevShm { path: p.clone() }, s, Role::Publisher)
                .unwrap();
        // 0, 2 만 초기화.
        for vm_id in [0, 2] {
            let sub = sr.sub_region(SubKind::OrderRing { vm_id }).unwrap();
            let _ = OrderRingWriter::from_region(sub, s.order_ring_capacity).unwrap();
        }
        let gw =
            SharedRegion::open_view(Backing::DevShm { path: p.clone() }, s, Role::OrderGateway)
                .unwrap();
        let m = MultiOrderRingReader::attach_lenient(&gw);
        assert_eq!(m.len(), 2);
    }
}

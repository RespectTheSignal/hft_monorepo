//! 최근 주문 rate 추적기 — 레거시 `GateOrderManager::recent_orders_queue` 교체.
//!
//! 레거시는 `RwLock<VecDeque<LastOrder>>` 에 심볼 문자열까지 통째로 100개를
//! 쌓아두고 **매 strategy eval 마다 O(100) iter** 로 심볼별 카운트를 계산했다.
//! 여기서는 두 가지 지점을 최적화한다:
//!
//! 1. **ring buffer + parking_lot::Mutex**: `RwLock` 은 `tokio::sync` 가 아닌
//!    이상 write lock 이 들어가면 reader 가 모두 block 된다. `parking_lot` 의
//!    Mutex 는 uncontended 경로가 cmpxchg 1 회로 끝나고 contended 경로도
//!    `RwLock` 보다 짧다. 또 ring buffer (`VecDeque<Entry>`) 에 capacity 를
//!    고정해 alloc 0 운용 (push_back 시 capacity==N 이면 pop_front).
//! 2. **심볼별 카운터 pre-aggregate**: 매 주문마다 `AHashMap<Symbol, u32>` 를
//!    증감시켜 `is_symbol_too_many_orders` 가 매번 100 iter 하는 것을 O(1) 조회로
//!    바꾼다.
//! 3. **recent flag TTL**: 레거시의 `is_recently_too_many_orders` (1시간 윈도우)
//!    는 단일 `i64` 타임스탬프만 필요하므로 atomic 한 개 — 락 없이 load/store.
//!
//! # Ordering / 시간 소스
//! 모든 timestamp 는 ms since epoch. caller (strategy runner) 가 Clock 을 통해 주입한다.
//! 테스트에서는 explicit ts 로 push / check 가능.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;

use ahash::AHashMap;
use parking_lot::Mutex;
use std::collections::VecDeque;

use hft_types::Symbol;

/// 하나의 recent-order 엔트리. LastOrder 를 들고 다니면 OrderLevel/price 등 불필요 필드가
/// 있어, rate 추적에는 심볼 + 타임스탬프만 뽑아둔다 (cache line 압축).
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// Symbol 는 Arc<str> 기반 cheap clone.
    sym_hash: u64,
    timestamp_ms: i64,
}

/// 최근 주문 rate 트래커.
#[derive(Debug)]
pub struct OrderRateTracker {
    /// ring buffer capacity — 레거시는 100 고정.
    capacity: usize,
    /// 카운트 threshold — `is_too_many_orders` 는 buffer 가 가득 차야 true 후보 (레거시와 동일).
    full_threshold: usize,
    /// recent_orders_queue equivalent.
    queue: Mutex<VecDeque<Entry>>,
    /// 심볼별 집계 카운터 — queue 와 lock-step.
    per_symbol: Mutex<AHashMap<u64, u32>>,
    /// 마지막으로 is_too_many_orders 가 true 였던 시각 (ms). 0 = 없음.
    last_too_many_orders_ms: AtomicI64,
    /// 스무스 카운터 (observability) — totally_pushed.
    total_pushed: AtomicI64,
}

impl OrderRateTracker {
    /// 기본 구성: capacity=100, full_threshold=100 (레거시와 동일).
    pub fn new() -> Self {
        Self::with_capacity(100, 100)
    }

    /// capacity 와 full_threshold 를 분리해 조정. 테스트에서 작게.
    pub fn with_capacity(capacity: usize, full_threshold: usize) -> Self {
        Self {
            capacity,
            full_threshold,
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            per_symbol: Mutex::new(AHashMap::with_capacity(capacity / 4 + 1)),
            last_too_many_orders_ms: AtomicI64::new(0),
            total_pushed: AtomicI64::new(0),
        }
    }

    /// 새 주문을 push. queue 가 가득 차면 가장 오래된 엔트리를 drop 하고, 그 심볼의
    /// per_symbol 카운터도 함께 감소.
    pub fn push(&self, symbol: &Symbol, timestamp_ms: i64) {
        let h = symbol_hash(symbol);
        let mut q = self.queue.lock();
        if q.len() == self.capacity {
            if let Some(old) = q.pop_front() {
                // per_symbol lock 은 q 를 잡은 상태에서 take — drop 순서는 nested OK
                // (deadlock 회피: 항상 queue → per_symbol 순서).
                let mut ps = self.per_symbol.lock();
                if let Some(c) = ps.get_mut(&old.sym_hash) {
                    *c = c.saturating_sub(1);
                    if *c == 0 {
                        ps.remove(&old.sym_hash);
                    }
                }
            }
        }
        q.push_back(Entry {
            sym_hash: h,
            timestamp_ms,
        });
        let mut ps = self.per_symbol.lock();
        *ps.entry(h).or_insert(0) += 1;
        self.total_pushed.fetch_add(1, Ordering::Relaxed);
    }

    /// 레거시 `is_too_many_orders` — buffer 가 가득 찼고 (≥full_threshold) 가장 오래된
    /// 엔트리의 시간 간격이 `time_gap_ms` 미만이면 true. side effect: true 일 때
    /// `last_too_many_orders_ms` 를 갱신 (레거시와 호환).
    pub fn is_too_many_orders(&self, now_ms: i64, time_gap_ms: i64) -> bool {
        let q = self.queue.lock();
        if q.len() < self.full_threshold {
            return false;
        }
        let oldest = match q.front() {
            Some(e) => e.timestamp_ms,
            None => return false,
        };
        drop(q);
        let gap = now_ms - oldest;
        if gap < time_gap_ms {
            // 레거시의 last_too_many_orders_time_ms 를 최신화.
            self.last_too_many_orders_ms.store(now_ms, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// 레거시 `is_symbol_too_many_orders` — buffer 가 가득 찼고, 가장 오래된 엔트리
    /// 시간 간격이 `time_gap_ms` 미만이며, 그 심볼의 현재 카운트 > threshold 이면 true.
    pub fn is_symbol_too_many_orders(
        &self,
        symbol: &Symbol,
        now_ms: i64,
        symbol_count_threshold: u32,
        time_gap_ms: i64,
    ) -> bool {
        let q = self.queue.lock();
        if q.len() < self.full_threshold {
            return false;
        }
        let oldest_ts = match q.front() {
            Some(e) => e.timestamp_ms,
            None => return false,
        };
        drop(q);
        if now_ms - oldest_ts >= time_gap_ms {
            return false;
        }
        let h = symbol_hash(symbol);
        let ps = self.per_symbol.lock();
        ps.get(&h).copied().unwrap_or(0) > symbol_count_threshold
    }

    /// 레거시 `is_recently_too_many_orders` — 지난 1시간 내에 is_too_many_orders 가
    /// true 를 반환한 적이 있는가. `now_ms - last > 3_600_000` 이면 false.
    pub fn is_recently_too_many_orders(&self, now_ms: i64) -> bool {
        let last = self.last_too_many_orders_ms.load(Ordering::Relaxed);
        last > 0 && now_ms - last < 3_600_000
    }

    /// 현재 buffer 크기 (observability).
    pub fn len(&self) -> usize {
        self.queue.lock().len()
    }

    /// 현재 buffer 가 비었는지.
    pub fn is_empty(&self) -> bool {
        self.queue.lock().is_empty()
    }

    /// 누적 push 수.
    pub fn total_pushed(&self) -> i64 {
        self.total_pushed.load(Ordering::Relaxed)
    }

    /// 전체 리셋. 테스트/재시작 복구 경로에서 쓰는 hard-reset.
    ///
    /// **운영 주의**: 프로덕션 strategy 루프에서는 본 함수 대신
    /// [`Self::decay_before`] / [`Self::decay_before_now`] 를 사용하라.
    /// full clear 는 `last_too_many_orders_ms` recent flag 까지 지우기 때문에
    /// "최근 1시간 안에 과발주 있었음" 힌트가 소실된다. 거래 일시 중지 직후 완전
    /// 재시작 (PID 교체 / 장애 복구 후) 에만 적합.
    pub fn clear(&self) {
        self.queue.lock().clear();
        self.per_symbol.lock().clear();
        self.last_too_many_orders_ms.store(0, Ordering::Relaxed);
    }

    /// ring buffer 의 가장 오래된 엔트리 timestamp (ms). 비어 있으면 `None`.
    ///
    /// strategy 루프가 `now_ms - oldest > window` 인지 값싸게 확인 후 [`Self::decay_before`]
    /// 호출 여부를 결정할 때 쓴다. lock 한 번만 잡는다.
    pub fn oldest_ts(&self) -> Option<i64> {
        self.queue.lock().front().map(|e| e.timestamp_ms)
    }

    /// **운영용 time-based decay**. `cutoff_ms` **미만** timestamp 를 갖는 엔트리를
    /// front 에서부터 모두 drop 하고, 각 심볼 카운터를 lock-step 으로 감소시킨다.
    /// 반환값은 drop 된 엔트리 수.
    ///
    /// # 언제 호출?
    ///
    /// 1. **strategy eval 끝단**: 매 tick 마다
    ///    `tracker.decay_before(now_ms - trade_settings.too_many_orders_time_gap_ms)` 를 호출하면,
    ///    `is_too_many_orders` 판정 윈도우 밖의 ring 엔트리들이 매번 청소되어
    ///    그 이상 메모리 압력이 쌓이지 않는다. capacity=100 은 상한일 뿐, 평상시엔
    ///    훨씬 짧게 유지돼 `per_symbol` map 도 희소해진다.
    /// 2. **주기적 background**: account poller 또는 heartbeat task 에서 1s 마다
    ///    `decay_before_now(Duration::from_secs(60))` 호출. strategy 가 idle 상태로
    ///    오래 멈춰 있어도 stale 엔트리가 축적되지 않는다.
    ///
    /// # 왜 `clear` 와 다른가
    ///
    /// - `clear` 는 `last_too_many_orders_ms` recent flag 까지 리셋 →
    ///   `is_recently_too_many_orders` 의 1h 쿨다운 창이 조기 종료된다.
    /// - `decay_before` 는 시간 윈도우 밖 엔트리만 드롭하고 flag 는 유지 →
    ///   과발주 방지 로직이 stateful 하게 유지된다.
    ///
    /// # 락 순서
    ///
    /// `queue.lock()` → `per_symbol.lock()` 순으로 **중첩** 획득 (push 와 동일한
    /// 글로벌 순서). deadlock 없음.
    pub fn decay_before(&self, cutoff_ms: i64) -> usize {
        let mut q = self.queue.lock();
        // fast path — 가장 오래된 엔트리가 이미 cutoff 이상이면 할 일 없음.
        match q.front() {
            Some(e) if e.timestamp_ms >= cutoff_ms => return 0,
            None => return 0,
            _ => {}
        }
        let mut ps = self.per_symbol.lock();
        let mut dropped = 0usize;
        // pop_front 반복. ring buffer 는 timestamp 단조 증가 가정 (push 순서대로 찍힌다).
        while let Some(front) = q.front() {
            if front.timestamp_ms >= cutoff_ms {
                break;
            }
            let Entry { sym_hash, .. } = q.pop_front().expect("front was Some");
            if let Some(c) = ps.get_mut(&sym_hash) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    ps.remove(&sym_hash);
                }
            }
            dropped += 1;
        }
        dropped
    }

    /// `decay_before(now_ms - window.as_millis() as i64)` 편의 wrapper.
    ///
    /// overflow 안전: `window` 가 너무 커서 cutoff 가 음수가 되면 drop 대상 없음 (no-op).
    pub fn decay_before_now(&self, now_ms: i64, window: std::time::Duration) -> usize {
        let w_ms = window.as_millis().min(i64::MAX as u128) as i64;
        // now_ms 가 매우 작을 때 (테스트) cutoff underflow 방지.
        let cutoff = now_ms.saturating_sub(w_ms);
        self.decay_before(cutoff)
    }
}

impl Default for OrderRateTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Symbol hash. ahash 한번 — Symbol 은 Arc<str> 이라 포인터 주소로도 되지만 동일 내용
/// 다른 Arc 를 맞추기 위해 내용 기반.
///
/// RandomState 는 프로세스 전역 static (`OnceLock`) 으로 1회 seed. 모든 tracker 가
/// 동일한 해시 키를 써서 per_symbol 집계가 consistent.
#[inline]
fn symbol_hash(sym: &Symbol) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    static RS: OnceLock<ahash::RandomState> = OnceLock::new();
    let rs = RS.get_or_init(|| {
        ahash::RandomState::with_seeds(0x00C0_FFEE, 0xBADF_00D0, 0xDEAD_BEEF, 0x1234_5678)
    });
    let mut h = rs.build_hasher();
    h.write(sym.as_str().as_bytes());
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Symbol {
        Symbol::new(x)
    }

    #[test]
    fn empty_is_not_too_many() {
        let t = OrderRateTracker::with_capacity(10, 10);
        assert!(!t.is_too_many_orders(1000, 500));
        assert!(!t.is_symbol_too_many_orders(&s("BTC_USDT"), 1000, 3, 500));
        assert!(!t.is_recently_too_many_orders(1000));
    }

    #[test]
    fn below_threshold_is_not_too_many() {
        let t = OrderRateTracker::with_capacity(10, 10);
        for i in 0..5 {
            t.push(&s("BTC_USDT"), 900 + i);
        }
        assert!(!t.is_too_many_orders(1000, 500));
    }

    #[test]
    fn full_buffer_within_time_gap_triggers() {
        let t = OrderRateTracker::with_capacity(5, 5);
        for i in 0..5 {
            t.push(&s("BTC_USDT"), 800 + i); // 800..=804
        }
        // now=900, oldest=800, gap=100 < 500 → true
        assert!(t.is_too_many_orders(900, 500));
        // 심볼 카운터 5 > threshold 3 → true
        assert!(t.is_symbol_too_many_orders(&s("BTC_USDT"), 900, 3, 500));
        // 최근 flag 설정되었으므로 1h 이내 true
        assert!(t.is_recently_too_many_orders(900 + 10_000));
        // 1h 이후는 false
        assert!(!t.is_recently_too_many_orders(900 + 3_600_001));
    }

    #[test]
    fn ring_buffer_rotates_and_symbol_counter_stays_in_sync() {
        let t = OrderRateTracker::with_capacity(3, 3);
        t.push(&s("A"), 1);
        t.push(&s("B"), 2);
        t.push(&s("A"), 3);
        // A=2, B=1
        assert!(t.is_symbol_too_many_orders(&s("A"), 100, 1, 1000));
        assert!(!t.is_symbol_too_many_orders(&s("B"), 100, 1, 1000));
        // 새 push → A 앞쪽 drop 됨.
        t.push(&s("C"), 4);
        // 이제 A=1, B=1, C=1. A 더 이상 >1 아님.
        assert!(!t.is_symbol_too_many_orders(&s("A"), 100, 1, 1000));
    }

    #[test]
    fn outside_time_gap_not_too_many() {
        let t = OrderRateTracker::with_capacity(5, 5);
        for i in 0..5 {
            t.push(&s("X"), i);
        }
        // now=10_000, oldest=0, gap=10_000 > 500 → false
        assert!(!t.is_too_many_orders(10_000, 500));
    }

    #[test]
    fn recently_too_many_window_expires() {
        let t = OrderRateTracker::with_capacity(2, 2);
        t.push(&s("A"), 0);
        t.push(&s("A"), 1);
        assert!(t.is_too_many_orders(2, 1000));
        assert!(t.is_recently_too_many_orders(2));
        // 1h+ 지남 → false
        assert!(!t.is_recently_too_many_orders(2 + 3_600_001));
    }

    #[test]
    fn decay_before_drops_old_entries_and_keeps_counters_consistent() {
        let t = OrderRateTracker::with_capacity(10, 10);
        // A@10, B@20, A@30, B@40, C@50
        t.push(&s("A"), 10);
        t.push(&s("B"), 20);
        t.push(&s("A"), 30);
        t.push(&s("B"), 40);
        t.push(&s("C"), 50);
        assert_eq!(t.len(), 5);

        // cutoff=35 → timestamp <35 인 3 개 (10,20,30) drop.
        let dropped = t.decay_before(35);
        assert_eq!(dropped, 3);
        assert_eq!(t.len(), 2);
        // 남은 건 B@40, C@50. A 카운터는 0이 되어 제거, B=1, C=1.
        assert!(!t.is_symbol_too_many_orders(&s("A"), 100, 0, 1000));
        // threshold=0 이면 카운트 1 > 0 이므로 true (buffer full 이 아니어서 false 가 맞음).
        // 실제로 full_threshold=10 인데 len=2 이므로 is_symbol_too_many_orders 는 false.
        assert!(!t.is_symbol_too_many_orders(&s("B"), 100, 0, 1000));
        // oldest_ts 확인.
        assert_eq!(t.oldest_ts(), Some(40));
    }

    #[test]
    fn decay_before_noop_when_all_entries_fresh() {
        let t = OrderRateTracker::with_capacity(5, 5);
        for i in 0..3 {
            t.push(&s("Z"), 1_000 + i);
        }
        let dropped = t.decay_before(500);
        assert_eq!(dropped, 0);
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn decay_before_empty_is_noop() {
        let t = OrderRateTracker::with_capacity(5, 5);
        assert_eq!(t.decay_before(1_000_000), 0);
        assert_eq!(t.oldest_ts(), None);
    }

    #[test]
    fn decay_preserves_recent_flag_unlike_clear() {
        let t = OrderRateTracker::with_capacity(3, 3);
        t.push(&s("A"), 0);
        t.push(&s("A"), 1);
        t.push(&s("A"), 2);
        assert!(t.is_too_many_orders(3, 1000)); // recent flag 찍힘.
        // decay 로 모든 엔트리 제거해도 recent flag 는 살아있어야 함.
        let _ = t.decay_before(100);
        assert_eq!(t.len(), 0);
        assert!(t.is_recently_too_many_orders(500));
    }

    #[test]
    fn decay_before_now_uses_window() {
        let t = OrderRateTracker::with_capacity(5, 5);
        t.push(&s("A"), 1_000);
        t.push(&s("A"), 2_000);
        t.push(&s("A"), 3_000);
        // now=3_500, window=1_000 → cutoff=2_500 → timestamp<2500 인 2 개 drop.
        let dropped = t.decay_before_now(3_500, std::time::Duration::from_millis(1_000));
        assert_eq!(dropped, 2);
        assert_eq!(t.oldest_ts(), Some(3_000));
    }

    #[test]
    fn decay_before_now_does_not_underflow_for_small_now() {
        let t = OrderRateTracker::with_capacity(3, 3);
        t.push(&s("A"), 0);
        // now=0, window=10s → cutoff=saturating_sub → 0, timestamp<0 드롭 없음.
        let dropped = t.decay_before_now(0, std::time::Duration::from_secs(10));
        assert_eq!(dropped, 0);
    }

    #[test]
    fn clear_resets() {
        let t = OrderRateTracker::with_capacity(3, 3);
        for i in 0..3 {
            t.push(&s("A"), i);
        }
        assert_eq!(t.len(), 3);
        t.clear();
        assert_eq!(t.len(), 0);
        assert_eq!(t.total_pushed(), 3); // total_pushed 는 유지 (observability)
        assert!(!t.is_too_many_orders(100, 1000));
    }
}

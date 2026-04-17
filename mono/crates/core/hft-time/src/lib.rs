//! hft-time — 시간 & 지연 측정 primitives
//!
//! ## 설계
//! - `Clock` trait 으로 시간 소스를 추상화. 테스트에서는 `MockClock` 으로 교체.
//! - `SystemClock` 은 wall clock (epoch ms) 과 monotonic nanos 를 동시에 제공.
//!   wall clock 은 거래소 timestamp 와 비교할 때, monotonic 은 stage 간 delta 측정용.
//! - `LatencyStamps` 는 파이프라인 각 stage 에서 찍힌 **wall-clock ms** 와
//!   **monotonic ns** 를 모두 보관 — delta 는 monotonic 로 계산해 시계 점프 영향 제거.
//!
//! ## latency stage 매핑 (mono/docs/phase1/latency-budget.md 와 일치)
//! 1. `exchange_server_ms`  — 거래소가 메시지 방출
//! 2. `ws_received_ms`      — 우리 WS client 가 프레임 수신
//! 3. `serialized_ms`       — C-struct 바이트 직렬화 완료
//! 4. `pushed_ms`           — PUSH 소켓 send 호출 직후 (worker)
//! 5. `published_ms`        — PUB 소켓 publish 직후 (aggregator)
//! 6. `subscribed_ms`       — SUB 소켓 recv 직후 (subscriber)
//! 7. `consumed_ms`         — 전략이 이벤트 꺼내 처리 시작

#![deny(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

// ─────────────────────────────────────────────────────────────────────────────
// Clock trait
// ─────────────────────────────────────────────────────────────────────────────

/// 시간 소스 추상화. hot path 에서 매번 `SystemTime::now()` 호출하지 않도록 DI.
pub trait Clock: Send + Sync {
    /// wall clock (epoch millis). 거래소 server_time 과 비교할 때.
    fn now_ms(&self) -> i64;

    /// monotonic nanos. stage 간 delta 계산 전용 (벽시계 점프 영향 X).
    fn now_nanos(&self) -> u64;

    /// UTC epoch nanoseconds.
    ///
    /// `now_ms()` 는 ms 정밀도, `now_nanos()` 는 monotonic 전용이다.
    /// 주문 timestamp 처럼 wall-clock epoch ns 가 필요한 곳은 이 메서드를 사용한다.
    fn epoch_ns(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SystemClock — 프로덕션용
// ─────────────────────────────────────────────────────────────────────────────

/// 프로덕션 Clock. 내부적으로 `Instant` 기준점을 한 번만 잡아 monotonic 보장.
pub struct SystemClock {
    start: std::time::Instant,
}

impl SystemClock {
    /// 새 SystemClock. `now_nanos` 기준점이 생성 시점.
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    #[inline]
    fn now_ms(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[inline]
    fn now_nanos(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MockClock — 테스트 전용, deterministic
// ─────────────────────────────────────────────────────────────────────────────

/// 테스트용 MockClock. `advance_*` 로 시간 전진.
///
/// Atomic 으로 스레드 안전. `Arc<MockClock>` 형태로 공유하며 사용.
#[derive(Debug)]
pub struct MockClock {
    ms: AtomicI64,
    nanos: AtomicU64,
}

impl MockClock {
    /// 초기 시각 (epoch ms, monotonic nanos).
    pub fn new(initial_ms: i64, initial_nanos: u64) -> Arc<Self> {
        Arc::new(Self {
            ms: AtomicI64::new(initial_ms),
            nanos: AtomicU64::new(initial_nanos),
        })
    }

    /// 0 에서 시작.
    pub fn zero() -> Arc<Self> {
        Self::new(0, 0)
    }

    /// wall clock 을 `delta_ms` 만큼 전진. monotonic 도 같이 전진 (ms → ns).
    pub fn advance_ms(&self, delta_ms: i64) {
        self.ms.fetch_add(delta_ms, Ordering::SeqCst);
        self.nanos
            .fetch_add((delta_ms as u64).saturating_mul(1_000_000), Ordering::SeqCst);
    }

    /// monotonic 만 전진 (테스트에서 wall clock 과 monotonic 이 다른 속도로 흘러야 할 때).
    pub fn advance_nanos(&self, delta: u64) {
        self.nanos.fetch_add(delta, Ordering::SeqCst);
    }

    /// wall clock 절대값 설정.
    pub fn set_ms(&self, v: i64) {
        self.ms.store(v, Ordering::SeqCst);
    }
}

impl Clock for MockClock {
    fn now_ms(&self) -> i64 {
        self.ms.load(Ordering::SeqCst)
    }
    fn now_nanos(&self) -> u64 {
        self.nanos.load(Ordering::SeqCst)
    }
    fn epoch_ns(&self) -> u64 {
        self.now_ms() as u64 * 1_000_000
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Stamp — 한 스테이지의 (wall_ms, mono_ns) 쌍
// ─────────────────────────────────────────────────────────────────────────────

/// 파이프라인 한 stage 에서 기록되는 시각 쌍.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    /// wall clock (epoch ms). 0 = 찍히지 않음.
    pub wall_ms: i64,
    /// monotonic nanos. 0 = 찍히지 않음.
    pub mono_ns: u64,
}

impl Stamp {
    /// 주어진 Clock 에서 현재 시각을 찍음.
    #[inline]
    pub fn now<C: Clock + ?Sized>(clock: &C) -> Self {
        Self {
            wall_ms: clock.now_ms(),
            mono_ns: clock.now_nanos(),
        }
    }

    /// 아직 찍히지 않았는지.
    #[inline]
    pub fn is_zero(self) -> bool {
        self.wall_ms == 0 && self.mono_ns == 0
    }

    /// 다른 stamp 와의 경과 시간 (self 가 나중이라고 가정).
    /// Monotonic 기반 → saturating subtraction.
    #[inline]
    pub fn elapsed_ns_since(self, earlier: Stamp) -> u64 {
        self.mono_ns.saturating_sub(earlier.mono_ns)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LatencyStamps — 파이프라인 7-stage 레코드
// ─────────────────────────────────────────────────────────────────────────────

/// 파이프라인 전 구간의 stage timestamp.
///
/// `check.md §0` 와 `docs/phase1/latency-budget.md` 의 7 stage 정의와 1:1 대응.
/// 각 stage 는 `Stamp` (wall_ms + mono_ns) 쌍으로 기록.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyStamps {
    /// 1) 거래소 server time (wall_ms 만 유의미 — 외부 시계).
    pub exchange_server: Stamp,
    /// 2) WS frame 수신.
    pub ws_received: Stamp,
    /// 3) C-struct 직렬화 완료.
    pub serialized: Stamp,
    /// 4) PUSH 소켓 send 완료 (worker → aggregator).
    pub pushed: Stamp,
    /// 5) PUB 소켓 publish 완료 (aggregator).
    pub published: Stamp,
    /// 6) SUB 소켓 recv 완료 (subscriber).
    pub subscribed: Stamp,
    /// 7) 전략/소비자 처리 시작.
    pub consumed: Stamp,
}

impl LatencyStamps {
    /// 빈 stamps — 모두 0.
    pub const fn new() -> Self {
        Self {
            exchange_server: Stamp { wall_ms: 0, mono_ns: 0 },
            ws_received: Stamp { wall_ms: 0, mono_ns: 0 },
            serialized: Stamp { wall_ms: 0, mono_ns: 0 },
            pushed: Stamp { wall_ms: 0, mono_ns: 0 },
            published: Stamp { wall_ms: 0, mono_ns: 0 },
            subscribed: Stamp { wall_ms: 0, mono_ns: 0 },
            consumed: Stamp { wall_ms: 0, mono_ns: 0 },
        }
    }

    /// exchange_server 는 거래소에서 받은 ms 라 mono_ns 는 알 수 없음.
    /// wall_ms 만 기록.
    pub fn set_exchange_server_ms(&mut self, ms: i64) {
        self.exchange_server.wall_ms = ms;
        // mono_ns 는 0 유지 → delta 기준 전구간은 ws_received 를 기점으로 계산.
    }

    /// 전체 end-to-end (exchange_server_ms → consumed_ms), wall clock 기반.
    /// 서로 다른 머신 시계 가정 → wall_ms diff 만 사용.
    pub fn end_to_end_ms(&self) -> Option<i64> {
        if self.exchange_server.wall_ms == 0 || self.consumed.wall_ms == 0 {
            return None;
        }
        Some(self.consumed.wall_ms - self.exchange_server.wall_ms)
    }

    /// 내부 stage (ws_received → consumed) 경과 nanos. 같은 머신 monotonic.
    pub fn internal_ns(&self) -> Option<u64> {
        if self.ws_received.is_zero() || self.consumed.is_zero() {
            return None;
        }
        Some(self.consumed.elapsed_ns_since(self.ws_received))
    }

    /// stage 간 delta (from → to) nanos, 둘 다 찍혔을 때만 Some.
    pub fn delta_ns(&self, from: Stage, to: Stage) -> Option<u64> {
        let a = self.stamp(from);
        let b = self.stamp(to);
        if a.is_zero() || b.is_zero() {
            return None;
        }
        Some(b.elapsed_ns_since(a))
    }

    /// stage 별 stamp 접근.
    pub fn stamp(&self, stage: Stage) -> Stamp {
        match stage {
            Stage::ExchangeServer => self.exchange_server,
            Stage::WsReceived => self.ws_received,
            Stage::Serialized => self.serialized,
            Stage::Pushed => self.pushed,
            Stage::Published => self.published,
            Stage::Subscribed => self.subscribed,
            Stage::Consumed => self.consumed,
        }
    }

    /// stage 에 현재 시각을 기록.
    pub fn mark<C: Clock + ?Sized>(&mut self, stage: Stage, clock: &C) {
        let s = Stamp::now(clock);
        match stage {
            Stage::ExchangeServer => self.exchange_server = s,
            Stage::WsReceived => self.ws_received = s,
            Stage::Serialized => self.serialized = s,
            Stage::Pushed => self.pushed = s,
            Stage::Published => self.published = s,
            Stage::Subscribed => self.subscribed = s,
            Stage::Consumed => self.consumed = s,
        }
    }
}

/// 파이프라인 stage enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// 1) 거래소 server.
    ExchangeServer,
    /// 2) WS 프레임 수신.
    WsReceived,
    /// 3) C-struct 직렬화.
    Serialized,
    /// 4) PUSH 완료.
    Pushed,
    /// 5) PUB 완료.
    Published,
    /// 6) SUB 수신.
    Subscribed,
    /// 7) 소비자 처리 시작.
    Consumed,
}

// ─────────────────────────────────────────────────────────────────────────────
// 테스트
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_clock_advance_ms_also_advances_nanos() {
        let c = MockClock::new(1000, 0);
        assert_eq!(c.now_ms(), 1000);
        assert_eq!(c.now_nanos(), 0);
        c.advance_ms(5);
        assert_eq!(c.now_ms(), 1005);
        assert_eq!(c.now_nanos(), 5_000_000);
    }

    #[test]
    fn mock_clock_advance_nanos_only() {
        let c = MockClock::zero();
        c.advance_nanos(500);
        assert_eq!(c.now_ms(), 0);
        assert_eq!(c.now_nanos(), 500);
    }

    #[test]
    fn stamp_elapsed_saturates() {
        let a = Stamp { wall_ms: 100, mono_ns: 1_000 };
        let b = Stamp { wall_ms: 200, mono_ns: 500 };
        // b 가 a 보다 앞선 mono_ns 면 0 으로 saturate
        assert_eq!(b.elapsed_ns_since(a), 0);
    }

    #[test]
    fn latency_stamps_mark_and_delta() {
        let clock = MockClock::new(1_000, 0);
        let mut ls = LatencyStamps::new();

        ls.set_exchange_server_ms(950);
        clock.advance_ms(2);
        ls.mark(Stage::WsReceived, clock.as_ref());
        clock.advance_nanos(500_000);
        ls.mark(Stage::Serialized, clock.as_ref());
        clock.advance_nanos(2_500_000);
        ls.mark(Stage::Pushed, clock.as_ref());
        clock.advance_nanos(1_000_000);
        ls.mark(Stage::Published, clock.as_ref());
        clock.advance_nanos(500_000);
        ls.mark(Stage::Subscribed, clock.as_ref());
        clock.advance_nanos(200_000);
        ls.mark(Stage::Consumed, clock.as_ref());

        // ws_received → serialized = 500us
        assert_eq!(
            ls.delta_ns(Stage::WsReceived, Stage::Serialized),
            Some(500_000)
        );
        // serialized → pushed = 2.5ms
        assert_eq!(
            ls.delta_ns(Stage::Serialized, Stage::Pushed),
            Some(2_500_000)
        );
        // internal ws → consumed
        assert_eq!(ls.internal_ns(), Some(4_700_000));
        // e2e = consumed_wall (1002) - exchange_server_wall (950) = 52ms
        assert_eq!(ls.end_to_end_ms(), Some(52));
    }

    #[test]
    fn delta_ns_is_none_when_stage_missing() {
        let ls = LatencyStamps::new();
        assert_eq!(ls.delta_ns(Stage::WsReceived, Stage::Pushed), None);
    }

    #[test]
    fn system_clock_monotonic_increases() {
        let c = SystemClock::new();
        let a = c.now_nanos();
        // busy sleep
        for _ in 0..1000 {
            std::hint::black_box(());
        }
        let b = c.now_nanos();
        assert!(b >= a);
    }

    #[test]
    fn system_clock_epoch_ns_is_reasonable() {
        let clock = SystemClock::new();
        let ns = clock.epoch_ns();
        assert!(ns > 1_767_225_600_000_000_000);
        let ms_as_ns = clock.now_ms() as u64 * 1_000_000;
        assert!((ns as i64 - ms_as_ns as i64).unsigned_abs() < 1_000_000_000);
    }

    #[test]
    fn mock_clock_epoch_ns_matches_ms() {
        let mock = MockClock::new(1_700_000_000_000, 0);
        let ns = mock.epoch_ns();
        assert_eq!(ns, 1_700_000_000_000 * 1_000_000);
    }
}

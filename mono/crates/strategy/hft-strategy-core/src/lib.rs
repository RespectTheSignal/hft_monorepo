//! 전략 코어 — 시그널 계산 + 주문 결정 + 리스크 체크.
//!
//! Phase 2 리팩토링 목표: **안정성 + 효율성 + 성능** 3축 상향.
//! 레거시 `gate_hft_rust::{signal_calculator, order_decision, handle_chance}` 를
//! 다음 관점에서 재조립:
//!
//! * `String` side/level → `&'static str` enum-ish. Hot path 의 String alloc 제거.
//! * `f64` 가격/사이즈 → 레거시와 시맨틱 동일하되, `#[inline]` 로 signal 계산 함수를
//!   aggressive inlining. 레거시 `SignalResult` clone 은 호출부 구조에 따라 필요했으나
//!   본 crate 에서는 값을 소비하는 패턴을 우선시한다.
//! * 주문 타이밍 제한의 랜덤 범위 (예: 400..=600 ms) 는 호출부에서 thread-local RNG 로
//!   한번만 draw 해 `sample_jitter` 헬퍼로 전달. 레거시처럼 handle_chance 내부에서
//!   매번 `rand::thread_rng()` 호출하는 구조는 latency jitter 의 잠재적 원인이라
//!   외부에서 주입하는 형태로 바꿈.
//! * 포지션/계정 쿼리는 trait 으로 추상화 (`PositionOracle`) → 테스트 용이성 + 향후
//!   SHM/RPC 기반 조회 구현을 손쉽게 swap.

use std::sync::atomic::{AtomicI64, Ordering};

pub mod decision;
pub mod risk;
pub mod signal;

pub use decision::{
    decide_order, decide_order_v6, decide_order_v7, decide_order_v8, OrderDecision, OrderLevel,
    OrderSide, V6DecisionCtx, V7DecisionExtras, V8DecisionCtx,
};
pub use risk::{Chance, ExposureSnapshot, PositionOracle, RiskCheck, RiskConfig};
pub use signal::{
    calculate_signal, calculate_signal_v6, calculate_signal_v8, BookTickerSnap, GateContract,
    SignalResult, TradeSnap,
};

/// 간단한 monotonic 카운터. v8 의 `LAST_TOO_MANY_ORDERS_WARN_MS` 류를
/// 전역 AtomicI64 난사하지 않도록 명시적 타입으로 감싼다.
#[derive(Debug, Default)]
pub struct AtomicMs(pub AtomicI64);

impl AtomicMs {
    #[inline]
    pub fn load(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
    #[inline]
    pub fn store(&self, v: i64) {
        self.0.store(v, Ordering::Relaxed);
    }
    /// `last < now - gap_ms` 이면 갱신하고 true. throttled logging 패턴.
    pub fn should_fire(&self, now_ms: i64, gap_ms: i64) -> bool {
        let last = self.load();
        if last < now_ms - gap_ms {
            self.store(now_ms);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_ms_should_fire_throttles() {
        let am = AtomicMs::default();
        assert!(am.should_fire(10_000, 1000));
        assert!(!am.should_fire(10_500, 1000)); // 500ms 만 지남
        assert!(am.should_fire(11_001, 1000)); // 1001ms 지남
    }
}

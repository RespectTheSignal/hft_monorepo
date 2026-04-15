//! warmup_readiness — publisher 의 warmup 단계가 실제 hot path 이벤트 전에
//! 종료되어 jitter 가 사라지는지 검증.
//!
//! # 상태
//! Phase 1 에선 **스펙 자리 홀더**. `hft_testkit::WarmUp` 는 이미 reusable 한 모듈로 빠져 있지만,
//! publisher 의 warmup 종료 시점을 hot loop 경계에서 정확히 측정하려면 publisher 내부에
//! `is_warm()` probe 를 노출해야 한다 (Phase 2 작업).
//!
//! # Phase 2 구현 스케치
//! - `WarmupConfig { enabled: true, events: 5000 }` 로 publisher 기동.
//! - live feed 가 도착하기 전에 `is_warm()` 이 `true` 로 전환되는지 polling.
//! - warmup 중과 warmup 이후의 per-event latency HDR 비교 → 후자 p99 가 전자의 1/2 이하인지 확인
//!   (JIT warming 효과 검증).
//!
//! 실행: `cargo test --test warmup_readiness -- --ignored`.

#![allow(clippy::unwrap_used)]

#[test]
#[ignore = "SPEC placeholder — Phase 2 wires publisher::is_warm probe"]
fn warmup_completes_before_live_feed() {
    unimplemented!("Phase 2: assert warmup flag flips before first live event");
}

#[test]
#[ignore = "SPEC placeholder — Phase 2 compares pre/post warmup latency distributions"]
fn post_warmup_latency_improves_over_cold() {
    unimplemented!("Phase 2: assert post-warmup p99 latency ≤ cold p99 / 2");
}

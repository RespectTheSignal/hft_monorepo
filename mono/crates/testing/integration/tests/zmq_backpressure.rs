//! zmq_backpressure — ZMQ PUSH/SUB 포화 상황에서 `Downstream::push` 의 fall-back 검증.
//!
//! # 상태
//! Phase 1 에선 **스펙 자리 홀더**. 아래 두 가지 이유로 `#[ignore]` 로 남겨둔다:
//! 1. CI 컨테이너에서 zmq `inproc://` endpoint 는 시그널 누수 가능성이 있어 `cargo test`
//!    병렬 실행 시 다른 테스트와 간섭할 수 있다. Phase 2 에서 전용 전용 subprocess runner 로 격리 예정.
//! 2. `services/publisher` 가 아직 Phase 2 튜닝 단계이며, `drain_batch_cap` / `hwm` 조합의
//!    공식 임계치 수치가 확정돼야 "p99 latency within 50ms even at overflow" 를 증명할 수 있다.
//!
//! # Phase 2 구현 스케치
//! - tokio multi-thread runtime 에서 aggregator 2개를 스폿 up.
//! - `Downstream::push` 를 `hwm` 에 근접한 burst 로 호출.
//! - `DownstreamBusy` 누적 카운트 → aggregator 가 recover 후 drain 하는지 확인.
//! - HDR 으로 per-event latency 기록 → 50 ms p99 가드.
//!
//! 모든 테스트가 `#[ignore]` 표시이므로 기본 `cargo test` 는 skip.
//! 실행: `cargo test --test zmq_backpressure -- --ignored`.

#![allow(clippy::unwrap_used)]

#[test]
#[ignore = "SPEC placeholder — Phase 2 implements real ZMQ backpressure harness"]
fn zmq_push_saturation_yields_downstream_busy() {
    unimplemented!("Phase 2: saturate PUSH socket beyond hwm and assert DownstreamBusy count");
}

#[test]
#[ignore = "SPEC placeholder — Phase 2 implements latency-under-overflow harness"]
fn latency_p99_under_50ms_at_overflow() {
    unimplemented!("Phase 2: measure e2e latency while aggregator is overflowing");
}

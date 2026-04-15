//! crash_recovery_storage — QuestDB ILP writer 의 spool → resume 파이프라인 검증.
//!
//! # 상태
//! Phase 1 에선 **스펙 자리 홀더**. `hft-storage` 의 `QuestDbSink` 는 `spool_dir` 에 ILP
//! 바이트를 임시 저장하는 fallback 까진 구현됐지만, 프로세스 재기동 후 `spool_dir` 스캔 →
//! resume 재전송 로직은 Phase 2 에서 `hft-storage::spool::replay()` 로 추가된다.
//!
//! # Phase 2 구현 스케치
//! - 임시 `spool_dir` 에 10 000 개의 ILP row 가 들어간 spool 파일 생성.
//! - `QuestDbSink::new(spool_dir)` 이 startup 시 `spool::replay()` 를 호출해 테스트 더미 서버에
//!   re-push 하는지 확인.
//! - replay 도중 실패 시 재시도 간격이 `reconnect_backoff_ms` 지수 backoff 를 따르는지 확인.
//!
//! 실행: `cargo test --test crash_recovery_storage -- --ignored`.

#![allow(clippy::unwrap_used)]

#[test]
#[ignore = "SPEC placeholder — Phase 2 implements QuestDbSink spool replay"]
fn spooled_rows_replay_on_restart() {
    unimplemented!("Phase 2: restart QuestDbSink and assert spool files are drained");
}

#[test]
#[ignore = "SPEC placeholder — Phase 2 verifies exponential backoff on replay failure"]
fn replay_respects_reconnect_backoff() {
    unimplemented!("Phase 2: simulate intermittent DB and observe backoff sequence");
}

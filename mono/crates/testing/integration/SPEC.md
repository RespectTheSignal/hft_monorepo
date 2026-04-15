# testing/integration — SPEC

## 역할
여러 crate 를 한꺼번에 엮는 end-to-end 통합 테스트 모음. `publish = false`.

각 테스트 파일 = 한 시나리오.

## Phase 1 테스트 목록

### `tests/e2e_pipeline_mock.rs`
- MockFeed → Worker → Aggregator → SubSocket → Strategy (NoopStrategy)
- 1만 건 BookTicker 주입
- stage 3~7 각각의 p99.9 assert
- 전체 ExchangeServer → Consumed p99.9 < 50ms (MockClock 이 네트워크 stage 의 delay 를 합성)

### `tests/wire_compat.rs`
- 기존 gate_hft 의 바이트 출력과 `hft-protocol::encode_*` 결과가 동일한지
- 테스트 벡터 = `tests/fixtures/*.bin`

### `tests/config_layering.rs`
- default → TOML → env 순서로 override 정확한지
- supabase 는 mock (별도 mock server)

### `tests/zmq_backpressure.rs`
- SUB 를 일부러 slow consumer 로 → publisher HWM 초과 → `WouldBlock` drop
- drop metric 이 정확히 증가
- hot p99.9 는 이 동안에도 유지 (send_dontwait 덕분)

### `tests/crash_recovery_storage.rs`
- questdb-export 를 죽임
- publisher 는 영향 0 (독립 프로세스 시뮬)
- spool 파일 생성 확인
- 재기동 시 replay

### `tests/warmup_readiness.rs`
- WarmUp 실행 전엔 readiness probe red
- warm-up 1만 건 후 green
- warm-up 중 dropped 0

## 계약
- 이 crate 는 다른 crate 의 library API 만 사용. main.rs 직접 호출 금지 (이미 `hft-testkit` 에 library-fied).
- 각 테스트는 self-contained — ZMQ inproc, MockClock.
- CI 없이도 `cargo test -p integration` 로 돌아야 함.

## Phase 1 TODO
1. 위 6개 테스트를 하나씩 `tests/*.rs` 에 작성.
2. fixtures 디렉토리에 wire 호환성 테스트 벡터 복사 (기존 gate_hft 의 샘플 사용).

## 완료 조건
- [ ] 6개 테스트 모두 통과
- [ ] 각 테스트 실행 시간 < 5s (CI 부담 적게)
- [ ] `cargo test -p integration` 이 `cargo test --workspace` 와 같은 결과

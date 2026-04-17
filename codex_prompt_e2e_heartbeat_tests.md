# Codex Prompt: E2E Heartbeat + Reject 통합 테스트

## 목적
기존 `e2e_strategy_result.rs` 와 동일한 in-process 스타일로,
heartbeat liveness guard 와 주문 reject 경로를 검증하는 통합 테스트를 추가한다.

## 파일 위치
`mono/crates/testing/integration/tests/e2e_heartbeat_liveness.rs` (신규)

## 선행 지식
- `strategy/src/main.rs` 의 `GatewayLiveness` 구조체: `last_heartbeat_ns: Arc<AtomicU64>`, `timeout_ns: u64`
  - `is_alive()` → `last_heartbeat==0 → true (startup grace)`, `timeout_ns==0 → true (disabled)`, 그 외 `now - last < timeout`
  - `touch(ts_ns)` → `last_heartbeat_ns.store(ts_ns, Release)`
- `spawn_order_drain_with_now(rx, egress, cancel, now_fn, liveness)` — liveness guard 가 stale 이면 주문을 drop 하고 `counter_inc(StrategyGatewayStale)`
- `spawn_result_listener_with_context(...)` — heartbeat wire 를 받으면 `liveness.touch(ts_ns)` 호출
- `build_heartbeat_wire(ts_ns) -> OrderResultWire` (hft-protocol)
- `is_heartbeat_wire(buf) -> bool`
- `STATUS_HEARTBEAT: u8 = 255`, `STATUS_ACCEPTED: u8 = 0`, `STATUS_REJECTED: u8 = 1`
- `OrderResultWire.encode(&self, buf)` / `OrderResultWire::decode(buf)`
- `e2e_strategy_result.rs` 의 패턴: SingleShotStrategy, ingress_from_order, result_info_from_wire

## 테스트 케이스 (3개)

### Test 1: `heartbeat_keeps_drain_alive`
1. `GatewayLiveness::new(5_000_000_000)` (5초 timeout) 생성
2. MockClock 기반 `now_fn` 사용 (deterministic)
3. drain 시작 전 `liveness.touch(now_fn())` 로 초기 heartbeat 설정
4. SingleShotStrategy 가 주문 1건 발행
5. drain 이 주문을 정상 전달하는지 확인 (egress mock: 채널로 주문 수신)
6. assert: 주문이 drop 되지 않음

### Test 2: `stale_gateway_drops_orders`
1. `GatewayLiveness::new(100_000_000)` (100ms timeout) 생성
2. MockClock 기반 `now_fn` → 초기값 200_000_000 (200ms) — liveness.touch(0) 로 시작
3. `now_fn` 이 항상 200ms 를 반환하도록 — last_heartbeat=0 은 startup grace 이므로
   touch(50_000_000) 으로 50ns 에 한 번 찍고, now=200ms 이면 150ms > 100ms → stale
4. SingleShotStrategy 가 주문 1건 발행
5. drain 이 주문을 drop 하는지 확인 (egress mock: 채널에 아무것도 안 옴, 500ms timeout)
6. assert: 주문 미수신

### Test 3: `gateway_reject_reaches_strategy`
기존 `strategy_gateway_result_roundtrip_accepted` 의 변형:
1. `NoopExecutor` 대신 `RejectExecutor` 를 만든다 (place_order → Err or STATUS_REJECTED)
   - 가장 간단한 방법: `NoopExecutor` 를 쓰되, gateway `emit_result_wire` 에서
     status=REJECTED 를 생성하는 경로를 타게 한다.
   - 또는 `RejectingExecutor` 를 별도 정의: `place_order -> Err(ApiError::Rejected("test"))`.
2. SingleShotStrategy 발행 → gateway 가 STATUS_REJECTED result wire 생성
3. strategy 가 `on_order_result` 에서 `ResultStatus::Rejected` 를 받는지 확인

## 코딩 스타일
- 한국어 주석 + doc comments (/// 형태)
- `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`
- 기존 `e2e_strategy_result.rs` 의 헬퍼 함수 (`encode_zero_padded`, `ingress_from_order` 등) 를 복사하지 말고, 공통 module 로 추출하거나 각 테스트에서 inline 으로 재정의해도 OK (테스트 간 의존성 최소화 우선)
- timeout 은 500ms~1s 으로 짧게 (CI 환경에서도 빠르게 통과)

## 필수 Build Gate (커밋 전 반드시 수행)
```bash
cargo check --workspace --all-targets
cargo test --workspace -j 2
cargo clippy --workspace --all-targets -- -D warnings
```
3개 전부 녹색이 아니면 커밋하지 마세요.

## check.md 업데이트
변경 없음 (이 테스트는 기존 항목의 보강이지 새 항목이 아님).

## Cargo.toml 업데이트
`mono/crates/testing/integration/Cargo.toml` 에 필요한 dep 가 이미 있는지 확인.
`hft-protocol`, `hft-time`, `strategy`, `order-gateway`, `hft-testkit`, `hft-exchange-api`, `hft-types` 가 필요.
없으면 추가.

# Phase 1 — Core Types & Low-Jitter Pipeline 기반

## 목표

**exchange WS → publisher → ZMQ → subscriber → strategy** 파이프라인을
end-to-end **50ms 이내, p99.9 기준 tail spike 0** 로 안정화할 수 있는
뼈대 타입·trait·핫패스 규약을 확정한다.

Phase 1 끝나면:
- `hft-types`, `hft-protocol`, `hft-time`, `hft-telemetry` 실구현 완성
- `hft-zmq`, `hft-storage`, `hft-config` 실구현 완성
- `hft-exchange-api` trait 시그니처 확정 + `hft-testkit` 의 `MockFeed` 가 compile + 50ms budget 테스트 통과
- 실 거래소 구현체 (Phase 2) 를 붙이기만 하면 end-to-end 가 바로 돌아가는 상태

## 이 디렉토리의 문서

| 문서 | 내용 |
|---|---|
| `latency-budget.md` | 50ms 예산을 stage 별로 쪼갠 내역 + 각 stage 의 측정 지점 |
| `jitter-playbook.md` | duration spike 의 근본 원인 7가지 + 각각의 대응책. 모든 crate 가 공유하는 규율 |
| `module-wiring.md` | 각 crate 가 누구를 어떻게 부르는지, 데이터가 어느 방향으로 흐르는지 |
| `hot-path-rules.md` | 핫패스 코드에서 지켜야 할 금기·권장 사항 체크리스트 |
| `phase1-todo.md` | Phase 1 에서 해야 할 작업을 crate 순서로 나열 (의존성 고려한 진행 순서) |

## 핵심 원칙 (모든 crate 공통)

1. **Hot path 에서 heap alloc 금지.** 버퍼는 미리 잡는다. 새 버퍼 필요하면 `Vec::with_capacity` + `pool` 패턴.
2. **Hot path 에서 syscall 최소화.** ZMQ 는 non-blocking + HWM flow control, 로그는 `info!` 까지만 hot path, `debug!/trace!` 는 feature gate 로 컴파일 차단.
3. **Latency 는 stage 별 timestamp 로 측정.** `LatencyStamps` struct 에 각 단계 timestamp 찍어 전달 → downstream 에서 `now - stage_ms` 로 구간별 지연 추정.
4. **mimalloc 전역 allocator** 로 모든 서비스가 같은 allocator 쓰도록 강제. `hft-types` 에서 `#[global_allocator]` 선언하고 서비스 바이너리가 `use hft_types::ALLOCATOR`.
5. **cache-line 정렬 (64B)** 이 필요한 공유 struct 는 `#[repr(C, align(64))]`, false sharing 가능성 있는 atomic counter 는 `CachePadded<AtomicU64>`.
6. **Clock 은 항상 `hft_time::Clock` trait 경유.** `SystemTime::now()` / `Instant::now()` 직접 호출 금지 (테스트 불가, 벤치 불가).
7. **설정 값은 `hft-config` 에서만 로드.** 코드 안에 magic number 금지. 핫패스에 영향 주는 값 (HWM, batch cap, WS reconnect backoff) 은 전부 Config struct 필드.

## 진행 순서 (의존성 bottom-up)

이 순서대로 짜면 각 단계마다 바로 아래 crate 들이 이미 컴파일돼 있어 통합 마찰이 없다:

1. `hft-types` → `hft-time` → `hft-protocol` (core, 서로만 참조)
2. `hft-telemetry` (core 위)
3. `hft-config` (core 위)
4. `hft-zmq` → `hft-shm` → `hft-ipc` (transport, core + telemetry 위)
5. `hft-storage` (transport 와 독립, core + telemetry 위)
6. `hft-exchange-api` (core 위. 구현체는 Phase 2)
7. `hft-testkit` (위 전부 위. `MockFeed` 로 파이프라인 단독 검증)
8. `publisher` / `subscriber` 서비스 (Phase 1 말 또는 Phase 2 진입점)

각 crate 의 `SPEC.md` 가 해당 crate 를 시작할 때 읽을 단일 소스.

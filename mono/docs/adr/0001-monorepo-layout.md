# ADR 0001: Monorepo Layout

- **Status**: Accepted
- **Date**: 2026-04-15
- **Author**: sigma13-K

## Context

기존 `gate_hft` repo 는 여러 독립 프로젝트가 루트에 평탄하게 나열된 구조:
`data_publisher_rust/`, `data_subscriber/`, `gate_hft_rust/`, `order_processor_go/`,
`questdb_export/`, `futures_collector/`, `latency_test_subscriber/` …

공통 타입·설정·로깅이 각 프로젝트에 복제돼 drift 발생, 의존성 버전 불일치, 테스트 어려움.

별도로 `hft_monorepo` repo 에는 Flipster 전용 트랙 (`feed/`, `strategy_flipster_python/`)
이 이미 존재.

## Decision

`hft_monorepo` 를 단일 cargo workspace 의 루트로 삼아, `gate_hft` 의 기능들을 처음부터
재작성해 이 workspace 로 흡수. Go 코드(`order-processor`)는 browser automation 특성상
Go 유지.

Flipster 트랙(`feed/`, `strategy_flipster_python/`)은 workspace `exclude` 로 독립 유지.

## Structure

```
hft_monorepo/
├── crates/
│   ├── core/       # hft-types, hft-protocol, hft-config, hft-time
│   ├── obs/        # hft-telemetry (tracing + OTel)
│   ├── transport/  # hft-zmq, hft-shm, hft-ipc
│   ├── storage/    # hft-storage (QuestDB)
│   ├── exchange/   # hft-exchange-api + 거래소별 구현
│   ├── services/   # publisher, subscriber, strategy, order-gateway (binary)
│   ├── tools/      # latency-probe, questdb-export, futures-collector (binary)
│   └── testing/    # hft-testkit, integration
├── go/order-processor/
├── deploy/
├── configs/
├── docs/
│
├── feed/                       # workspace 외부 — Flipster 전용 트랙
└── strategy_flipster_python/   # workspace 외부 — Flipster 전용 트랙
```

## Consequences

**Positive**:
- `[workspace.dependencies]` 로 tokio/serde/tracing 버전 단일화
- 단일 `target/` → 빌드 캐시 공유, incremental build 속도 ↑
- 모든 서비스가 같은 allocator (mimalloc) 공유 → 런타임 캐시 locality 유리
- 서비스끼리 내부 crate 교차 참조 용이 (예: `publisher + subscriber` 한 바이너리로 합칠 수도 있음)

**Negative**:
- workspace 전체 rebuild 트리거가 많아질 수 있음 → `cargo check -p <crate>` 패턴 권장
- Flipster 트랙이 외부에 있어, 최종 합류 시 `exclude` 에서 빼고 `members` 에 편입 필요

**Neutral**:
- 기존 `gate_hft` repo 의 코드는 참조용으로만 유지. 점진 마이그레이션.

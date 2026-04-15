# mono — HFT Monorepo (rust/go)

단일 cargo workspace + Go 서브모듈로 구성된 HFT 시스템.

> **Status**: Phase 0 스캐폴드 완료, Phase 1 진입 전.
>
> Phase 1 진입 전 필독: `docs/phase1/README.md` (latency budget, jitter playbook, module wiring, hot path rules, step-by-step TODO).
> 각 crate 안의 `SPEC.md` 가 해당 crate 를 시작할 때 읽는 단일 소스.

## 빠른 시작

```bash
# workspace 전체 컴파일 (mono/ 안에서)
make build

# 테스트
make test

# 포맷·린트
make fmt lint
```

## 디렉토리

```
hft_monorepo/                      # 레포 루트
├── mono/                          # ← 이 파일이 있는 곳 (작업 영역)
│   ├── crates/                    # Rust workspace
│   │   ├── core/                  # 도메인 타입·프로토콜·설정·시간
│   │   ├── obs/                   # 관측 (tracing + OTel + HDR histogram)
│   │   ├── transport/             # ZMQ·SHM·IPC 래퍼
│   │   ├── storage/               # QuestDB writer
│   │   ├── exchange/              # 거래소 추상화 + 구현
│   │   ├── services/              # 실행 바이너리
│   │   ├── tools/                 # 분석·유틸 CLI
│   │   └── testing/               # testkit + integration
│   ├── go/order-processor/        # Go 브라우저 자동화 서비스
│   ├── deploy/                    # tmux / systemd / 스크립트
│   ├── configs/                   # 런타임 설정
│   └── docs/                      # ADR + phase 별 설계 문서
│       ├── adr/
│       └── phase1/                # Phase 1 진입 전 필독
│
├── feed/                          # Flipster 전용 피드 — 기존 트랙 (건드리지 않음)
└── strategy_flipster_python/      # Flipster 전략 — 기존 트랙 (건드리지 않음)
```

## 설계 원칙

1. **단일 workspace / 단일 allocator** — 캐시 locality 극대화 (mimalloc 공유).
2. **거래소는 trait 뒤에 숨긴다** — `hft-exchange-api::ExchangeFeed` / `ExchangeExecutor`.
3. **타입과 와이어 포맷 분리** — `hft-types` (Rust-idiomatic) / `hft-protocol` (C-struct 120B).
4. **Latency는 stage별 timestamp 로 명시 계측** — `hft-time::LatencyStamps`.
5. **서비스 간 규약은 `hft-protocol::topics`에 집중** — 토픽/채널 이름을 한 곳에서.
6. **관측은 `tracing` + OTel + HDR histogram** — span duration 으로 자동 latency 측정.

자세한 내용은 `docs/adr/` 를 참고.

## Flipster 트랙과의 관계

`feed/` 와 `strategy_flipster_python/` 은 Flipster 거래소 전용 코드로, 별도 트랙에서
계속 개발 중. 본 workspace 의 `exclude` 설정으로 독립 빌드 유지. 향후 Flipster 를
workspace 에 합류시킬 때는 `hft-exchange-api::ExchangeFeed` / `ExchangeExecutor`
trait 만 만족시키면 됨.

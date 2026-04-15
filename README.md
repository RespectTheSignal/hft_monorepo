# hft_monorepo

두 개의 독립 트랙을 같은 레포에 둔 모노레포.

```
hft_monorepo/
├── mono/                           # 현재 작업 중인 rust/go 모노레포 (단일 cargo workspace)
│   ├── crates/                     # rust crates (24개)
│   ├── go/order-processor/         # go 주문 프로세서 (browser automation)
│   ├── deploy/                     # docker-compose / systemd / scripts
│   ├── configs/                    # 런타임 config (env / supabase 로드 대상)
│   └── docs/                       # ADR + phase별 설계 문서
│
├── feed/                           # Flipster 전용 track (건드리지 않음)
└── strategy_flipster_python/       # Flipster 전용 track (건드리지 않음)
```

## 현재 작업

`mono/` 안에서 기존 `gate_hft` repo 의 기능을 처음부터 재작성 중.
상세는 `mono/README.md`, `mono/docs/phase1/README.md` 참고.

Flipster 트랙 (`feed/`, `strategy_flipster_python/`) 은 별도로 유지되며
`mono/Cargo.toml` 의 workspace `exclude` 로 빠져 있음.

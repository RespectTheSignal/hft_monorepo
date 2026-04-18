# HFT Monorepo — 코드베이스 위키

> **대상 독자**: 개발자, 퀀트 엔지니어, 코드 리뷰어
>
> **범위**: `mono/` Cargo workspace 전체 (39 크레이트, 12 카테고리)
>
> **최종 업데이트**: Phase 3 완료 (commit `f40f731`, 2026-04-18)

---

## 목차

1. [전체 아키텍처](#1-전체-아키텍처)
2. [Workspace 구조](#2-workspace-구조)
3. [Core Layer](#3-core-layer) — 도메인 타입, 와이어 프로토콜, 설정, 시간
4. [Observability Layer](#4-observability-layer) — 텔레메트리, 메트릭, 히스토그램
5. [Infrastructure Layer](#5-infrastructure-layer) — 공통 유틸, Redis 상태
6. [Account Layer](#6-account-layer) — 멀티 서브계정, 마진 모니터링
7. [Transport Layer](#7-transport-layer) — ZMQ, SHM, IPC, Order Egress
8. [Storage Layer](#8-storage-layer) — QuestDB ILP, Parquet
9. [Exchange Layer](#9-exchange-layer) — 거래소 피드, REST, 실행기
10. [Strategy Layer](#10-strategy-layer) — 시그널, 의사결정, 리스크, 캐시
11. [Services Layer](#11-services-layer) — Publisher, Gateway, Strategy, Monitoring
12. [Tools Layer](#12-tools-layer) — 운영 CLI 도구
13. [Testing Layer](#13-testing-layer) — 테스트킷, 통합 테스트
14. [핫패스 설계 원칙](#14-핫패스-설계-원칙)
15. [의존성 그래프](#15-의존성-그래프)
16. [아키텍처 문서 목록](#16-아키텍처-문서-목록)
17. [주의사항 & 함정](#17-주의사항--함정)

---

## 1. 전체 아키텍처

```
┌──────────────────────────── Data Flow ────────────────────────────┐
│                                                                    │
│  Exchange WS ──► publisher ──► SHM SharedRegion ──► strategy      │
│  (Gate/Binance)   (ZMQ PUB)    (seqlock slots)     (v6/v7/v8)    │
│                                     │                   │          │
│                                     ▼                   ▼          │
│                               subscriber          OrderRequest     │
│                             (ZMQ SUB → ch)     (SHM ring / ZMQ)   │
│                                     │                   │          │
│                                     ▼                   ▼          │
│                            futures-collector     order-gateway     │
│                            (Parquet Hive)       (REST execute)     │
│                                                      │             │
│                                                      ▼             │
│                                                 Exchange REST      │
│                                              (place/cancel order)  │
│                                                      │             │
│                                                      ▼             │
│                                               OrderResult (ZMQ)   │
│                                                 → strategy         │
└────────────────────────────────────────────────────────────────────┘
```

**레이턴시 예산**: exchange WS 수신 → 전략 주문 제출까지 50ms 이내 (7단계 파이프라인).

---

## 2. Workspace 구조

```
mono/
├── Cargo.toml                    ← workspace root
├── DEPLOYMENT_GUIDE.md           ← 배포 가이드
├── CODEBASE_WIKI.md              ← 이 문서
│
├── crates/
│   ├── core/                     ← 도메인 타입 + 프로토콜 + 설정 + 시간
│   │   ├── hft-types/            ←  Symbol, BookTicker, Trade, ExchangeId
│   │   ├── hft-protocol/         ←  Wire encoding (120B/128B), ZMQ topics, OrderWire
│   │   ├── hft-config/           ←  Figment layered config (defaults → TOML → env)
│   │   └── hft-time/             ←  Clock trait, LatencyStamps 7-stage pipeline
│   │
│   ├── obs/                      ← 관측성
│   │   └── hft-telemetry/        ←  tracing, OTLP, HDR histogram, Prometheus export
│   │
│   ├── infra/                    ← 인프라 공통
│   │   ├── hft-common/           ←  health server, supervisor (run_with_restart)
│   │   └── hft-state/            ←  Redis state backend (graceful degrade)
│   │
│   ├── account/                  ← 계정 관리
│   │   └── hft-account/          ←  AccountManager, MarginManager, expand_logins
│   │
│   ├── transport/                ← 전송 계층
│   │   ├── hft-zmq/              ←  ZMQ PUSH/PULL/PUB/SUB wrapper
│   │   ├── hft-shm/              ←  SharedRegion, seqlock, SPMC/SPSC rings
│   │   ├── hft-ipc/              ←  EventSink/EventSource trait facade
│   │   ├── hft-order-egress/     ←  Strategy → Gateway 주문 전송 (SHM/ZMQ)
│   │   ├── hft-strategy-shm/     ←  Strategy VM v2 SHM 클라이언트
│   │   └── hft-strategy-shm-py/  ←  Python ctypes 바인딩 (Phase 2 pending)
│   │
│   ├── storage/                  ← 저장소
│   │   └── hft-storage/          ←  QuestDB ILP writer (TCP 9009)
│   │
│   ├── exchange/                 ← 거래소 연동
│   │   ├── hft-exchange-api/     ←  ExchangeFeed, ExchangeExecutor trait
│   │   ├── hft-exchange-common/  ←  SymbolCache, JsonScratch, lenient deserializers
│   │   ├── hft-exchange-rest/    ←  Credentials, RestClient, HMAC signing
│   │   ├── hft-exchange-gate/    ←  Gate.io Futures (WS + REST + UserStream)
│   │   ├── hft-exchange-binance/ ←  Binance Futures (WS + REST)
│   │   ├── hft-exchange-bybit/   ←  Bybit (WS + REST)
│   │   ├── hft-exchange-bitget/  ←  Bitget (WS + REST)
│   │   └── hft-exchange-okx/     ←  OKX (WS + REST)
│   │
│   ├── strategy/                 ← 전략 알고리즘
│   │   ├── hft-strategy-config/  ←  TradeSettings, StrategyConfig, Supabase loader
│   │   ├── hft-strategy-core/    ←  signal, decision, risk (handle_chance)
│   │   └── hft-strategy-runtime/ ←  PositionOracleImpl, caches (ArcSwap/DashMap)
│   │
│   ├── services/                 ← 실행 바이너리
│   │   ├── publisher/            ←  Multi-exchange WS → ZMQ aggregator
│   │   ├── subscriber/           ←  ZMQ SUB → crossbeam channel
│   │   ├── strategy/             ←  Trading engine (v6/v7/v8 variants)
│   │   ├── order-gateway/        ←  Multi-exchange order router
│   │   └── monitoring-agent/     ←  Prometheus scraper + Telegram alert
│   │
│   ├── tools/                    ← 운영 CLI
│   │   ├── latency-probe/        ←  ZMQ roundtrip 레이턴시 측정
│   │   ├── questdb-export/       ←  LatencyStamps → QuestDB 기록
│   │   ├── futures-collector/    ←  Parquet Hive 아카이버
│   │   ├── healthcheck/          ←  서비스 상태 점검 + Gate 테스트 주문
│   │   ├── status-check/         ←  파이프라인 통합 대시보드
│   │   └── close-positions/      ←  긴급 전량 청산
│   │
│   └── testing/                  ← 테스트
│       ├── hft-testkit/          ←  MockFeed, MockClock, fixtures
│       └── integration/          ←  E2E 통합 테스트
│
├── deploy/                       ← 배포 설정
│   ├── monitoring/               ←  docker-compose (Prometheus + Grafana)
│   ├── systemd/                  ←  서비스 유닛 파일
│   ├── scripts/                  ←  호스트 사전 점검, VM 런치
│   └── tmux/                     ←  개발 환경 레이아웃
│
└── docs/                         ← 아키텍처 문서
    ├── architecture/             ←  SHM_DESIGN.md, MULTI_VM_TOPOLOGY.md
    ├── adr/                      ←  ADR-0001 ~ 0005
    ├── phase1/                   ←  hot-path-rules, latency-budget, jitter-playbook
    ├── phase2/                   ←  CHANGES_v2 phase docs
    └── runbooks/                 ←  INFRA_REQUIREMENTS.md
```

---

## 3. Core Layer

### 3.1 hft-types — 도메인 타입

**경로**: `crates/core/hft-types/`

모든 크레이트가 의존하는 최하위 타입 정의. 외부 의존성 최소화 (serde, thiserror 만).

| 타입 | 설명 | 핫패스 주의점 |
|------|------|--------------|
| `Symbol` | `Arc<str>` 래퍼. `ptr_eq()` 로 identity 비교 가능 | intern 후 clone 비용 = Arc refcount bump 만 |
| `ExchangeId` | enum (Gate, Binance, Bybit, Bitget, Okx). `const ALL` 배열 제공 | Copy trait, 패턴매칭 분기 |
| `BookTicker` | `#[repr(C)]` — exchange, symbol, bid/ask price/size, timestamps | 120B wire 호환. `mid()`, `spread_bp()` 헬퍼 |
| `Trade` | exchange, symbol, price, signed size, trade_id, timestamps | 128B wire 호환. size 부호 = aggressor 방향 |
| `MarketEvent` | enum — BookTicker / Trade / WebBookTicker | strategy 입력 이벤트 |
| `Price(f64)` | 투명 newtype, `is_valid()` 검증 | |
| `Size(f64)` | 투명 newtype, `abs()` 제공 | |
| `Side` | Buy / Sell | |
| `DataRole` | Primary / WebOrderBook | feed 역할 구분 |

**상수**: `consts::EPS = 1e-12` — 0 나눗셈 방어용 epsilon.

> **주의**: `BookTicker` 와 `Trade` 의 `#[repr(C)]` 레이아웃은 wire encoding 과 byte-exact 대응이 필요하다. 필드 순서나 타입을 변경하면 `hft-protocol` wire 인코딩이 깨진다.

---

### 3.2 hft-protocol — Wire 인코딩 + ZMQ 토픽

**경로**: `crates/core/hft-protocol/`

레거시 Python 시스템과 **byte-exact wire parity** 를 유지하는 인코딩/디코딩 계층.

**서브모듈**:

| 모듈 | 역할 | 주요 상수 |
|------|------|----------|
| `wire` | BookTicker(120B), Trade(128B) C-struct 레이아웃 | `BOOK_TICKER_SIZE=120`, `TRADE_SIZE=128` |
| `frame` | 메시지 프레이밍: `[type_len:u8][type:str][payload]` | |
| `topics` | ZMQ topic: `{exchange}_{type}_{symbol}` | `MSG_BOOKTICKER`, `MSG_TRADE` |
| `order_wire` | 128B 주문 wire (Phase 2 E) | `ORDER_REQUEST_WIRE_SIZE=128`, `ORDER_RESULT_WIRE_SIZE=128` |
| `order_adapter` | OrderRequest → wire 변환, 가격 양자화 | `PRICE_SCALE=1e8` (정수 스케일링) |

**Wire 레이아웃 (BookTickerWire 120B)**:
```
Offset  0: exchange     (16B, zero-padded ASCII)
Offset 16: symbol       (32B, zero-padded ASCII)
Offset 48: bid_price    (f64, 8B)
Offset 56: bid_size     (f64, 8B)
Offset 64: ask_price    (f64, 8B)
Offset 72: ask_size     (f64, 8B)
Offset 80: event_time   (i64, 8B ms)
Offset 88: server_time  (i64, 8B ms)
Offset 96: pushed_ms    (i64, 8B — publisher 가 patch)
...
```

**OrderRequestWire 128B 오프셋**:
```
Offset  0: exchange (u16)   Offset  2: side (u8)      Offset  3: order_type (u8)
Offset  4: symbol_id (u32)  Offset  8: tif (u8)       Offset  9: level (u8)
Offset 10: flags (u8)       Offset 16: price (i64)     Offset 24: size (i64)
Offset 32: client_seq (u64) Offset 40: origin_ts_ns    Offset 48: text_tag (48B)
```

**WireLevel**: `Open(0)` / `Close(1)` — SHM/ZMQ 주문 방향 구분.

**가격 양자화**: `round_to_tick(price, tick_size)` — 거래소별 tick size 에 맞춰 가격을 반올림. `QuantizeHint` 로 정밀도 제어.

> **주의**: wire 인코딩은 레거시 Python serializer 와 byte-exact 호환이 필수다. 필드 오프셋이나 타입을 변경하면 publisher ↔ subscriber 간 데이터 파싱이 깨진다. 변경 시 반드시 `wire::tests` 의 golden-byte 테스트를 통과해야 한다.

---

### 3.3 hft-config — 계층형 설정 로더

**경로**: `crates/core/hft-config/`

Figment 기반 3-layer 설정: **defaults → TOML → 환경변수** (후자가 우선).

**핵심 타입**:

| 타입 | 용도 |
|------|------|
| `AppConfig` | 최상위 설정. `Arc<AppConfig>` 로 multi-thread 공유 |
| `ExchangeConfig` | 거래소별 심볼/튜닝 |
| `ZmqConfig` | endpoint, HWM, drain_batch_cap |
| `TelemetryConfig` | OTLP, log level, JSON 출력 |
| `ShmConfig` | backing type, layout spec |
| `OrderEgressConfig` | SHM/ZMQ 모드, backpressure policy |

**환경변수 prefix**: `HFT_`, nested separator: `__`.
예: `HFT_SHM__BACKING=pci_bar`, `HFT_TELEMETRY__PROM_PORT=9100`.

**`load_all()`**: panic 없이 `Result<Arc<AppConfig>>` 반환. TOML 미존재 시 defaults + env 만으로 구성.

> **주의**: `AppConfig` 는 `Arc` 로 감싸서 공유한다. `RwLock` 을 쓰지 않는 이유는 lock contention 에 의한 latency jitter 방지.

---

### 3.4 hft-time — Clock 추상화 + 레이턴시 측정

**경로**: `crates/core/hft-time/`

| 타입 | 용도 |
|------|------|
| `Clock` trait | `now_ms()` (wall), `now_nanos()` (monotonic), `epoch_ns()` (wall ns) |
| `SystemClock` | 프로덕션 구현. `Instant` baseline |
| `MockClock` | 테스트 구현. `AtomicI64`/`AtomicU64`, `advance_ms()` |
| `LatencyStamps` | 7-stage 파이프라인 타이밍 (wall + monotonic 쌍) |
| `Stage` | enum — ExchangeServer, WsReceived, Serialized, Pushed, Published, Subscribed, Consumed, EndToEnd |

**LatencyStamps 7단계**:
```
Exchange WS server_time → WS received → Serialized (wire) → Pushed (ZMQ)
→ Published (PUB) → Subscribed (SUB) → Consumed (strategy eval)
```

각 단계에서 wall-clock ms + monotonic nanos 를 기록한다. Wall clock 은 NTP 점프에 취약하므로, per-stage delta 는 monotonic 을 사용.

> **주의**: `Stage::EndToEnd` 는 Phase 3 A-2 에서 추가된 variant. e2e 레이턴시를 별도 히스토그램 버킷에 기록한다.

---

## 4. Observability Layer

### 4.1 hft-telemetry — 트레이싱 + 메트릭 + 히스토그램

**경로**: `crates/obs/hft-telemetry/`

| 기능 | 구현 |
|------|------|
| Structured logging | `tracing` + `tracing-subscriber` (EnvFilter) |
| OTLP export | `opentelemetry-otlp` (optional) |
| HDR histogram | `hdrhistogram` — stage별 p50/p95/p99/max |
| Atomic counters | `CounterKey` enum → `AtomicU64` array |
| Atomic gauges | `GaugeKey` enum → `AtomicI64` array |
| Prometheus export | text exposition format (`/metrics` endpoint) |
| CPU affinity | `pin_current_thread(core_id)`, `next_hot_core()` |

**핫패스 API**:
- `counter_inc(key)` — atomic fetch_add, lock-free
- `gauge_set(key, value)` — atomic store
- `record_stage_nanos(stage, nanos)` — HDR histogram recording

**feature flag**: `verbose-trace` — release 빌드에서 `trace!` 매크로 자동 제거.

> **주의**: HDR histogram 은 기본 5s dump 주기로 통계 출력. 히스토그램 자체는 per-stage 별도 인스턴스로, mutex contention 을 최소화한다.

---

## 5. Infrastructure Layer

### 5.1 hft-common — Health Server + Supervisor

**경로**: `crates/infra/hft-common/`

| 모듈 | 역할 |
|------|------|
| `health_server` | HTTP `/health` + `/metrics` endpoint (hyper/axum) |
| `supervisor` | `run_with_restart()` — exponential backoff 재시작 (RestartConfig) |
| `telegram` | `TelegramNotifier` — Bot API HTML 메시지 전송 |

**`run_with_restart(name, config, cancel, factory)`**: 서비스 팩토리 함수를 받아 실패 시 자동 재시작. `RestartConfig` 로 max_retries, backoff 제어. `SupervisorExit::Clean` 또는 `MaxRetriesExceeded` 반환.

---

### 5.2 hft-state — Redis 상태 백엔드

**경로**: `crates/infra/hft-state/`

**설계 원칙**: Graceful degrade — Redis 연결 실패 시 모든 API 가 no-op (warn 로그만). 전략은 정상 동작.

| 타입 | 역할 |
|------|------|
| `RedisState` | Redis 연결 래퍼. `from_env()` 로 초기화 |
| `StrategySession` | variant, login, pid, hostname, heartbeat_ms |

**주요 메서드**:
- `save_last_orders(store)` — DashMap 전체를 per-symbol JSON 으로 Redis 에 저장 (TTL 3600s)
- `restore_last_orders(store)` — Redis → DashMap 복원 (strategy 재시작 시)
- `save_session(session)` — 전략 세션 메타 기록 (TTL 86400s)
- `heartbeat(now_ms)` — last_heartbeat_ms 갱신

**Key 패턴**: `{REDIS_STATE_PREFIX}:{login_name}:last_order:{symbol}`

> **주의**: Redis 가 없어도 전략은 정상 동작한다. 다만 재시작 시 last_order 복원이 안 되므로 position tracking 이 초기화된다.

---

## 6. Account Layer

### 6.1 hft-account — 멀티 서브계정 관리

**경로**: `crates/account/hft-account/`

Phase 3 D-2 에서 추가. 레거시 Python `account_manager.py`, `margin_manager.py`, `subaccount_utils.py` 대체.

| 모듈 | 역할 |
|------|------|
| `accounts` | `AccountEntry`, `AccountsConfig` JSON 스키마, `load_accounts_from_env()` |
| `manager` | `AccountManager` — login_name → `AccountHandle` 레지스트리 |
| `margin` | `MarginManager` — 전 계정 잔고 병렬 수집 (읽기 전용) |
| `login_expand` | `expand_logins("sigma001~sigma010")` → Vec 확장 |
| `ip_detect` | `detect_local_ip()` — UDP connect trick egress IP |
| `keys` | key naming convention 상수 |

**AccountHandle** 번들:
```
login_name: String
client: GateAccountClient      ← REST polling 용
credentials: Credentials       ← WS user stream 용
symbols: Vec<String>           ← 이 계정이 담당하는 심볼
leverage: Option<f64>          ← 계정별 레버리지 override
```

**Fallback 우선순위**:
1. `HFT_ACCOUNTS_FILE` JSON → 멀티계정
2. `GATE_API_KEY`/`SECRET` → 단일계정
3. 둘 다 없음 → 빈 레지스트리 (warn)

> **주의**: 현재 strategy main.rs 는 `AccountManager` 를 사용하지 않고 직접 `GATE_API_KEY` 를 읽는다. 멀티계정 wiring 은 후속 개발 필요 (DEPLOYMENT_GUIDE.md §8 참조).

---

## 7. Transport Layer

### 7.1 hft-zmq — ZMQ 소켓 래퍼

**경로**: `crates/transport/hft-zmq/`

| 타입 | 역할 |
|------|------|
| `Context` | 프로세스 전역 ZMQ context (Arc 내부, clone 저렴) |
| `PushSocket` / `PullSocket` | 1:1 메시징 |
| `PubSocket` / `SubSocket` | 1:N 브로드캐스트 |
| `SendOutcome` | Sent / WouldBlock / Error |

**소켓 튜닝** (중앙 관리):
- HWM, LINGER, TCP_KEEPALIVE, IMMEDIATE 등 `ZmqConfig` 에서 일괄 설정
- 모든 sender 는 non-blocking (`send_dontwait`)
- WouldBlock = 호출자가 drop/retry 결정

> **주의**: `drain_batch(cap)` 으로 배치 수신 시, Vec 재사용으로 핫패스 alloc 을 피한다.

---

### 7.2 hft-shm — 공유 메모리 (sub-μs 레이턴시)

**경로**: `crates/transport/hft-shm/`

이 크레이트가 시스템의 **핵심 성능 경로**. publisher → strategy 간 50-300ns p50 레이턴시를 달성한다.

**핵심 구성요소**:

| 타입 | 패턴 | 용도 |
|------|------|------|
| `SharedRegion` | 단일 파일/PCI BAR 매핑 | 전체 SHM 영역 관리 |
| `QuoteSlot` | Seqlock (per-symbol, 256B aligned) | 최신 best bid/ask |
| `TradeRing` | SPMC broadcast ring | trade 히스토리 |
| `OrderRing` | SPSC Lamport queue | strategy → gateway 주문 |
| `SymbolTable` | (ExchangeId, symbol) ↔ idx 매핑 | 심볼 인터닝 |

**Region 구조**:
```
SharedRegion (단일 mmap)
├── Header (magic, version, layout_digest)
├── QuoteSlot[0..QUOTE_SLOTS]           ← seqlock, 256B aligned
├── TradeRing (SPMC, capacity power-of-2)
├── SymbolTable (open addressing hash)
└── OrderRing[0..N_MAX] (SPSC per VM)   ← vm_id 로 격리
```

**Backing enum**:
- `DevShm { path }` — `/dev/shm`, 개발/단일호스트
- `Hugetlbfs { path }` — 2MB huge pages, TLB hit rate 향상
- `PciBar { path }` — ivshmem PCI BAR2, multi-VM 프로덕션

**Role enum**:
- `Publisher` — region 생성 + 초기화 (master)
- `Strategy { vm_id }` — region attach (slave, vm_id 로 OrderRing 격리)
- `OrderGateway` — `MultiOrderRingReader` 로 N개 SPSC ring 팬인

**Seqlock 3-phase protocol** (QuoteSlot):
```
1. Writer: seq_store(odd)     ← in-progress marker
2. Writer: payload write      ← 실제 데이터
3. Writer: seq_store(even)    ← commit

Reader: loop { seq1 = load(); read payload; seq2 = load(); if seq1 == seq2 && even → valid }
```

> **주의 (치명적)**:
> - SHM 레이아웃 파라미터 (`N_MAX`, `QUOTE_SLOTS`, `TRADE_RING_CAPACITY`, `ORDER_RING_CAPACITY`) 는 publisher 와 strategy 사이에 **한 바이트라도 다르면 torn read / 데이터 오염** 발생.
> - `layout_digest(spec)` 으로 양쪽 일치 여부를 검증한다.
> - `OrderRing` 은 SPSC 이므로 하나의 vm_id 에 strategy 프로세스 2개를 붙이면 안 된다.
> - Seqlock 3-phase 를 2-phase 로 단축하면 capacity-1 gap 에서 torn read 가 재발한다 (검증됨).

---

### 7.3 hft-ipc — Transport 추상화 Facade

**경로**: `crates/transport/hft-ipc/`

| trait | 메서드 | 용도 |
|-------|--------|------|
| `EventSink` | `try_send(topic, payload)` | non-blocking 이벤트 전송 |
| `EventSource` | `recv_timeout()`, `drain_into()` | 이벤트 수신 (배치 가능) |

ZMQ 위에 trait 을 씌워 테스트 시 mock 교체 가능하게 한다. `drain_into(cap, &mut vec)` 는 caller Vec 재사용으로 핫패스 alloc 제거.

---

### 7.4 hft-order-egress — 주문 전송 (SHM / ZMQ)

**경로**: `crates/transport/hft-order-egress/`

| 타입 | 역할 |
|------|------|
| `OrderEgress` trait | `try_submit(req, meta) → Result<SubmitOutcome>` |
| `ShmOrderEgress` | SHM OrderRing 에 128B wire 기록 |
| `ZmqOrderEgress` | ZMQ PUSH 소켓으로 128B wire 전송 |
| `PolicyOrderEgress<T>` | backpressure 래퍼 (retry/drop/block) |

**SubmitOutcome**: `Sent` / `WouldBlock` (WouldBlock 은 에러가 아님 — backpressure 정상 동작).

---

### 7.5 hft-strategy-shm — Strategy VM SHM 클라이언트

**경로**: `crates/transport/hft-strategy-shm/`

`StrategyShmClient::attach(backing, spec, vm_id)` 한 번 호출로 SharedRegion 의 5개 하위 영역 (quote, trade, symtab, order ring, heartbeat) 을 한꺼번에 오픈.

---

## 8. Storage Layer

### 8.1 hft-storage — QuestDB ILP Writer

**경로**: `crates/storage/hft-storage/`

QuestDB InfluxDB Line Protocol (ILP) 를 raw TCP (port 9009) 로 전송.

**테이블**:
- `bookticker` — symbol, exchange, bid/ask prices/sizes, timestamps
- `trade` — symbol, exchange, price, signed size, trade_id, timestamps
- `latency_stages` — stage name, nanos delta

**배치 전략**: `batch_rows` 또는 `batch_ms` 임계 중 먼저 도달 시 flush. 연결 실패 시 `spool_dir/*.ilp` 에 기록 후 재접속 시 replay.

> **주의**: 핫패스에서 사용하면 안 된다. subscriber/questdb-export 서비스에서만 호출.

---

## 9. Exchange Layer

### 9.1 hft-exchange-api — 추상 계약 (traits)

**경로**: `crates/exchange/hft-exchange-api/`

| trait | 역할 |
|-------|------|
| `ExchangeFeed` | `stream(symbols, emit, cancel)` — WS 시세 수신. 내부 재접속 책임 |
| `ExchangeExecutor` | `place_order(req)`, `cancel(id)` — REST 주문 실행 |

**Emitter**: `Arc<dyn Fn(MarketEvent, LatencyStamps) + Send + Sync>` — allocation-free callback. 채널 대신 직접 함수 호출로 레이턴시 최소화.

**OrderRequest**: symbol, side, qty, price, reduce_only, tif, client_id, client_seq, origin_ts_ns.

**ApiError**: Auth, RateLimited, Rejected, InvalidOrder, Connection, Decode, Cancelled.

**Backoff**: `new(base_ms, cap_ms)` → `next_ms()` 는 base × 2^attempt (cap 초과 방지). `reset()` 성공 시.

> **주의**: `ExchangeFeed::stream()` 은 long-lived Future 다. 내부적으로 WS 재접속을 포함하며, 호출자가 retry 할 필요 없다.

---

### 9.2 hft-exchange-common — 핫패스 공통 유틸

**경로**: `crates/exchange/hft-exchange-common/`

| 타입 | 역할 | 핫패스 특성 |
|------|------|------------|
| `SymbolCache` | `DashMap<String, Symbol>` intern pool | 99% read → Arc::clone 만 |
| `JsonScratch` | 재사용 `Vec<u8>` 버퍼 | clear + extend, 첫 grow 이후 alloc 없음 |
| `deserialize_f64_lenient` | string / number 둘 다 수용 | Gate 등의 inconsistent JSON 대응 |

---

### 9.3 hft-exchange-rest — REST 공통 인프라

**경로**: `crates/exchange/hft-exchange-rest/`

| 타입 | 역할 |
|------|------|
| `Credentials` | api_key + api_secret + passphrase (Arc<str>). Debug 시 masking |
| `RestClient` | reqwest 래퍼. 5xx 3회 retry, 429 Retry-After 존중 |

**서명 함수**: `sign_hmac_sha256`, `sign_hmac_sha512`, `sign_base64` — 5개 거래소 서명 방식 전부 커버.

> **주의**: `Credentials` 의 `Debug` impl 은 secret 을 masking 한다. 로그에 평문 키가 노출되지 않도록 설계됨.

---

### 9.4 hft-exchange-gate — Gate.io Futures

**경로**: `crates/exchange/hft-exchange-gate/`

가장 큰 거래소 크레이트. 4개 서브모듈:

| 모듈 | 역할 |
|------|------|
| `lib.rs` (GateFeed) | `futures.book_ticker` + `futures.trades` WS 구독 |
| `account.rs` | `GateAccountClient` (REST polling) + `AccountPoller` (bg task) |
| `executor.rs` | `GateExecutor` — REST v4 주문 place/cancel |
| `user_stream.rs` | `GateUserStream` — private WS (orders, positions, balances) |

**AccountPoller 아키텍처**:
```
AccountPoller::builder(client)
    .meta(Arc<SymbolMetaCache>)         ← 60s 주기 갱신
    .positions(Arc<PositionCache>)      ← 1s 주기 갱신
    .balance_slot(Arc<BalanceSlot>)     ← 2s 주기 갱신
    .warm_start(true)                   ← 시작 시 즉시 1회 fetch
    .spawn()                            → PollerHandle
```

**GateUserStream** — HMAC-SHA512 인증 WS:
- 채널: `futures.orders`, `futures.usertrades`, `futures.positions`, `futures.balances`
- `UserStreamEvent` enum → `UserStreamCallback` (Arc closure)
- 포지션/잔고 업데이트는 `PositionCache`/`BalanceSlot` 에 직접 반영

**핫패스 최적화** (Phase 2):
- Borrow 기반 역직렬화: `GateFrame<'a>` + `#[serde(borrow)]` → DOM 없이 직접 파싱
- `SymbolCache::intern()` → 심볼당 1회 alloc, 이후 Arc::clone
- `JsonScratch` 버퍼 재사용
- 벤치마크: Phase 1 (2.1μs/bookTicker) → Phase 2 (0.35μs, ~6× 향상)

> **주의**: Gate WS 는 필드 타입이 일관되지 않다 (string/number 혼용). `deserialize_f64_lenient` 으로 방어한다.

---

### 9.5 기타 거래소 (Binance, Bybit, Bitget, OKX)

동일 패턴: `ExchangeFeed` impl + `ExchangeExecutor` impl. Gate 와 구조 동일하되 WS 프레임 스키마와 서명 방식이 다름.

**Binance 특이사항**: 심볼 매핑 `BTCUSDT` (wire) → `BTC_USDT` (canonical). trade sign convention: `m=true` (buyer-is-maker) → size 음수.

---

## 10. Strategy Layer

### 10.1 hft-strategy-config — 매매 설정

**경로**: `crates/strategy/hft-strategy-config/`

| 타입 | 역할 |
|------|------|
| `TradeSettings` | 100+ 매매 파라미터. 레거시 Python 기본값 그대로 보존 |
| `StrategyConfig` | login_name + symbols + trade_settings |

**TradeSettings 주요 파라미터 그룹**:

| 그룹 | 예시 | 기본값 |
|------|------|--------|
| 주문 주기 | `update_interval_seconds` | 0.1s |
| 사이징 | `order_size`, `max_position_size` | 환경별 설정 |
| 진입/퇴출 | `spread_bp_threshold`, `close_raw_mid_profit_bp` | |
| 시간 제한 | `same_side_price_ms` (400-600ms range) | 랜덤화 |
| 수익 필터 | `profit_bp_ema_alpha` (0.1), `threshold` (1.20) | |
| 리스크 | `funding_rate_threshold` (5bp/8h) | 0.0005 |

> **주의**: `ArcSwap<Option<StrategyConfig>>` 으로 캐시. Supabase 장애 시 마지막 유효 설정 유지.

---

### 10.2 hft-strategy-core — 순수 알고리즘

**경로**: `crates/strategy/hft-strategy-core/`

외부 의존성 없는 **순수 함수** 계층. 모든 상태는 인자로 전달.

| 모듈 | 함수 | 역할 |
|------|------|------|
| `signal` | `calculate_signal_v6()`, `_v8()` | BookTicker + Trade → SignalResult |
| `decision` | `decide_order_v6()`, `_v7()`, `_v8()` | Signal → OrderDecision |
| `risk` | `handle_chance()` | OrderDecision → RiskCheck (7-point gating) |

**handle_chance 7-point 검증**:
```
1. account_symbol 여부 (AccountMembership)
2. last_order 시간 제한 (same_side_price_ms)
3. 포지션 방향 vs 주문 방향 일치 검증
4. time_restriction 랜덤화 범위 검증
5. 계정 잔고 충분성
6. net_exposure ratio (|net_size| × price / leverage < ratio × balance)
7. max_position_size 초과 여부
```

**PositionOracle trait**:
```rust
trait PositionOracle {
    fn snapshot(&self, symbol: &str) -> ExposureSnapshot;
    fn record_last_order(&self, symbol: &str, order: LastOrder);
}
```

`ExposureSnapshot`: total_long_usdt, total_short_usdt, this_symbol_usdt, is_account_symbol, risk_limit, min_order_size, last_order, quanto_multiplier.

> **주의**: `handle_chance` 는 **reject-by-default** 정책이다. 7개 조건 중 하나라도 실패하면 None 반환. 내부에 `rand` 호출 없음 — jitter 는 호출자가 pre-sample 해서 전달.

---

### 10.3 hft-strategy-runtime — 런타임 캐시 + Oracle 구현

**경로**: `crates/strategy/hft-strategy-runtime/`

| 모듈 | 타입 | 저장소 | 핫패스 특성 |
|------|------|--------|------------|
| `symbol_meta` | `SymbolMetaCache` | `ArcSwap<HashMap>` | load = Arc::clone, lock-free |
| `positions` | `PositionCache` | `ArcSwap<PositionSnapshot>` | load = Arc::clone |
| `last_orders` | `LastOrderStore` | `DashMap<Symbol, LastOrder>` | read = shard lock (수 ns) |
| `order_rate` | `OrderRateTracker` | `DashMap<Symbol, VecDeque>` | decay + record |
| `oracle` | `PositionOracleImpl` | 위 4개 조합 | trait impl |

**PositionOracleImpl** — `PositionOracle` trait 의 실제 구현:
```rust
struct PositionOracleImpl {
    meta: Arc<SymbolMetaCache>,
    positions: Arc<PositionCache>,
    last_orders: Arc<LastOrderStore>,
    membership: Arc<AccountMembership>,
}
```

**AccountMembership** — closure 기반 심볼 소속 판정:
```rust
AccountMembership::fixed(["BTC_USDT", "ETH_USDT"])  // HashSet-backed
AccountMembership::none()                             // 항상 false (safety default)
```

> **주의**: `ArcSwap::load()` 는 핫패스에서 안전하다 (atomic load + Arc clone). 하지만 `store()` 는 background poller 에서만 호출해야 한다.

---

## 11. Services Layer

### 11.1 publisher — 시세 수집기

**경로**: `crates/services/publisher/`

**동작**: 거래소별 WS feed → ZMQ PUB 로 브로드캐스트 + SHM QuoteSlot/TradeRing 기록.

- Multi-threaded worker pool (거래소별 + 심볼 샤딩)
- 5s 주기 latency stats dump (p50/p95/p99/max)
- `READINESS_PORT` 로 warm-up 완료 시그널
- CPU affinity: cores 4-7 (gateway 와 분리)

---

### 11.2 subscriber — ZMQ 수신기

**경로**: `crates/services/subscriber/`

ZMQ SUB → `InprocQueue` (crossbeam bounded channel, 8192). strategy 와 같은 프로세스 내에서 embed 되어 동작.

---

### 11.3 strategy — 트레이딩 엔진

**경로**: `crates/services/strategy/`

가장 복잡한 서비스. 전체 스택을 `bring_up_full()` 에서 조립.

**Strategy Variants**:

| Variant | 용도 | Signal | Decision |
|---------|------|--------|----------|
| `noop` | 스모크 테스트 (주문 없음) | — | — |
| `v6` | 레거시 v6 포트 | `calculate_signal_v6` | `decide_order_v6` |
| `v7` | 레거시 v7 포트 | `calculate_signal` | `decide_order_v7` |
| `v8` | Phase 2 A (EMA profit + rate tracking) | `calculate_signal_v8` | `decide_order_v8` |
| `close_v1` | 기본 포지션 청산 | — | LimitClose only |
| `mm_close` | MM 멀티심볼 청산 | — | 병렬 long/short 축소 |
| `close_unhealthy` | stale 포지션 선택 청산 | — | stale quote/position 기준 |

**bring_up_full() 조립 순서**:
```
1. StrategyConfig (login_name, symbols, trade_settings)
2. 캐시 생성 (SymbolMetaCache, PositionCache, LastOrderStore, BalanceSlot)
3. AccountMembership::fixed(symbols)
4. PositionOracleImpl::new(meta, positions, last_orders, membership)
5. RedisState restore (last_orders 복원)
6. AccountPoller spawn (optional, creds 필요)
7. GateUserStream spawn (optional)
8. subscriber start (ZMQ SUB → InprocQueue)
9. Strategy variant 인스턴스 생성 + start()
10. Balance pump task (500ms)
11. Rate decay task (1s)
12. Order drain (SHM/ZMQ)
13. Result listener (ZMQ PULL)
14. Redis heartbeat (30s)
```

**StrategyControl** enum — strategy 런타임에 제어 메시지 전달:
- `SetAccountBalance { total_usdt, unrealized_pnl_usdt }`
- `WsPositionUpdate { contract, size, entry_price, ... }`
- `WsBalanceUpdate { balance, change }`
- `WsOrderUpdate { order_id, contract, status, ... }`
- `OrderResult(OrderResultInfo)`

**StrategyHandle**: strategy thread join + control channel + shutdown token.

> **주의**: `AccountMembership::none()` 이 기본 safety default 다. `is_account_symbol = false` 이면 `handle_chance` 가 모든 주문을 거부한다. 반드시 `fixed(symbols)` 로 초기화해야 실주문이 나간다.

---

### 11.4 order-gateway — 주문 라우터

**경로**: `crates/services/order-gateway/`

| 컴포넌트 | 역할 |
|----------|------|
| `RoutingTable` | ExchangeId → ExchangeExecutor 매핑 |
| `MultiOrderRingReader` | N개 SPSC ring 팬인 (SHM ingress) |
| `ZmqIngress` | ZMQ PULL 수신 (fallback) |
| `DedupCache` | client_id → result (1000 entry, LRU) |
| `RetryPolicy` | max_retries, backoff |
| `NoopExecutor` | creds 없는 거래소용 dummy |

**주문 흐름**:
```
SHM OrderRing / ZMQ PULL
    → decode OrderRequestWire (128B)
    → RoutingTable lookup (ExchangeId)
    → DedupCache check (client_id)
    → ExchangeExecutor::place_order()
    → OrderResultWire encode (128B)
    → ZMQ PUSH → strategy
```

> **주의**: DedupCache 는 in-memory 이므로 gateway 재시작 시 초기화된다. 중복 주문 방지를 위해 client_seq 의 monotonic 증가를 보장해야 한다.

---

### 11.5 monitoring-agent — 메트릭 알림

**경로**: `crates/services/monitoring-agent/`

Prometheus 와 **독립적으로** 동작하는 경량 알림 서비스. 5s 주기로 `/metrics` 직접 스크래핑.

14개 알림 규칙 (threshold / rate / counter):
```
heartbeat_stale        — gateway heartbeat age > 10s
service_down           — /health 응답 없음
no_strategies          — active_strategies gauge = 0
e2e_latency_high       — e2e p99 > 50ms
supervisor_restart     — restart_count counter 증가
shm_ring_full          — ring utilization > 90%
order_rejected_rate    — rejected / (accepted + rejected) > 20%
...
```

`AlertState` — per-rule silence window + 이전 counter delta 추적으로 중복 알림 방지.

---

## 12. Tools Layer

| 도구 | 용도 | 주요 기능 |
|------|------|----------|
| `latency-probe` | ZMQ roundtrip 레이턴시 측정 | PUB → SUB → 측정 루프 |
| `questdb-export` | LatencyStamps → QuestDB | 6개 adjacent stage delta + e2e, PG wire DDL init |
| `futures-collector` | Parquet Hive 아카이버 | BookTickerRow(9col) + TradeRow(10col), ZSTD lv3, 100K row group |
| `healthcheck` | 서비스 상태 점검 | /health probe + Gate test order (place+cancel) |
| `status-check` | 파이프라인 통합 대시보드 | 계정잔고, 포지션, 레이턴시, 알림 상태 (`--json` 지원) |
| `close-positions` | 긴급 전량 청산 | dry-run, chunked, confirm prompt (`--yes` 비대화형) |

모든 도구: clap derive CLI, mimalloc, ctrlc signal handler, ExitCode.

> **주의**: `close-positions --yes` 는 확인 없이 즉시 시장가 전량 청산한다. 운영 사고 시에만 사용.

---

## 13. Testing Layer

### 13.1 hft-testkit — 테스트 유틸리티

**경로**: `crates/testing/hft-testkit/`

| 타입 | 용도 |
|------|------|
| `MockFeed` | `ScheduledEvent` 스크립트 재생 |
| `MockClock` | 결정적 시간 제어 (advance_ms) |
| `MockSink` | 이벤트 녹화 (Mutex<Vec>) |
| `Fixtures` | `bookticker()`, `trade()` 등 테스트 데이터 |
| `assert_p99_under()` | HDR histogram 기반 레이턴시 검증 |

### 13.2 integration — E2E 통합 테스트

**경로**: `crates/testing/integration/`

Mock Gate REST → strategy roundtrip 테스트. Phase 2 E 에서 추가.

---

## 14. 핫패스 설계 원칙

이 코드베이스는 **sub-millisecond 레이턴시**를 목표로 한다. 핫패스 (publisher WS 수신 → strategy 주문 제출) 에서 지켜야 할 원칙:

### 14.1 Zero-Allocation

| 패턴 | 적용 위치 |
|------|----------|
| `Arc<str>` intern | Symbol, client_id — 첫 생성 이후 clone = refcount bump |
| `JsonScratch` 버퍼 재사용 | WS 프레임 파싱 (clear + fill) |
| `drain_into(&mut vec)` | ZMQ 배치 수신 (caller Vec 재사용) |
| `#[repr(C)]` value type | BookTicker, Trade — stack 복사 |
| Borrow deserialization | `GateFrame<'a>` — DOM 없이 직접 파싱 |

### 14.2 Lock-Free

| 패턴 | 적용 위치 |
|------|----------|
| `ArcSwap<T>` | SymbolMetaCache, PositionCache — read = atomic load |
| `DashMap<K,V>` | LastOrderStore — per-shard read lock (수 ns) |
| `AtomicU64` counters | CounterKey, GaugeKey — fetch_add / store |
| Seqlock (atomic) | QuoteSlot — writer/reader 간 mutex 없음 |
| Lamport SPSC | OrderRing — atomic load/store 만 |

### 14.3 Anti-Jitter

| 패턴 | 적용 위치 |
|------|----------|
| `mimalloc` allocator | 모든 바이너리 — uniform latency |
| CPU affinity | publisher (4-7), gateway (8-9), strategy (0-3) |
| 64B cache-line padding | SHM 구조체 — false sharing 방지 |
| Non-blocking sender | ZMQ `send_dontwait`, SHM ring `try_write` |
| Background provider | AccountPoller, BalanceSlot — REST 호출은 bg thread |

### 14.4 핫패스 금지 사항

- `String` 생성 (Arc<str> 대신)
- `Mutex::lock()` (ArcSwap / DashMap 대신)
- `reqwest` HTTP 호출 (background task 에서만)
- `tracing::info!` 이상 level 의 per-event 로깅 (feature gate)
- `Vec::push` 반복 (pre-allocated buffer 재사용)

---

## 15. 의존성 그래프

```
                    hft-types
                       │
              ┌────────┼────────┐
              ▼        ▼        ▼
          hft-time  hft-config  hft-exchange-api
              │        │              │
              ▼        │         ┌────┴────────┐
        hft-telemetry  │         ▼             ▼
              │        │   hft-exchange-   hft-exchange-
              │        │   common          rest
              │        │         │              │
              │        │         └──────┬───────┘
              │        │                ▼
              │        │         hft-exchange-gate
              │        │         (binance/bybit/...)
              │        │
              ▼        ▼
          hft-protocol ◄──── hft-shm
              │                  │
              ▼                  ▼
         hft-ipc           hft-order-egress
              │                  │
              ▼                  ▼
         hft-common        hft-strategy-shm
              │
              ▼
    ┌─────────┼──────────┐
    ▼         ▼          ▼
strategy   publisher  order-gateway
    │
    ├── hft-strategy-config
    ├── hft-strategy-core
    ├── hft-strategy-runtime
    ├── hft-state
    └── hft-account
```

**원칙**: 하위 크레이트는 상위를 의존하지 않는다. `hft-types` → `hft-protocol` → `hft-shm` → 서비스 방향으로만 의존.

---

## 16. 아키텍처 문서 목록

| 문서 | 위치 | 내용 |
|------|------|------|
| SHM 설계 | `docs/architecture/SHM_DESIGN.md` | 공유 메모리 레이아웃, seqlock, SPMC/SPSC |
| Multi-VM 토폴로지 | `docs/architecture/MULTI_VM_TOPOLOGY.md` | ivshmem 기반 publisher ↔ strategy[N] |
| ADR-0001 | `docs/adr/0001-monorepo-layout.md` | Workspace 구조 결정 근거 |
| ADR-0002 | `docs/adr/0002-exchange-trait.md` | ExchangeFeed/Executor trait 경계 |
| ADR-0003 | `docs/adr/0003-multi-vm-ivshmem-topology.md` | SharedRegion multi-VM 설계 |
| ADR-0004 | `docs/adr/0004-result-path-scope.md` | 주문 결과 back-channel |
| ADR-0005 | `docs/adr/0005-price-regime-wire-unification.md` | ×1e8 정수 스케일링 |
| 핫패스 규칙 | `docs/phase1/hot-path-rules.md` | Zero-alloc, lock-free 원칙 |
| 레이턴시 예산 | `docs/phase1/latency-budget.md` | 50ms e2e, 7-stage 분배 |
| Jitter 플레이북 | `docs/phase1/jitter-playbook.md` | 레이턴시 변동 디버깅 |
| 모듈 배선도 | `docs/phase1/module-wiring.md` | 서비스 간 연결 다이어그램 |
| 인프라 요구사항 | `docs/runbooks/INFRA_REQUIREMENTS.md` | CPU, hugepage, ZMQ 튜닝 |
| 기능 체크리스트 | `check.md` (프로젝트 루트) | 레거시 → 신규 매핑 + 진행 상태 |

---

## 17. 주의사항 & 함정

### 17.1 치명적 (데이터 오염 / 자금 손실 가능)

1. **SHM 레이아웃 불일치**: publisher 와 strategy 의 `N_MAX`, `QUOTE_SLOTS`, `TRADE_RING_CAPACITY`, `ORDER_RING_CAPACITY` 가 다르면 torn read 발생. `layout_digest()` 검증 필수.

2. **Seqlock 3-phase 단축 금지**: QuoteSlot 의 in-progress marker → payload → commit 3단계를 2단계로 줄이면 capacity-1 gap 에서 torn read 재발 (검증됨).

3. **OrderRing SPSC 위반**: 하나의 vm_id 에 strategy 프로세스 2개를 붙이면 ring corruption 발생.

4. **AccountMembership::none() 기본값**: 초기화 안 하면 모든 주문이 거부된다. `fixed(symbols)` 로 명시 설정 필요.

5. **Wire 인코딩 변경**: `hft-protocol` 의 offset/size 변경은 레거시 호환 깨짐. golden-byte 테스트 필수.

### 17.2 운영 (서비스 장애 가능)

6. **서비스 기동 순서**: publisher → order-gateway → strategy. publisher 미기동 시 SHM attach 실패.

7. **디스크 부족**: `cargo build` target 디렉터리 10GB+. `cargo clean` 으로 정리.

8. **Redis 없이 운영**: 가능하지만 재시작 시 last_order 초기화됨. position tracking 이 리셋되므로 주의.

9. **Supabase 장애**: `ArcSwap` 캐시로 마지막 유효 설정 유지. 하지만 설정 변경이 반영 안 됨.

10. **Gate API rate limit**: 10 req/s. AccountPoller 1-2s 주기는 단일 계정에서 안전. 멀티계정 다수 시 semaphore 필요.

### 17.3 개발 (빌드 / 테스트)

11. **workspace dep 통일**: 모든 external crate 는 `[workspace.dependencies]` 에 선언. 개별 Cargo.toml 에서 버전 직접 지정 금지.

12. **feature flag**: `hft-telemetry` 의 `verbose-trace` feature 는 release 빌드에서 비활성화 필수 (trace! 호출이 핫패스에 들어감).

13. **테스트 격리**: `hft-state` 테스트는 실 Redis 필요 없음 (graceful degrade 로 skip). `integration` 테스트는 ZMQ context 격리 필요.

14. **Korean 주석 컨벤션**: 모든 코드에 한국어 주석 + 영문 doc-comment 병행. 새 코드도 이 컨벤션 유지.

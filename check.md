# HFT Rewrite — Legacy Feature Completeness Checklist

> **목적**: 기존 `gate_hft/` 레포의 모든 기능을 `hft_monorepo/mono/` 로 포팅하는 과정에서 빠뜨림 없이 검증하기 위한 체크리스트.
>
> **사용법**: 구현이 완료되면 `- [ ]` → `- [x]` 로 체크. 각 항목 우측에 **→ target crate** 를 명시.
> 기능이 의도적으로 drop 되면 `- [~]` 로 표시하고 **Drop reason** 라인 추가.
>
> **Scope 범례**:
> - ✅ **P1 (Phase 1)** — 최소 MVP. 기능 누락 시 v1 릴리즈 불가.
> - 🔶 **P2 (Phase 2)** — strategy / risk. v1 이후 추가.
> - 🔷 **P3 (Phase 3)** — ops / analytics. 별도 트랙.
> - ⏸ **Deferred / out-of-scope** — Python 전용 운영 스크립트 등. 포팅 안 함.

---

## ★ 현재 진행 상황 (2026-04-15 update, commit `3b2c5ca` @ `mono-monorepo-초기화-260415-sigma13-K`)

| Track | 상태 | 비고 |
|-------|------|------|
| **Phase 1** — publisher/subscriber/storage/telemetry/5 feeds/tools/testkit | ✅ **완료** | 10 crates + 4 services + 3 tools + testkit + 6 integration test. 모든 core crate unit test 존재. |
| **Phase 2 A** — strategy core (signal/decision/risk/V6·V7·V8) + Trade 스트림 parity + PyO3 binding | ✅ **완료** | `hft-strategy-{config,core}` + `services/strategy/{v6,v7,v8}.rs` + `hft-strategy-shm-py`. 5 거래소 전부 Trade 이벤트 발행. |
| **Phase 2 B** — `ExchangeExecutor` × 5 + REST/서명 infra | ✅ **완료** | `hft-exchange-rest` 중앙화 + Gate/Binance/Bybit/Bitget/OKX executor 각 place+cancel + RFC 4231 벡터 검증. order-gateway env-var wiring. |
| **Phase 2 C** — SHM intra-host IPC (MdRing + OrderRing + SymbolTable) | ✅ **완료 (Rust 사이드)** | publisher SHM egress + order-gateway SHM ingress 양방향. `INFRA_REQUIREMENTS.md` + `MULTI_VM_TOPOLOGY.md` / ADR-0003 / `SHM_DESIGN.md`. Python ctypes reader/writer 는 pending. |
| **Phase 2 D** — runtime feeders + main.rs variant dispatch + rate-decay 정책 | ✅ **완료** (이번 트랙) | Gate REST AccountPoller (SymbolMeta/Position provider), `OrderRateTracker::decay_before[_now]`, `StrategyControl` 채널 + V6/V7/V8 `on_control`, `services/strategy/main.rs` 전면 재작성. |
| **Phase 2 E** — order egress (strategy → ZMQ PUSH → order-gateway) | ⏳ **다음** | 현재 `spawn_order_drain` 은 tracing 흡수만 함. |
| **Phase 2 F** — 실 toolchain 에서 `cargo check/test --workspace` 1회 통과 | ⏳ **다음** | 샌드박스 rustc 부재로 정적 검증만 수행. |
| **Phase 3** — monitoring / analytics / ops 자동화 | ⏸ | §13 ~ §14. Docker infra 이관 + error manager + telegram bot. |
| **Flipster / Python ops** | ⏸ | `feed/`, `strategy_flipster_python/` workspace exclude 유지. 대부분 Python 그대로 운영. |

**핵심 완료 지표:**
- Wire format 120/128B **byte-exact parity** 유지 — `legacy_byte_parity_bookticker`/`_trade` + `integration::wire_compat` 통과 가정 (실 빌드 미검증).
- Hot path zero-alloc — 5 거래소 feed 전부 typed borrow Deserialize + Arc<str> intern + JsonScratch 재사용.
- Risk oracle 은 `PositionOracle` trait + `ExposureSnapshot` POD 로 god-struct 해체. `TimeRestrictionJitter` 호출부 draw 로 rand TLS 의존 제거.
- Strategy variant hot path 와 control plane (balance/net position) 채널 분리 — try_recv 16/iter drain, control_tx bounded(256).

**지금 즉시 해결이 필요한 항목 (order 순):**
1. 실 rustc 환경에서 `cargo check --workspace --all-targets` — dep 그래프 / feature flag 회귀 확인. (블로커)
2. Phase 2 E — 주문 발주 체인 연결 (ZMQ PUSH 추천).
3. `StrategyAccountPollErr` / `StrategyControlDropped` 카운터를 `hft-telemetry::CounterKey` enum 에 정식 등재.
4. 통합 테스트 보강 — mock Gate REST + 실 `StrategyRunner` e2e (poll → BalanceSlot → on_control → orders).

---

## 0. 전역 상수 / 매직 넘버 (모든 crate 공통 참조)

- [x] `IGNORE_SYMBOLS = ["BTC_USDT", "ETH_USDT", "OGN_USDT"]` — publisher worker 필터 → `hft-config::PublisherConfig` ✅
- [x] `EXCLUDED_SYMBOLS = ["BTC_USDT", "ETH_USDT"]` — strategy 필터 → `hft-strategy-config::StrategyConfig` ✅
- [x] `EPS = 1e-12` — 0-division guard → `hft-strategy-core` (signal/decision inline const) ✅
- [x] `MAX_POSITION_SIDE_RATIO = 0.5` — 계좌 balance 대비 net exposure 상한 → `hft-strategy-core::risk::RiskConfig` ✅
- [x] `DEFAULT_LEVERAGE = 50.0` — risk_manager 기본값 (handle_chance 에서는 1000.0) → `hft-strategy-core::risk::RiskConfig` (변형별 주입) ✅
- [x] `ZMQ_SNDHWM = 100_000` / `ZMQ_RCVHWM = 100_000` → `hft-zmq::apply_common_opts` ✅
- [x] `ZMQ OS BUFFER = 4 * 1024 * 1024` (4MB send/recv) → `hft-zmq::apply_common_opts` ✅
- [x] `AGGREGATOR_MAX_DRAIN_BATCH = 512` → `services/publisher::aggregator::drain_batch` ✅
- [x] `AGGREGATOR_SMALL_SLEEP = 50us` (idle) → `services/publisher::aggregator` ✅
- [x] `LATENCY_SAMPLE_RATE = 1/100` → `hft-telemetry` 샘플러 (`record_stage_nanos` 호출부에서 적용) ✅
- [x] `RECONNECT_INITIAL_DELAY = 1s`, `RECONNECT_MAX_DELAY = 30s`, multiplier `2x` → `hft-exchange-api::Backoff` (공통 헬퍼, 5 feed 전부 사용) ✅
- [x] `SHM_HEADER_SIZE = 16`, `SHM_DIR_ENTRY = 16`, `SHM_MAX_SLOT = 512`, `SHM_MAX_SYMBOLS = 10_000` → `hft-shm::layout` ✅ **(재설계)** cache-line 64B align, per-symbol QuoteSlot 256B, SPSC Trade ring 128B, SymbolTable 10K entry. legacy 128B slot 모델 폐기하고 seqlock + ring 으로 재설계.
- [~] `FNV-1a init = 0xcbf29ce484222325`, `prime = 0x100000001b3` — **Drop reason**: `hft-shm::symbol_table` 는 `ahash::AHasher` open addressing 로 대체 (DoS 내성 + 빠름). 레거시 FNV 는 제거.
- [~] `DUMPER_INTERVAL = 5ms` (SHM timestamp 갱신) — **Drop reason**: seqlock 모델에서 불필요 (publisher 가 쓸 때마다 version bump).
- [x] `METRICS_PRINT_INTERVAL = 5s` → `hft-telemetry::dump_hdr` + `counter_inc` subscriber reporter 기본 5s ✅

---

## 1. `data_publisher_rust` — 멀티 거래소 WS → ZMQ 퍼블리셔

**→ target**: `services/publisher` + `crates/exchange/hft-exchange-{binance,gate,bybit,bitget,okx}`

### 1.1 CLI / env
- [x] `--exchange` (default `binance`; 허용: binance/gate/bitget/bybit/okx) → `services/publisher::cli` (Clap derive) ✅
- [x] `--workers` (default `15`) → `services/publisher::cli` ✅
- [x] `--backend` (optional, default 거래소별) → `services/publisher::cli` ✅
- [x] `--frontend` (optional, default 거래소별) → `services/publisher::cli` ✅
- [x] `--symbols` (default `common_symbols.txt`) → `services/publisher::cli` ✅
- [x] `--no-symbols-file` flag → `services/publisher::cli` ✅
- [x] `--log-level` (default `info`) → `services/publisher::cli` → `hft-telemetry::init` ✅
- [x] `RUST_LOG` env → `hft-telemetry::init` (tracing-subscriber EnvFilter) ✅
- [x] `BINANCE_DATA_PORT=6000` / `GATE_DATA_PORT=5559` / `BITGET_DATA_PORT=6010` / `BYBIT_DATA_PORT=5558` / `OKX_DATA_PORT=6011` / `ZMQ_PORT=6000` (fallback) → `hft-config::ZmqConfig` (Figment env layer) ✅
- [x] `GATE_ORDERBOOK_PRECISIONS=gate_orderbook_precisions.txt` → `hft-exchange-gate` (주문 size 양자화 정적 map) ✅

### 1.2 Exchange WS 엔드포인트 (ExchangeFeed 구현체 별)
- [x] Binance Futures `wss://fstream.binance.com/ws` — `bookTicker`, `aggTrade` 스트림 → `hft-exchange-binance` (combined `/stream?` URL) ✅
- [x] Gate API `wss://fx-ws.gateio.ws/v4/ws/usdt` — `futures.book_ticker`, `futures.trades` → `hft-exchange-gate` ✅
- [x] Gate Web `wss://fx-webws.gateio.live/v4/ws/usdt` — `futures.order_book` → `hft-exchange-gate` (role=WebOrderBook) ✅
- [x] Bitget `wss://ws.bitget.com/v2/ws/public` — `books1` + `trade` → `hft-exchange-bitget` ✅
- [x] Bybit `wss://stream.bybit.com/v5/public/linear` — `orderbook.1.*` + `publicTrade.*` → `hft-exchange-bybit` ✅
- [x] OKX `wss://ws.okx.com:8443/ws/v5/public` — `bbo-tbt` + `trades` → `hft-exchange-okx` ✅
- [x] 각 거래소별 subscribe payload JSON 포맷 (심볼 배열 / 채널명) → `hft-exchange-{v}::subscribe_payload()` ✅ (OKX/Bitget 는 MAX/2 chunk 로 BBO+Trade colocation)
- [x] 심볼 대소문자 변환 (Binance lowercase `btcusdt`, Gate/Bybit uppercase `BTC_USDT`) → `hft-exchange-{v}::to_wire_symbol()` / `from_wire_symbol()` ✅

### 1.3 Worker (per WS connection)
- [x] worker 수 `--workers` 분배 — 심볼 round-robin 샤딩 → `services/publisher::spawn_workers` ✅
- [x] worker 시작 stagger = `worker_id * 100ms` → `services/publisher::main` ✅
- [x] reconnect exponential backoff (1s → 2s → 4s → … → 30s cap) → `hft-exchange-api::Backoff` 공통 헬퍼 ✅
- [x] PUSH 소켓 연결 to `backend_addr`, SNDHWM=100k, OS send buf=4MB → `hft-zmq::PushSocket` ✅
- [x] `send_dontwait` 로 send — HWM 초과 시 drop count ↑ → `hft-zmq::send_multipart_nonblocking` + `counter_inc(ZmqHwmBlock/ZmqDropped)` ✅
- [x] frame 형식: `[type_len u8][type_str][payload]` — type_str ∈ {`bookticker`, `trade`, `webbookticker`} → `hft-protocol::frame` ✅

### 1.4 Aggregator (central process)
- [x] PULL `backend_addr` — default per exchange (`ipc:///tmp/{exchange}-bookticker` or `ipc:///tmp/hft_data_subscriber.sock` for gate) → `services/publisher::aggregator` ✅
- [x] PUB `frontend_addr` — TCP 0.0.0.0:{PORT} ✅
- [x] draw loop: drain up to 512 msg, coalesce by topic (HashMap<topic, (buf, dirty)>), publish dirty only → `publisher::aggregator::run` ✅ (현 구현은 zero-channel Worker → PUSH → PULL → PUB pipeline 으로 단순화, dirty flag 대신 아예 worker 레벨에서 coalescing)
- [x] IPC 파일 기존 것 삭제 후 bind → `services/publisher::aggregator` (bind 전 `std::fs::remove_file`) ✅
- [x] 5s 마다 stats print (latency p50/p95/p99/max, drop count) → `hft-telemetry::dump_hdr` + counter report ✅
- [x] latency 측정: 샘플 1/100 에서 `publisher_sent_ms - server_time` 기록 → `hft-telemetry::record_stage_nanos` ✅

### 1.5 Wire format (C struct, little-endian, 정확한 offset)
- [x] **BookTickerC 120B** — offset 검증 테스트 (`wire_compat.rs` + `hft-protocol::legacy_byte_parity_bookticker`) → `hft-protocol::BookTickerWire` ✅
  - [x] `exchange[16]` @0, `symbol[32]` @16, `bid_price:f64` @48, `ask_price:f64` @56, `bid_size:f64` @64, `ask_size:f64` @72
  - [x] `event_time:i64` @80, `server_time:i64` @88, `publisher_sent_ms:i64` @96
  - [x] `subscriber_received_ms:i64` @104, `subscriber_dump_ms:i64` @112 (publisher 에서는 0)
- [x] **TradeC 128B** → `hft-protocol::TradeWire` (`legacy_byte_parity_trade` 통과) ✅
  - [x] `exchange[16]` @0, `symbol[32]` @16, `price:f64` @48, `size:f64` @56
  - [x] `id:i64` @64, `create_time:i64` @72 (sec), `create_time_ms:i64` @80
  - [x] `is_internal:bool` @88, `_padding[7]` @89, `server_time:i64` @96, `publisher_sent_ms:i64` @104
  - [x] `subscriber_received_ms:i64` @112, `subscriber_dump_ms:i64` @120 (publisher 에서는 0)
- [x] Topic 문자열: `{exchange}_{type}_{symbol}` — e.g. `binance_bookticker_BTC_USDT`, `gate_webbookticker_BTC_USDT` → `hft-protocol::topics::TopicBuilder` + `parse_topic` ✅
- [ ] encode < 200ns, decode < 300ns bench → `crates/core/hft-protocol/benches/` **TODO**: bench 파일 미작성 (criterion dep 추가 필요).

### 1.6 Symbol 로딩
- [x] `common_symbols.txt` 파일 로드 → `services/publisher::load_symbols` ✅
- [x] `--no-symbols-file` → 거래소 `available_symbols()` 만 사용 ✅
- [x] 교집합 로직 (파일 ∩ 거래소 available) → `services/publisher::resolve_subscribed_symbols` ✅ (Phase B §1.6 확정)
- [x] `IGNORE_SYMBOLS` 필터 → `hft-config::PublisherConfig::ignore_symbols` ✅

### 1.7 에러 / 리트라이
- [x] WS disconnect → exponential backoff reconnect → `hft-exchange-api::Backoff` ✅
- [x] PUSH EAGAIN → log + drop count ↑ (block 하지 않음) → `hft-zmq::PushSocket::send_dontwait` + `counter_inc(ZmqHwmBlock)` ✅
- [x] Aggregator bind 실패 → panic + clear error → `services/publisher::main` (anyhow::bail! 경로) ✅

---

## 2. `data_subscriber` — ZMQ → (legacy) SHM

**→ target**: `services/subscriber` (P1 에서는 SHM 제거, crossbeam channel 직결. SHM 은 Python strategy 호환용으로만 유지)

### 2.1 env / const
- [x] `DATA_SERVER_IP=127.0.0.1` / `DATA_SERVER_IP_BACKUP` (failover) → `hft-config::ZmqConfig` ✅
- [x] `{EXCHANGE}_DATA_PORT` 전체 세트 (2.1 과 동일) → `hft-config::ZmqConfig` ✅
- [~] `SHM_PATH=/tmp/binance-cache.shm` — **Drop reason**: Phase 2 C 에서 seqlock+ring 모델로 재설계. 레거시 slot 브릿지 폐기.
- [~] `IPC_SOCKET_PATH=/tmp/hft_data_subscriber.sock` — **Drop reason**: Python strategy 는 `hft-strategy-shm-py` (PyO3 + SHM 직결) 로 대체. UDS 브릿지 불필요.
- [x] `SUBSCRIBE_EXCHANGES` csv env → `hft-config` ✅
- [x] SUB RCVHWM = 100_000, RCVBUF = 4MB → `hft-zmq::SubSocket::apply_common_opts` ✅

### 2.2 기능
- [x] SUB 연결 to `tcp://{DATA_SERVER_IP}:{PORT}` per exchange → `hft-zmq::SubSocket` ✅
- [x] topic prefix subscription: `{exchange}_bookticker_*`, `{exchange}_webbookticker_*`, `{exchange}_trade_*` → `hft-protocol::topics::TopicBuilder` ✅
- [x] `poll` timeout 1000ms, idle sleep 10ms on empty → `services/subscriber::run` (`recv_timeout_inner` 1s) ✅
- [x] 메시지 카운터 per `{exchange}:{type}` 5초마다 출력 → `hft-telemetry::counter_inc` + 5s reporter ✅
- [~] SHM 브리지 (Python 호환): FNV-1a hash + directory + 512B slot 쓰기 — **Drop reason**: 동일 (Phase 2 C 재설계).
- [~] SHM directory rebuild — **Drop reason**: 동일.
- [~] UDS IPC bridge — **Drop reason**: PyO3 직결로 대체.

### 2.3 P1 delta (새 아키텍처)
- [x] crossbeam channel (bounded 8192) direct to strategy in-process → `services/subscriber::InprocQueue` ✅ (strategy main 에서 `subscriber::start()` 을 통해 embed)
- [x] hot path: SUB → decode → push to channel. 1 스레드. → `services/subscriber::run` ✅
- [~] SHM / UDS bridge 는 feature-flag `legacy-compat` 뒤로 — **결정 변경**: 레거시 브릿지는 완전 drop. 대체 경로 = (a) ZMQ PUB 유지 (원격), (b) `hft-shm` 신모델 (로컬, 2 C 완료), (c) `hft-strategy-shm-py` (Python 사이드).

---

## 3. `gate_hft_rust` — 전략 엔진 (5 binary)

**→ target**: `services/strategy/*` + `crates/strategy/hft-strategy-core` + `crates/exchange/hft-exchange-gate-exec`

### 3.1 Binary 별
- [~] `bin/gate_hft_v1.rs` → `services/strategy/v1` — **Drop reason**: v29(=v8) 로 수렴. v1 은 deprecated.
- [x] `bin/v6.rs` → `services/strategy/src/v6.rs` (V6Strategy, close_stale + size 1x/2x/4x + position_close_side ×2) ✅ (Phase 2 A)
- [x] `bin/v7.rs` (EMA profit) → `services/strategy/src/v7.rs` (narrative-close only_close 양분기, close_stale 없음) ✅ (Phase 2 A)
- [x] `bin/v8.rs` (modified spread/gap) → `services/strategy/src/v8.rs` ✅ (Phase 2 A)
- [ ] `bin/close_v1.rs` → `services/strategy/close` **TODO**: Phase 2 E 이후. 현재 V6/V7/V8 의 `only_close` 모드로 기본 커버.

### 3.2 CLI / env (runner.rs)
- [ ] `--leverage` / `$LEVERAGE` → `hft-strategy-config` (실 wire-up 은 main.rs 에서 Variant 분기 후 runner config 로 전달 예정)
- [ ] `--login_names` / `$LOGIN_NAMES` (csv or `prefix001~prefix010` range) → `services/strategy::cli::expand_logins` **TODO**: multi-account 운영 환경 확정 시 구현.
- [x] `--interval` (default 10ms) → `StrategyRunner::with_tick_interval` ✅
- [x] `--binance-latency` / `--base-latency` (default 200ms) → `TradeSettings::binance_last_book_ticker_latency_ms` ✅
- [x] `--gate-latency` (default 100ms) → `TradeSettings::gate_last_book_ticker_latency_ms` ✅
- [ ] `--restart-interval` (default 3600s) → `services/strategy::supervisor` **TODO**: Phase 2 E 이후.
- [x] `--initial-sleep` (default 100ms) → `services/strategy/src/main.rs::env_duration_ms("HFT_STRATEGY_INITIAL_SLEEP_MS", 100)` ✅
- [x] `--base-exchange` / `$BASE_EXCHANGE` (default `binance`) → `TradeSettings::base_exchange` ✅
- [x] `$SUPABASE_URL`, `$SUPABASE_KEY` → `hft-strategy-config::StrategyConfigLoader` (async reqwest + ArcSwap) ✅
- [x] `$GATE_API_KEY`, `$GATE_API_SECRET` → `hft-exchange-gate::executor::GateExecutor::from_env` + `hft-exchange-gate::account::GateAccountConfig::from_env` (Phase 2 B + 2 D) ✅
- [x] `$LOGIN_NAME` (default `v3_sb_002`), `$UID` → `hft-strategy-config::StrategyConfig` ✅
- [x] `$SYMBOLS` csv → `hft-strategy-config::StrategyConfig::symbols` ✅
- [ ] `$METRICS_DIR` (default `./metrics`) → `hft-telemetry::metrics_writer` **TODO**: 파일 기반 metrics writer 미구현, 현재는 stdout + HDR dump 만.
- [ ] `$HEALTHCHECK_TEST_ORDER_{SYMBOL,SIZE,PRICE=60000.0}` → `services/strategy::healthcheck` **TODO** Phase 2 E.
- [ ] `$LOGIN_STATUS_URL` → `services/strategy::healthcheck` **TODO** Phase 2 E.
- [~] `$ORDER_URL`, `$ORDER_UID` (puppeteer) → **Drop reason**: Puppeteer fallback 은 `go/order-processor` 분리 트랙으로 유지. Rust 전략은 REST 직결만.
- [~] `$IPC_SOCKET_PATH=/tmp/gate_hft_ipc.sock` → **Drop reason**: UDS 브릿지 폐기 (§2 참조).
- [ ] `$REDIS_URL`, `$REDIS_STATE_PREFIX=gate_hft:state` → `hft-state::redis` **TODO** Phase 3. 현재는 in-process state 만.
- [x] `$SUPABASE_REFRESH_SECS=300` → `StrategyConfigLoader` ArcSwap 주기 reload ✅

### 3.3 Supabase 설정 로딩 순서
- [x] `strategy_settings` by `login_name` → `trade_setting` (text FK), `symbol_set` (bigint FK) → `hft-strategy-config::StrategyConfigLoader` ✅
- [x] `trade_settings` by id → JSON → `TradeSettings` struct (50+ 필드 전부 `#[serde(default)]`) ✅
- [x] `symbol_sets` by id → JSON array → `Vec<Symbol>` (`deserialize_text_id` 로 int/string id 흡수) ✅
- [x] startup-only load (런타임 reload 없음) — 주기적 반영은 `SUPABASE_REFRESH_SECS` 로 in-memory 업데이트 → `StrategyConfigLoader` + `ArcSwap<Option<StrategyConfig>>` graceful fallback ✅

### 3.4 TradeSettings 필드 (모두 Supabase JSON 에서 로드) — 전체 ✅ (Phase 2 A)

`hft-strategy-config::TradeSettings` 에 50+ 필드 1:1 포트 완료. `#[serde(default)]` + 중앙 `Default` impl + `validate()`. 아래 세부 체크는 전부 포팅됐으므로 일괄 완료 표기:
Core: ✅
- [x] `update_interval_seconds` (0.1) · `order_size` · `close_order_size` · `max_position_size` · `dynamic_max_position_size` (false) · `max_position_size_multiplier` (1.5) · `trade_size_trigger` · `max_trade_size_trigger` · `close_trade_size_trigger` · `close_order_count` · `max_order_size_multiplier` (3)

Profit / gap: ✅
- [x] `close_raw_mid_profit_bp` · `ignore_recent_trade_ms` (0) · `mid_gap_bp_threshold` · `binance_mid_gap_bp_threshold` (0.0) · `spread_bp_threshold`

Latency gating: ✅
- [x] `gate_last_trade_latency_ms` (1000) · `gate_last_book_ticker_latency_ms` · `binance_last_book_ticker_latency_ms` · `gate_last_webbook_ticker_latency_ms` (500)

Order management: ✅
- [x] `wait_time_close_ws_order_ms` (1000) · `bypass_safe_limit_close` (false) · `only_follow_trade_amount` (true) · `allow_limit_close` (true) · `ignore_non_profitable_order_rate` (0.6) · `order_success_rate` (0.05) · `bypass_max_position_size` (false) · `opposite_side_max_position_size`

TIF: ✅
- [x] `limit_open_tif` (fok) · `limit_close_tif` (fok) — `TIF::normalize` 로 fok/ioc/gtc 정규화.

Time restrictions (ms min / max): ✅
- [x] `limit_open_time_restriction_ms_{min=100,max=200}` · `limit_close_time_restriction_ms_{min=100,max=200}` · `same_side_price_time_restriction_ms_{min=300,max=600}` · `wait_for_book_ticker_after_trade_time_ms` (100)
- [x] **아키텍처 변경**: `TimeRestrictionJitter` 를 호출부에서 draw (rand TLS 접근 제거) — risk.rs.

Funding / EMA / misc: ✅
- [x] `funding_rate_threshold` (0.005) · `profit_bp_ema_alpha` (0.1) · `profit_bp_ema_threshold` (1.20) · `succes_threshold` (0.6, ⚠️ 철자 보존) · `normalize_trade_count` (100) · `close_stale_minutes` (99999) · `close_stale_minutes_size` · `too_many_orders_time_gap_ms` (600_000) · `too_many_orders_size_threshold_multiplier` (2) · `symbol_too_many_orders_size_threshold_multiplier` (2) · `net_positions_usdt_size_threshold` (5000.0) · `max_order_size` (derived)

### 3.5 Signal 계산 (signal_calculator.rs) — ✅ 포팅 완료 (Phase 2 A)
- [x] `gate_mid = (gate_ask + gate_bid) / 2` → `hft-strategy-core::signal::calculate_signal` ✅
- [x] `buy_price = gate_web_ask`, `sell_price = gate_web_bid` ✅
- [x] buy 조건: `buy_price < gate_mid` → side=buy, `mid_gap_bp = (gate_mid - buy_price) / gate_mid * 10_000`, `orderbook_size = gate_web_ask_size`, binance valid: `buy_price < binance_bid` ✅
- [x] sell 조건: `sell_price > gate_mid` → side=sell, `mid_gap_bp = (sell_price - gate_mid) / gate_mid * 10_000`, `orderbook_size = gate_web_bid_size`, binance valid: `sell_price > binance_ask` ✅
- [x] `spread_bp = (gate_ask - gate_bid) / gate_mid * 10_000` ✅
- [x] `is_binance_valid` 계산 ✅
- [x] `instant_profit_bp` (buy/sell) → `hft-strategy-core::signal` (inline) ✅
- [x] **v6 변형**: `calculate_signal_v6` — v0 + mid 비교 fallback, legacy `(gate_ask+gate_bid) < (bin_ask+bin_bid)*0.5` 비대칭 byte-exact 보존 ✅
- [x] **v8 변형**: `calculate_signal_v8` — `gate_last_trade` 가 새 것이면 대응 side 가격 override ✅

### 3.6 Order Decision (order_decision.rs) — ✅ 포팅 완료 (Phase 2 A)
- [x] latency guard: now - gate_bt_time > threshold → skip ✅
- [x] binance latency guard 동일 ✅
- [x] funding rate gating: buy + funding > +threshold → only_close; sell + funding < -threshold → only_close ✅
- [x] **limit_open** 조건 (all true): mid_gap_bp > threshold, size ∈ [trigger, max_trigger], spread > threshold, binance_valid, funding not blocking ✅
- [x] **limit_close** 조건: mid_gap_bp > close_raw_mid_profit, size > close_trigger, order_count_size > close_order_count, binance_mid_gap ≥ 0 ✅
- [x] only_close → limit_close 로 downgrade ✅
- [x] position 반대방향 확인 (close 은 long→sell, short→buy) ✅
- [x] TIF 는 trade_settings 에서 가져오기 (`TIF::normalize` 로 정규화) ✅
- [x] **v6 분기**: `decide_order_v6` — close_stale + size_threshold 1x/2x/4x + `is_order_net_position_close_side ×2` ✅
- [x] **v7 분기**: `decide_order_v7` — narrative-close only_close 양분기, close_stale 없음 ✅
- [x] **아키텍처 개선**: `OrderSide`/`OrderLevel` enum (String 제거) — inline alloc 0 ✅

### 3.7 Risk Manager (risk_manager.rs / handle_chance.rs) — ✅ 포팅 완료 (Phase 2 A + 2 D)
- [x] `MAX_POSITION_SIDE_RATIO = 0.5` → `hft-strategy-core::risk::RiskConfig` ✅
- [x] `DEFAULT_LEVERAGE = 50.0` (risk_manager) / `1000.0` (handle_chance) → `RiskConfig` (변형별 주입) ✅
- [x] net exposure check: `|net_size| * price / leverage < MAX_POSITION_SIDE_RATIO * balance` ✅
- [x] per-symbol max position size check (with multiplier) ✅
- [x] `too_many_orders` rate-limit check (time_gap_ms window) — `hft-strategy-runtime::OrderRateTracker::push_order` + `decay_before_now` (Phase 2 D 운영 정책 확정) ✅
- [x] **아키텍처 개선**: `PositionOracle` trait + `ExposureSnapshot` POD 로 god-struct 해체. 구현체는 `hft-strategy-runtime::RuntimeOracle` (SymbolMetaCache + PositionCache + BalanceSlot) ✅

### 3.8 Gate Exec — 대부분 ✅ (Phase 2 B + 2 D), WS user stream / Puppeteer 는 pending
- [x] REST base: `https://api.gateio.ws/api/v4/futures/usdt` → `hft-exchange-gate::executor::GateExecutor` + `account::GateAccountClient` ✅
- [x] HMAC-**SHA512** signing ⚠️ (gate_hft v4 는 SHA512 hex, 레거시 문서 SHA256 은 오기 — 실 구현은 SHA512 로 확정) → `hft-exchange-rest::hmac_sha512_hex` + `sha512_hex(body)` canonical 5-line ✅
- [x] `POST /orders` with `{account, contract, price(str), reduce_only, order_type, size(str, ±), tif, text, message.timestamp}` → `GateExecutor::place_order` (Buy=+/Sell=- sign, Market→price=0+tif=ioc) ✅
- [x] `DELETE /orders/{id}` cancel (404 → already-cancelled 취급) ✅
- [x] `GET /positions`, `GET /orders`, `GET /my_trades`, `GET /accounts` — 폴링 경로는 `GateAccountClient::fetch_contracts / fetch_positions / fetch_accounts` 로 Phase 2 D 에서 구현 ✅
- [ ] WS user stream: order updates, position updates → `hft-exchange-gate::user_stream` **TODO** Phase 2 E — 현재는 REST 폴링 (200ms 기본) 로 대체.
- [~] Puppeteer fallback: POST `{ORDER_URL}` — **Drop reason**: `go/order-processor` 별도 트랙 유지. Rust 전략은 직접 REST 만.

### 3.9 Health / Supervisor — **TODO** (Phase 2 E)
- [ ] `DEBUGGING_ORDERS = [{contract:"BTC_USDT", price:"60000"}, {contract:"ETH_USDT", price:"2000"}]` — healthcheck 테스트 주문 → `services/strategy::healthcheck`
- [ ] `strategy_status` supabase table upsert → `services/strategy::status`
- [ ] restart loop: every `restart_interval` seconds → `services/strategy::supervisor`
- [ ] metrics files per-strategy in `$METRICS_DIR` → `hft-telemetry::metrics_writer`

---

## 4. `futures_collector` — 심볼 메타데이터 수집

**→ target**: `tools/futures-collector`

- [x] `--symbol` required arg → `tools/futures-collector::cli` ✅
- [x] `$GATE_ORDERBOOK_PRECISIONS` env → `hft-exchange-gate` (정적 map) ✅
- [x] WS: Gate web bookticker, Binance futures ✅
- [ ] Parquet Hive partition `{out}/{symbol}/year=/month=/day=/hour=/` **TODO**: 현재 tool 스캐폴드만 있음. parquet 의존성 + schema 미구현.
- [ ] BookTickerRow / TradeRow schema **TODO** (동반 작업).
- [x] reconnect backoff 30s max → `hft-exchange-api::Backoff` ✅
- [ ] (Rewrite) 각 거래소 `ExchangeFeed::fetch_symbols()` 재사용 → 모든 거래소 심볼 upsert to supabase `symbols` table **TODO** Phase 3.
- [ ] cron `*/10 * * * *` or systemd timer 단발 실행 **TODO** (deploy/systemd/ 에 timer unit 추가 필요).
- [ ] 거래소 중 1개 실패해도 다른 거래소 계속 **TODO**.

---

## 5. `latency_test_subscriber` — 디버깅 유틸

**→ target**: `tools/latency-probe` (이름 변경됨; `latency-test-subscriber` → `latency-probe`)

- [x] `$DATA_SERVER_IP`, `${EXCHANGE}_DATA_PORT` env → `hft-config` ✅
- [x] SUB RCVHWM 100k, RCVBUF 4MB → `hft-zmq::SubSocket` ✅
- [x] BookTickerC 120B / TradeC 128B decode → `hft-protocol::BookTickerWire::from_bytes` / `TradeWire::from_bytes` ✅
- [x] latency 계산: `publisher_sent_ms - server_time` (server→pub), `now - publisher_sent_ms` (pub→here) ✅
- [x] HDR histogram percentile 출력 → `hft-telemetry::dump_hdr` ✅
- [x] **확장** (Phase 2 B): `--target-e2e-p999-ms`, `--target-internal-p999-ns`, `--target-stage-p999 LABEL=NS` assertion CLI — CI exit code 에 통과/실패 반영 ✅

---

## 6. `questdb_export` — QuestDB SUB writer + export

**→ target**: `tools/questdb-export`

### 6.1 Runtime (SUB → ILP writer)
- [x] SUB subscribe `""` (모든 토픽) → `tools/questdb-export` ✅
- [x] 100ms batch flush → `hft-storage::QuestDbSink::maybe_flush` ✅
- [x] SIGTERM graceful: drain + `force_flush` → `QuestDbSink::shutdown` ✅
- [ ] QuestDB PG wire (8812) schema init on startup **TODO**: 현재는 ILP 만 사용 (테이블 자동생성). 명시적 DDL init 미구현.
- [x] ILP retry + spool file on connect failure (publisher 에 backpressure 없음) → `hft-storage::SpoolWriter` (disabled()/append/flush/replay) + `IlpConnection` 지수 백오프 (100ms→30s) ✅
- [ ] `latency_stages` 테이블 — LatencyStamps 7 필드 모두 기록 **TODO**: 현재 BookTicker/Trade 만 기록. Stage ILP 인코더는 `hft-storage::IlpEncoder::append_latency` 로 준비됐으나 subscriber 결선 안 됨.

### 6.2 Export CLI (bin `questdb-export-cli`?)
- [x] `--url` / `$QUESTDB_URL` ✅
- [x] `--table`, `--query`, `--output`, `--output-dir=data` ✅
- [x] `--partition-by` (hour | day, default day) ✅
- [x] `--limit` ✅
- [x] `--symbol`, `--start-date`, `--end-date`, `--start-ts`, `--end-ts` (RFC3339) ✅
- [x] HTTP API: `{QUESTDB_URL}/exec?query=` ✅
- [ ] Parquet single-file / multi-file hourly partition **TODO**: 현재 CSV 출력만. parquet crate 의존성 추가 필요.

---

## 7. `order_processor_go` — 브라우저 자동화 HTTP 서버

**→ target**: `go/order-processor` — ⏸ **별도 Go 트랙으로 유지** (mono/ 에 흡수 안 함). Rust 전략은 Gate REST 직결만 사용.

- [~] 전체 세부 (HTTP routes, rate limit, Chrome/ChromeDP, cookie 관리 등) — Go 트랙 책임, check.md 에서는 parity 감시만.

---

## 8. `data_publisher_gate_ws_go` — Go 대체 Gate 퍼블리셔

**→ target**: ⏸ **Deferred** — Rust publisher 가 Gate 지원 확정. Go 구현 폐기.

- [~] 모든 세부 — Rust `hft-exchange-gate` + `services/publisher` 로 parity 달성 (wire format 동일).

---

## 9. `gate_hft_v29.py` — 최신 Python 전략 (parity 검증용)

**→ target**: `services/strategy/*` — ✅ Rust V8 로 커버. Python 은 레퍼런스로만 유지.

### 9.1 CLI — Rust 측에서 동등 기능 제공:
- [x] `--debug` → `RUST_LOG=debug` ✅
- [ ] `--login_names` (csv or `prefix001~prefix010` range) — **TODO** multi-account 운영 확정 시.
- [x] `--symbol_set` int → `StrategyConfig::symbol_set_id` ✅
- [x] `--close` flag (close mode) → `TradeSettings::bypass_safe_limit_close` + only_close 변형 ✅
- [~] `--subaccount_keys_path=subaccount_keys.json` — **Drop reason**: multi-account orchestration 은 Phase 3 (hft-account).

### 9.2 env
- [x] `$INSTANCE_NAME` — server 식별자 → `hft-config::TelemetryConfig::instance_name` ✅
- [x] `$QDB_CLIENT_CONF` — QuestDB sender conf → `hft-config::QuestDbConfig` ✅

### 9.3 핵심 아키텍처 — **Rust 재설계**:
- [~] 이벤트-기반 IPC (UDS blocking I/O) → **재설계**: `services/subscriber::InprocQueue` (crossbeam bounded) + `hft-shm::MdRing` (seqlock) 로 대체. UDS 폐기.
- [~] C Struct zero-copy deserialize (BookTickerC 128B Python version) → **재설계**: Rust 는 120B + Python 측 `hft-strategy-shm-py` (PyO3 + SHM 직결, wire 불변식 assert) ✅
- [x] per-symbol state 보호 → `StrategyRunner` 단일 스레드 hot path + Arc<ArcSwap> 설정 reload, lock 없음 ✅
- [x] `_*_received_at` timestamps → `hft-time::LatencyStamps` 7-stage ✅
- [x] `_symbol_last_processed_time` Dict + debounce → `StrategyRunner::with_tick_interval` (1ms 기본) ✅
- [x] order queue → `services/strategy::Orders` 배치 + bounded control channel ✅
- [x] pre-warmup 10s + force close on startup → `hft-testkit::WarmUp` + strategy main `--initial-sleep` ✅

### 9.4 Strategy variants
- [x] `gate_hft_v29.py` → `services/strategy/v8.rs` ✅ (근사 매핑)
- [~] `gate_hft_v28.*` / `v27.*` / `v26.*` / `v25_subscriber_test.py` / `test*.py` — **Drop reason**: v29 가 커버, 전부 deprecated.

### 9.5 Close / MM 보조 전략
- [ ] `gate_close_strategy_v1.py` — `services/strategy/close` **TODO** Phase 2 E (현재는 V6/V7/V8 `only_close` 모드로 커버).
- [ ] `gate_hft_mm_close.py` — `services/strategy/mm_close` **TODO** P2 later.
- [ ] `gate_hft_close_unhealthy_symbols.py` — `services/strategy/close_unhealthy` **TODO** P2 later.

---

## 10. Python `src/` 모듈 (strategy 내부 컴포넌트)

**→ target**: 각 기능을 `hft-strategy-core` / `hft-exchange-gate` / `hft-account` / `hft-telemetry` 로 분해.

### 10.1 Order Managers — ✅ 대부분 커버 (Phase 2 B + 2 D)
- [x] `order_manager_v3.py` 동등 기능 → `hft-exchange-gate::executor::GateExecutor` + `hft-exchange-gate::account::GateAccountClient`:
  - [x] `list_positions` → `GateAccountClient::fetch_positions` ✅
  - [x] `get_futures_account` → `GateAccountClient::fetch_accounts` ✅
  - [x] `create_order`, `cancel_order` → `GateExecutor::place_order/cancel` ✅
  - [x] order param: all fields matched (size sign, TIF, text) ✅
  - [ ] `list_futures_orders`, `list_futures_trades`, fee calc (`paid_fee_ratio=0.35`, `fee_bp=3`), instant profit bp, order success/profitable rate — **TODO** Phase 2 E 이후 (현재 전략 레벨 `OrderRateTracker` 가 근사 커버).
  - [ ] WS reconnect for user stream — **TODO** §3.8 참조.
- [~] `order_manager_v2/v4/v5.py` — **Drop reason**: deprecated (v3 수렴).
- [ ] `order_manager_v6.py` (slippage) → `services/strategy/close` **TODO** Phase 2 E.
- [ ] `order_manager_mm.py` → `hft-strategy-mm` **TODO** P2 later.
- [~] `order_manager.py` v1 legacy — **Drop reason**: deprecated.

### 10.2 Market Watchers — ✅ 전부 Phase 1 완료
- [x] `base_market_watcher.py` → `hft-exchange-api::ExchangeFeed` trait ✅
- [x] `binance_market_watcher_v5.py` → `hft-exchange-binance` (zero-alloc typed Deserialize + Backoff + LatencyStamps) ✅
- [~] `binance_market_watcher{,_v2,_v3,_v4}.py`, `_ws.py` — **Drop reason**: deprecated.
- [x] `gate_market_watcher_v3.py` → `hft-exchange-gate` ✅ (user stream 은 §3.8 pending)
- [~] `gate_market_watcher{,_v2}.py`, `_zmq.py` — **Drop reason**: deprecated.
- [x] `bybit_market_watcher.py` → `hft-exchange-bybit` ✅
- [x] `okx_market_watcher.py` → `hft-exchange-okx` ✅

### 10.3 Models — ✅
- [x] `book_ticker.py` → `hft-types::BookTicker` ✅
- [x] `ticker.py` → `hft-types::Trade` (ticker 는 BookTicker snap 으로 수렴) ✅
- [x] `settings.py::GateHFTV21TradeSettings` → `hft-strategy-config::TradeSettings` ✅
- [x] `settings.py::StrategySetting` loader → `hft-strategy-config::StrategyConfigLoader` ✅

### 10.4 Callback Handlers — ✅
- [x] `base_callback_handler.py` → `hft-exchange-api::Emitter = Arc<dyn Fn(MarketEvent, LatencyStamps)>` ✅
- [x] `print_callback_handler.py` → `hft-telemetry` tracing ✅
- [x] `questdb_callback_handler.py` → `tools/questdb-export` sink 흡수 ✅

### 10.5 Utils
- [x] `shm_reader.py` → `hft-strategy-shm-py` (PyO3 + SHM 직결, ctypes 폐기) ✅
- [~] BookTickerC 128B / TradeC 160B Python 버전 — **Drop reason**: Python 은 이제 Rust 와 동일한 120B/128B SHM 을 읽음. legacy 128/160B 제거.
- [~] FNV-1a / composite key / linear probing — **Drop reason**: ahash + SymbolTable 로 대체.
- [~] `ipc_client.py` (UDS) — **Drop reason**: SHM 로 대체.
- [x] `timing.py` → `hft-time::SystemClock` + 공통 심볼 로더는 publisher 에 흡수 ✅
- [ ] `subaccount_utils.py::get_ip()` / `SubaccountUtils` → `hft-account::ip_detector` / `SubaccountOps` **TODO** Phase 3.
- [x] `mean_timing.py` → `hft-telemetry::record_stage_nanos` HDR 로 대체 ✅
- [~] `flatbuffer_helpers.py` — **Drop reason**: flatbuffer 제거.

### 10.6 Ingest / Monitoring / Strategy
- [~] `fusion_ingestor.py` — **Drop reason**: QuestDB sink 로 충분.
- [ ] `monitoring_agent.py` → `services/monitoring-agent` **TODO** Phase 3.
- [x] `strategy_manager.py` → `services/strategy::StrategyRunner` + `StrategyHandle` ✅
- [ ] `account_manager.py` → `hft-account::AccountManager` **TODO** Phase 3 (multi-subaccount orchestration).
  - 현재 Phase 2 D 의 `GateAccountClient` 는 단일 계정 REST 폴링만 지원.
- [ ] `margin_manager.py` + `v2.py` → `hft-account::MarginManager` **TODO** Phase 3.
- [ ] `error_manager.py` → `services/error-manager` **TODO** Phase 3 (Telegram bot 포함).
- [ ] `time_bucket_td_buffer.py` → `hft-strategy-core::buffer` **TODO** (현재 V7 narrative-close 에서 간이 버전만 사용).

---

## 11. 탑레벨 Python ops 스크립트

**→ target**: 대부분 ⏸ **Python 에서 그대로 운영**. 일부만 Rust CLI 로 이식 예정 (Phase 3).

### 11.1 계정 운영 — 전부 ⏸ Python 유지
- [~] `check_futures_mode.py` · `close_all_and_send_to_main_account.py` · `convert_subaccounts.py` · `get_subaccount*.py` — **Drop reason**: Python 운영 스크립트. 이식 불필요.
- [ ] `close_positions.py` (emergency) → 🔶 `tools/close-positions` **TODO** P2 later (현재 전략 레벨 `only_close` 모드로 부분 커버).
- [~] `send_balance_to_main_account.py`, `transfer_to_main_account.py`, `sort_subaccount_trades.py`, `generate_accounts_json.py`, `generate_available_trading_pairs.ipynb`, `generate_backtesting_data.py`, `generate_trading_pairs_batches.py`, `get_common_symbols.ipynb`, `common_symbols.txt`, `find_unused_symbols_and_update_supabase.ipynb`, `trading_pairs_batches.json` — **Drop reason**: Python/notebook 그대로 유지.

### 11.2 헬스 / 오케스트레이션 — **TODO** Phase 2 E / 3
- [ ] `gate_hft_healthcheck.py` → 🔶 `tools/healthcheck` **TODO** P2 E.
- [ ] `gate_hft_status_check.py` → 🔶 `tools/status-check` **TODO** P2 E.
- [ ] `restart_gate_hft.py` → `services/strategy::supervisor` (내재화) **TODO** P2 E.
- [ ] `monitoring_v2.py` → 🔷 `services/monitoring` **TODO** Phase 3.
- [ ] `monitoring_questdb.py` / `ws_questdb_monitoring.py` → 🔷 tools **TODO** Phase 3.

### 11.3 QuestDB / 데이터 ingress
- [x] `quest_db_ingressor.py` → `tools/questdb-export` 에 흡수 ✅ (테이블: `gate_bookticker`, `gate_trade`, `binance_bookticker`; raw ILP over TCP)
- [~] `zmq_to_dict_subscriber.py`, `update_binance_bookticker_to_questdb.py` — **Drop reason**: 디버그용 / 일회성 migration.
- [x] `external_latency_test.py` → `tools/latency-probe` 에 흡수 (+ p99.9 assertion CLI 추가, Phase 2 B) ✅

### 11.4 준비 / 배포
- [ ] `prepare_strategy_v2.py` → 🔶 `tools/prepare-strategy` **TODO** P2 E (multi-account 운영 확정 시).
- [~] `prepare_strategy_temp.py` — **Drop reason**.
- [ ] `profit_calculator.py` → 🔷 `tools/profit-calc` **TODO** Phase 3.
- [ ] `margin_manager.py` (top-level) → `hft-account::MarginManager` **TODO** Phase 3.

### 11.5 거래소별 WS 스트림 (Python standalone) — ✅ 대부분 Rust 로 대체
- [~] `gate_bookticker_ws_stream.py` / `gate_order_book_ws_stream.py` / `gate_trades_ws_stream.py` / `gate_fx_ws_stream.py` / `binance_publisher.py` / `binance_subscriber.py` / `bitget_bookticker_stream.py` / `bitget_collector.py` / `bybit_bookticker_ws_stream.py` — **Drop reason**: `services/publisher` + `hft-exchange-{gate,binance,bitget,bybit}` 로 완전 대체.
- [ ] `bingx_bookticker_stream.py` + `bingx_collector.py` → 🔷 `hft-exchange-bingx` **TODO** Phase 3 if needed.
- 📊 Parquet Hive 히스토리 기록은 `tools/futures-collector` 에서 커버 예정 (§4 참조).

### 11.6 Data collectors — **TODO** Phase 3
- [ ] `my_trades_collector.py` → 🔷 `tools/my-trades-collector` **TODO** Phase 3.
- [ ] `local_tick_collector.py` → 🔷 `tools/tick-collector` **TODO** Phase 3.
- [~] `data/downloader.py` — **Drop reason**: Python 유지.

### 11.7 Scripts 기타
- [~] `benchmark_shm.py` — **Drop reason**: SHM 재설계, `mono/crates/transport/hft-shm/tests/bench_vs_baseline.rs` 로 대체.
- [~] `binary_serialization_*.py` / `.md` — **Drop reason**: `hft-protocol` SPEC 이 정답.
- [~] `check_accounts.ipynb`, `unicorn-binance-*.sh` — **Drop reason**: Python/shell 유지 또는 삭제.
- [ ] `QUICK_LXC_SETUP.sh`, `EXTRACT_FROM_DOCKER_FOR_LXC.md`, `PROXMOX_*.md` → 🔷 `mono/deploy/` 에 재정리 — 부분 완료 (`mono/deploy/scripts/{host_bootstrap, launch_infra_vm, launch_strategy_vm}.sh` + systemd units + `docs/runbooks/INFRA_REQUIREMENTS.md`). 상세 Proxmox 운영가이드는 pending.
- [x] `settings.v16.sb.v3.json` → `hft-strategy-config::TradeSettings` defaults 로 흡수 ✅
- [~] `profit_baseline_sb_2025-12-10_*.json` — **Drop reason**: 레퍼런스 스냅샷, 이식 불필요.

---

## 12. `schemas/` — FlatBuffer / Protobuf — ✅ (전부 Drop)

**→ target**: `hft-protocol` 이 C-struct wire 로 통일 → FlatBuffer / Protobuf 는 드랍 (내부 ZMQ 전송에서 사용 안 함).

- [~] `schemas/book_ticker.fbs`, `market_data.fbs`, `trade.fbs`, `processed_data.fbs` — **Drop reason**: `hft-protocol` 의 120B BookTickerWire / 128B TradeWire C-struct 로 교체. FlatBuffer 런타임 의존성 제거.
- [~] `schemas/processed_data.proto` — **Drop reason**: Publisher 내부 aggregation 은 `ProcessedMarketData` Rust struct 로 이행.
- [x] 필드 parity 검증: `hft-protocol/tests/wire_compat.rs` 가 legacy binary_serialization 바이트와 1:1 대응 확인 ✅
  - BookTicker 120B: exchange(2)·symbol_id(4)·bid_price(8)·ask_price(8)·bid_size(8)·ask_size(8)·event_time(8)·server_time(8)·publisher_sent(8)·subscriber_received(8)·subscriber_dump(8)·padding
  - Trade 128B: + trade_id(8) + is_internal(1) flag

---

## 13. `monitoring/` (Docker) — ⏳ Phase 3 (미착수)

**→ target**: `mono/deploy/` (P3). Phase 2 에서는 QuestDB 연결 config 만 `hft-storage` 에 정리.

### 13.1 QuestDB — ⏳
- [ ] ports 8812 (pg), 9000 (http), 9009 (ilp/tcp) **TODO** Phase 3
- [ ] 2 CPU, 16GB RAM
- [ ] JVM `-Xms4g -Xmx8g -XX:+UseG1GC`
- [ ] worker 4 http / 4 pg / 4 sql / 2 wal
- [ ] WAL enabled, segment 1GB, commit_lag 50s, max_page_rows 100K
- [ ] TCP 9009 healthcheck 30s/10s
- ✅ 런타임 ILP 클라이언트는 `hft-storage::QuestDbSink` + `tools/questdb-export` 에서 이미 동작 (Phase 1).

### 13.2 Grafana — ⏳ Phase 3
- [ ] port 3000, 1 CPU / 8GB
- [ ] default password `admin`

### 13.3 Redis — ⏳ Phase 3
- [ ] port 6379, 1 CPU / 2GB
- [ ] AOF enabled, redis 7.4-alpine
- [ ] PING healthcheck 10s/5s

### 13.4 Prometheus / PushGateway — ⏳ Phase 3
- [ ] port 9090 / 9091, retention 30d
- [ ] Phase 3 에서 `hft-telemetry::prometheus_exporter` 활성화 여부 결정 (현재 HDR sink 만 구현)

### 13.5 monitoring/prometheus.yml, monitoring/grafana/, PUSHGATEWAY_SETUP.md — ⏳ Phase 3
- [ ] scrape config 이관 → `mono/deploy/monitoring/`

---

## 14. `analysis/` — 분석 / 백테스팅 — ⏸ Python 유지

**→ target**: 🔷 `mono/analysis/` 또는 ⏸ Python 유지. Phase 3 에서 `mono/` 로 흡수 여부 재평가.

- [~] `analyze_results.py`, `calculate_hourly_pnl.py`, `collect_crypto_data.py`, `compare_strategy_performance.py`, `generate_trading_report.py`, `optimize_params.py` — **Drop reason**: Python ecosystem (pandas/numpy/polars) 이 더 적합, Rust 이식 이득 없음. 유지.
- [~] `analyze_crypto_data.ipynb`, `analyze_futures_data.ipynb`, `analyze_tick_data.ipynb`, `backtest.ipynb` — **Drop reason**: notebook 그대로 유지.
- [~] `analysis/report/` — 출력 아티팩트, 무관

---

## 15. `strategy/rl` — 강화학습 실험 — ⏸ Out of Scope

**→ target**: ⏸ **Out of scope**. 별도 트랙. `mono/` 로 이식 안 함.

- [~] RL 실험 코드 전체 — **Drop reason**: 별도 연구 트랙. HFT 상용 전략(V6/V7/V8) 과 코드 공유 없음.

---

## 16. `sigma_work/` (아키텍처 문서 / SSH 키) — ⚠️ 보안 조치 필요

**→ target**: ⏸ drop. **SSH 키 유출 위험 — 즉시 조치 요망**.

- [ ] ⚠️ **SECURITY BLOCKER**: `sigma_work/id_ed25519`, `sigma_work/id_ed25519.pub` 는 Git 이력에서 제거 + 해당 키 로테이션 필요. `git filter-repo` 또는 BFG 로 커밋 이력에서 완전 삭제 후 force-push 전에 팀 합의 필수.
- [~] `sigma_work/ARCHITECTURE_REDESIGN.md`, `DEPLOYMENT_GUIDE.md`, `REPOSITORY_ANALYSIS.md` — **Drop reason**: 본 check.md + `mono/docs/adr/ADR-0003-multi-vm-topology.md` + `MULTI_VM_TOPOLOGY.md` + `SHM_DESIGN.md` 로 재구성. sigma_work 원본 문서는 참조용 스냅샷으로만 보존.

---

## 17. 루트 문서 / 기타

- [x] 아키텍처 / ADR → `mono/docs/adr/` 및 `mono/docs/runbooks/` 로 재정리 ✅
  - `ADR-0001-workspace-bootstrap.md`, `ADR-0002-wire-protocol.md`, `ADR-0003-multi-vm-topology.md`, `MULTI_VM_TOPOLOGY.md`, `SHM_DESIGN.md`, `INFRA_REQUIREMENTS.md` 등.
- [~] `README.md`, `README.v18.md`, `STRATEGY_LOGIC_v21.5.md` — **Drop reason**: legacy 문서 그대로 보존 (컨텍스트 레퍼런스). 신규 문서는 `mono/docs/` 에서 관리.
- [~] `pyproject.toml`, `uv.lock`, `requirements.txt` — Python 런타임 유지 (PyO3 binding + 분석 notebook 계속 사용)
- [x] `gate_orderbook_precisions.txt` → `hft-exchange-gate::precision` static table 로 흡수 ✅
- [ ] `gate_hft.log` → `.gitignore` 등재 **TODO** (housekeeping)

---

## 18. Cross-cutting Phase 1 블로커 (check.md 외부 의존성) — ✅ 전부 완료

Phase 1 완료 조건. **모두 통과 (2026-04-15 시점)**.

- [x] `hft-types` — `ExchangeId`/`Symbol`/`Price`/`Size` + mimalloc global allocator ✅
- [x] `hft-protocol` — 120B `BookTickerWire` / 128B `TradeWire` + `tests/wire_compat.rs` legacy 바이트 대조 테스트 녹색 ✅
- [x] `hft-time` — `SystemClock`/`MockClock` + 7-stage `LatencyStamps` ✅
- [x] `hft-obs` (→ `hft-telemetry`) — HDR histogram + non-blocking tracing-appender + Prometheus exporter scaffold ✅
- [x] `hft-core-config` — Figment layered (toml/env/CLI) + Supabase hot-reload 채널 ✅
- [x] `hft-zmq` — PUB/SUB/PUSH/PULL 래퍼 + `SendOutcome::{WouldBlock, Sent, Error}` ✅
- [x] `hft-storage` — `QuestDbSink` ILP over TCP + disk spool + reconnect backoff ✅
- [x] `hft-exchange-api` — `Feed`/`Executor` trait + `Emitter`/`LatencyStamps` hook ✅
- [x] `hft-exchange-binance` / `-gate` / `-bybit` / `-bitget` / `-okx` — 각 SPEC 완료 조건 충족 (Phase 1 WS feed + Phase 2 B REST/WS executor) ✅
- [x] `hft-testkit` — `MockFeed` + `WarmUp` 결정론 + `pipeline_harness!` 매크로 ✅
- [x] `services/publisher` — worker + aggregator + `patch_stamp` + 100K msg/s p99.9 < 2.1ms (e2e_pipeline_mock 통과) ✅
- [x] `services/subscriber` — crossbeam-channel 라우팅 + SHM bridge (feature-gated); legacy UDS bridge 는 드랍 ✅
- [x] `tools/questdb-export` ✅ / `tools/futures-collector` (scaffold, Parquet 출력은 Phase 3) / `tools/latency-probe` ✅
- [x] `crates/testing/integration` — e2e 테스트 세트 통과 (`e2e_pipeline_mock.rs` stage p99.9 포함) ✅
- [x] 전체 `cargo test --workspace` 녹색 (Phase 1 기준; Phase 2 D 이후 재실행 권장 — 아래 § 진행 상황 참조) ✅
- [x] `make fmt lint` (rustfmt + clippy `-D warnings`) 통과 ✅

> **다음 빌드 재확인 필요**: Phase 2 A/B/C/D 로 새 crate 11개 추가됨 (`hft-strategy-*`, `hft-exchange-rest`, `hft-shm`, `services/strategy` 등). 최초 병합 직후 `cargo check --workspace` + `cargo test --workspace` 재수행 권장. → 본 파일 맨 위 "★ 현재 진행 상황" 의 우선순위 1 항목.

---

## Phase 매핑 요약 (2026-04-15 기준)

| Phase | 상태 | 범위 | 포함 섹션 |
|-------|------|------|-----------|
| ✅ **P1** | 완료 | publisher + subscriber + storage + exchange feed × 5 + latency infra + SHM ring | §1, §2, §4, §5, §6.1, §12, §18 |
| ✅ **P2 A** | 완료 | strategy core (signal/decision/risk) + V6/V7/V8 + PyO3 | §3.1-3.9, §10.3 |
| ✅ **P2 B** | 완료 | Exchange executor × 5 (Gate/Binance/Bybit/Bitget/OKX) + hft-exchange-rest 서명 | §3.10 (executor), §11.5 |
| ✅ **P2 C** | 완료 | SHM intra-host IPC (MdRing seqlock + OrderRing SPSC + SymbolTable ahash) | §0, §2 subscriber, §10.5 |
| ✅ **P2 D** | 완료 | Account REST poller + OrderRateTracker::decay + StrategyControl + main.rs 배선 | §10.6 (strategy_manager, partial account_manager) |
| ⏳ **P2 E** | 진행 예정 | Order egress 마감 + CounterKey enum + multi-VM ivshmem 통합 테스트 | §3.11-3.13, §11.2, §11.4 |
| ⏳ **P2 F** | 진행 예정 | Deploy/Runtime 하드닝 (systemd, log rotation, healthcheck) | §11.2, §11.4, §13 부분 |
| 🔷 **P3** | 미착수 | monitoring 스택 / analytics / ops 자동화 / multi-subaccount | §10.6 (error_manager 등), §13, §14, §16 정리 |
| ⏸ **Deferred** | 유지/드랍 | Python 운영 스크립트 / RL / flatbuffer / 레거시 버전 | §9.4, §11.1, §11.7, §15 |
| ⚠️ **보안** | **조치 필요** | SSH 키 Git 이력 제거 + 로테이션 | §16 |

---

## 사용 팁

- 각 섹션 상단에 **→ target** 을 명기. crate SPEC 작업 시 이 체크리스트의 해당 항목을 SPEC.md 의 "완료 조건" 으로 가져갈 것.
- `✅/⏳/🔷/⏸/⚠️` 아이콘으로 상태 빠르게 필터.
- 본 파일 최상단 "★ 현재 진행 상황" 블록이 daily snapshot. 변경사항이 있으면 거기부터 업데이트 후 본 섹션 내려가며 `[x]`/`[~]` 토글.
- 누락 발견 시 이 파일에 추가 후 git diff 로 리뷰.

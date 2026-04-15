# Phase 2 B — Strategy runtime + v6/v7/v8 variant 통합 포트

작성일: 2026-04-15
범위: hft-strategy-runtime crate 신설 + v6/v7/v8 strategy 본체 변환.

## 0. 한 줄 요약

레거시 `gate_hft_rust::strategies::{v6,v7,v8}` 를 새 워크스페이스로 옮기되,
계정/포지션/주문률 상태를 trait 화 (`PositionOracle` + `OrderRateTracker`) 하고
변형별 의사결정 로직은 `hft-strategy-core::decision` 의 순수 함수
(`decide_order_v6` / `decide_order_v7` / `decide_order_v8`) 로 분리.

## 1. 신설 / 확장된 크레이트와 모듈

### 1.1 `hft-strategy-runtime` (신설)
경로: `crates/strategy/hft-strategy-runtime/`

| 모듈 | 책임 |
|------|------|
| `symbol_meta` | DashMap<Symbol, ContractMeta> — risk_limit / min_order_size / quanto_multiplier |
| `positions` | ArcSwap<PositionSnapshot> — 계정 long/short/심볼별 노출 + update_time |
| `last_orders` | DashMap<Symbol, LastOrder> — handle_chance 시간 제한용 |
| `order_rate` | OrderRateTracker — recent_orders ring buffer + per-symbol counter |
| `oracle` | PositionOracleImpl — 위 4개 캐시를 PositionOracle trait 으로 노출 |

#### 1.1.1 `OrderRateTracker` 의 zero-cost 설계
- ring buffer (`VecDeque<Entry>`) + per-symbol count (`AHashMap<u64, u32>`) 동시 갱신.
- `is_too_many_orders(now, gap)` → 가장 오래된 entry 의 timestamp 와 비교, O(1).
- `is_symbol_too_many_orders(sym, now, threshold, gap)` → per-symbol count 만 확인, O(1).
- `is_recently_too_many_orders(now)` → AtomicI64 한 번 read.
- 심볼 hash 는 `OnceLock<ahash::RandomState>` 로 process-global 시드 고정 →
  같은 심볼은 항상 같은 u64 키.

#### 1.1.2 `PositionSnapshot` 의 schema 변경
이전: `by_symbol_usdt: AHashMap<Symbol, f64>` (notional 만)
이후: `by_symbol: AHashMap<Symbol, SymbolPosition>` (notional + update_time_sec)

이유: v6/v8 의 `close_stale` 분기는 update_time_sec 가 필요한데 이전 schema 는
notional 만 전달했음. SymbolPosition 도입으로 단일 lookup 으로 둘 다 얻음.

### 1.2 `hft-strategy-core::risk::ExposureSnapshot` 확장
새 필드 2개 추가:
- `position_update_time_sec: i64` — close_stale 판단에 사용.
- `quanto_multiplier: f64` — close_stale_minutes_size × multiplier 환산에 사용.

### 1.3 `hft-strategy-core::signal` 추가
- `calculate_signal_v6` (NEW) — v0 결과 + side 가 비어있을 때 mid 비교 fallback.
  레거시 `strategies::v6::calculate_signal` 를 1:1 이식. binance fallback 의
  `lhs = gate_ask + gate_bid` (mid 가 아님) 비대칭은 byte-exact parity 원칙에
  따라 그대로 보존. 주석으로 표시.

### 1.4 `hft-strategy-core::decision` 확장
- `decide_order_v6` + `V6DecisionCtx` (NEW) — close_stale + size_threshold (1x/2x/4x)
  + is_order_net_position_close_side. v8 와의 차이: `is_recently_too_many_orders`
  부스트 없음.
- `decide_order_v7` + `V7DecisionExtras` (NEW) — close_stale 없음, size_threshold
  multiplier 없음. is_order_net_position_close_side 시 size *2 + narrative-close
  분기에서 only_close 양분기.
- (기존) `decide_order_v8` + `V8DecisionCtx` — Phase 2 A 트랙에서 도입 완료.

#### 1.4.1 변형별 cheat sheet

| 항목 | v0 | v6 | v7 | v8 |
|------|----|----|----|----|
| signal fallback | × | ○ (mid 비교) | × | × |
| last_trade 가격 보정 | × | × | × | ○ |
| close_stale | × | ○ | × | ○ |
| size_threshold_mult (too_many_orders) | × | ○ (1x→2x→4x) | × | ○ (×2 더) |
| is_order_net_position_close_side size *2 | × | ○ | ○ | × (운용 정책상 미사용) |
| narrative-close 분기 only_close 양분기 | × | × | ○ | × |

### 1.5 `crates/services/strategy` 모듈 추가
- `src/v6.rs` (NEW) — `V6Strategy` impl. v6 신호/결정/리스크 + `set_account_net_position`.
- `src/v7.rs` (NEW) — `V7Strategy` impl. rate tracker는 옵션 (with_rate).
- `src/v8.rs` (확장) — `with_runtime(oracle, rate)` 주입 + `evaluate_symbol` 이
  실제 `OrderRequest` 발행하도록 완성.

## 2. 데이터 흐름 (V8 기준, V6/V7 동일 골격)

```
MarketEvent
  ↓ ingest()              ── SymbolCache 업데이트 (alloc 0 path)
  ↓
calculate_signal_v8       ── 순수, alloc 0
  ↓
oracle.snapshot(sym)      ── PositionOracleImpl: 4개 캐시 lookup
  ↓
build_v8_ctx              ── OrderRateTracker 에서 too_many 플래그 pull
  ↓
decide_order_v8           ── 순수, close_stale → MarketClose 또는 limit_*
  ↓
handle_chance             ── 노출/min_size/시간 제약/잔고 검사
  ↓
OrderRequest 생성         ── client_id = "v8-{seq}", AtomicU64
  ↓
rate.push(sym, now_ms)    ── 다음 too_many 판정용 self-feedback
```

## 3. 테스트

**core crate (decision.rs)**
- v0/v8 기존 11 개 + v6 4 개 + v7 4 개 = 19 개.

**core crate (signal.rs)**
- v0/v8 기존 7 개 + v6 4 개 = 11 개.

**runtime crate**
- order_rate 7 개, positions 1 개 (snapshot), oracle 4 개, last_orders 2 개,
  symbol_meta 2 개 = 16 개.

**services/strategy**
- v8 4 개 (label / non-strategy 스킵 / dummy oracle reject / close_stale 통합) +
  v6 4 개 (동일) + v7 3 개 = 11 개.

전체 ~57 개 신규/이식 테스트. 로컬 toolchain 부재 — syntax 레벨 검증만 완료.

## 4. 후속 작업 (Phase 2 C/D)

| 트랙 | 항목 | 우선순위 | 상태 |
|------|------|---------|------|
| C | PyO3 binding `hft-strategy-shm-py` (Python ctypes-compatible) | high | ✅ 완료 (본 문서 §7 참고) |
| C | mmap SIGBUS 가드 (segfault → graceful error) | high | ✅ 완료 (`hft-shm/src/mmap.rs::check_fd_size_covers`) |
| D | Gate REST 계정 폴러 → SymbolMetaCache + PositionCache 실데이터 | high | ✅ 완료 (`hft-exchange-gate/src/account.rs` — §8.1) |
| D | services/strategy/main.rs 에 V6/V7/V8Strategy::with_runtime 와이어업 | high | ✅ 완료 (변형 CLI + `StrategyControl` 경로 — §8.3) |
| D | Phase B §1.6 publisher 심볼 인터섹션 + p99.9 latency assertion | medium | ✅ 완료 (`publisher/src/lib.rs::resolve_subscribed_symbols` + `latency-probe --target-*-p999-*`) |
| D | OrderRateTracker `clear()` 운영 시점 정의 (현재는 unused — 테스트만) | low | ✅ 완료 (`decay_before` / `decay_before_now` 추가 — §8.2) |

## 7. `hft-strategy-shm-py` — PyO3 바인딩

경로: `crates/transport/hft-strategy-shm-py/`

### 7.1 구성
- `src/lib.rs` — PyO3 `_core` 모듈. 4개 pyclass (`PyStrategyClient`, `PyOrderBuilder`,
  `PyQuoteSnapshot`, `PyTradeFrame`) + 상수/헬퍼.
- `python/hft_strategy_shm/__init__.py` — re-export + import 타이밍 wire
  불변식 assertion (`ORDER_FRAME_SIZE == 128`, `ALIGN == 64`).
- `python/hft_strategy_shm/_core.pyi` — 전체 타입 스텁.
- `python/hft_strategy_shm/py.typed` — PEP 561 marker.
- `pyproject.toml` — maturin backend, module-name `hft_strategy_shm._core`.
- `tests/test_roundtrip.py` — builder / wire-size / exchange-helper 테스트.

### 7.2 공개 API
```
shm.PyStrategyClient.open_dev_shm(path, *, spec..., vm_id) -> client
shm.PyStrategyClient.open_hugetlbfs(...) / open_pci_bar(...)
shm.PyStrategyClient.open_dev_shm_with_retry(...)   # GIL release on sleep

client.vm_id / .heartbeat_age_ns()
client.intern_symbol(exchange, symbol) -> idx
client.read_quote(idx) -> PyQuoteSnapshot | None
client.try_consume_trade() -> PyTradeFrame | None
client.publish_order(builder) -> bool
client.publish_simple(exchange, symbol, *, kind, side, tif, ord_type, ...)

shm.PyOrderBuilder()   # 재사용용 hot-path 홀더
shm.wall_clock_ns()

# 상수
shm.SIDE_BUY / SIDE_SELL
shm.TIF_GTC / TIF_IOC / TIF_FOK / TIF_POSTONLY / TIF_DAY
shm.ORD_TYPE_LIMIT / ORD_TYPE_MARKET
shm.ORDER_KIND_PLACE / ORDER_KIND_CANCEL
shm.ORDER_FRAME_SIZE == 128, ORDER_FRAME_ALIGN == 64
```

### 7.3 성능 및 안전 노트
- `#[pyclass(unsendable)]` — 단일 파이썬 스레드 전제 (strategy main loop).
- hot path 3종 (publish_order/read_quote/try_consume_trade) 은 GIL 유지 —
  전환 비용 > 본 작업 (nanosec scale).
- `open_dev_shm_with_retry` 만 `allow_threads` 로 GIL 릴리즈. publisher boot race
  대비 재시도 루프가 길 수 있음.
- `PyOrderBuilder::into_frame` 은 `#[repr(C, align(64))] OrderFrame` 을 in-place
  구성 → Arc/Box alloc 0.
- SIGBUS guard (`check_fd_size_covers`) 는 attach 경로 전원 활성 — mmap 전에
  fd 크기가 spec 보다 작으면 즉시 `RuntimeError`. Python 쪽에서 `try/except`
  처리 가능.

### 7.4 테스트
- Rust: 5 개 (builder default/into_frame/parse_exchange OK·reject/snapshot roundtrip/wire const).
- Python: 6 개 (pytest — maturin develop 후 실행). 실 SharedRegion 부착 테스트는
  Rust 쪽 `hft-strategy-shm/tests/end_to_end.rs` 가 담당.

## 5. 성능 / 안전 노트

- `OrderRateTracker` 의 hash 시드는 `OnceLock` 으로 process-global → 다른 인스턴스
  들 사이에서도 같은 심볼은 같은 u64 키. shm 등으로 호스트 간 공유는 못 함
  (개별 호스트마다 시드 다를 수 있음. 본 트래커는 in-process 전용).
- `AccountMembership` 은 `Arc<dyn Fn>` 으로 dynamic dispatch. hot path 1회 호출
  뿐이라 pessimization 무시 가능.
- `V6Strategy` / `V7Strategy` / `V8Strategy` 는 `client_seq: AtomicU64` 로 fmt 1회
  alloc + Arc<str> wrap. 한 자릿수 microsecond 이내.
- `DummyPositionOracle` 안전장치: `with_runtime` 호출 전엔 `is_account_symbol=false`
  → handle_chance 가 무조건 reject → 실 주문 0.

## 6. 호환성 / 마이그레이션

- 기존 `PositionSnapshot` 사용처 (=runtime 내부 + tests) 에서 `by_symbol_usdt` →
  `by_symbol` 으로 필드명 변경. 외부 노출 안 함.
- `ExposureSnapshot` 필드 추가는 backward-compatible (Default 가 0 / 1.0 로 채움).


## 8. Phase 2 D 구현 상세

본 섹션은 Phase 2 D (2026-04-15) 세 항목의 구현 결과를 문서화한다.

### 8.1 Gate REST 계정 폴러 (`hft-exchange-gate::account`)

경로: `crates/exchange/hft-exchange-gate/src/account.rs` (~1040L).

#### 컴포넌트
- **`GateAccountConfig`** — `rest_base` / `settle` (기본 `usdt`) / `timestamp_tolerance_s`.
- **`GateAccountClient`** — `reqwest::Client` 공유 (`hft_exchange_rest::RestClient`).
  - `fetch_contracts()` → `Vec<(Symbol, ContractMeta)>`
  - `fetch_contract(sym)` → 단일 refresh
  - `fetch_positions()` → `PositionSnapshot` (single/dual mode 흡수)
  - `fetch_accounts()` → `AccountBalance { total_usdt, unrealized_pnl_usdt, available_usdt }`
- **Trait impls**: `SymbolMetaProvider::fetch_all`, `PositionProvider::fetch`.
- **`AccountPollerBuilder`** — meta/positions/accounts 3개 target, 각 주기 (`Duration`) 개별 설정,
  `warm_start=true` 시 최초 tick 직전 동기 warmup 1회. `validate()` 는 target 최소 1개 +
  period>0 강제.
- **`PollerHandle`** — `JoinHandle<()>` + `CancellationToken` + `Arc<PollerStats>`.
  `Drop::drop` 에서 자동 cancel+abort → 호출자가 `shutdown().await` 잊어도 누수 0.
- **`PollerStats`** — atomic ok/err 카운터 6개 + 마지막 성공 timestamp 3개 → Prom/OTel export 원재료.
- **`BalanceSlot = ArcSwap<AccountBalance>`** — strategy 가 lock-free 로 구독.

#### 서명 / 파싱
- `GET /api/v4/futures/usdt/contracts` — public (no-sign).
- `GET /api/v4/futures/usdt/positions` / `accounts` — v4 signed (`executor::GateExecutor` 와 동일 regex: `METHOD\nPATH\nQUERY\nSHA512_HEX(body)\nTS_SEC`).
- serde `Visitor` 2종 (`de_f64_string_or_num` / `de_i64_string_or_num`) 로 Gate 의 string/number 혼재 필드 흡수 → String alloc 제거.
- Dual-mode 포지션: `mode == "dual_long" | "dual_short"` 두 row 를 symbol 단위로 **signed notional 합산**, `update_time_sec` 는 max 로 병합.

#### 테스트 (offline, 9개)
- contracts_parse_and_defaults / zero_defaults
- positions_single_mode_signs_via_size / dual_mode_accumulates_net / zero_notional_dropped / empty_snapshot
- accounts_parse_mixed_types
- builder_requires_at_least_one_target / builder_rejects_zero_period

### 8.2 `OrderRateTracker` time-based decay

경로: `crates/strategy/hft-strategy-runtime/src/order_rate.rs`.

추가 API:
- `fn oldest_ts() -> Option<i64>` — ring buffer 의 front timestamp. 호출자 fast-path 판정용.
- `fn decay_before(cutoff_ms: i64) -> usize` — cutoff 미만 엔트리 pop_front 반복 드롭 + `per_symbol` 카운터 lock-step 감소. 반환값은 drop 수.
- `fn decay_before_now(now_ms, window: Duration) -> usize` — `cutoff = now_ms - window` 편의 wrapper. `saturating_sub` 으로 underflow 방지.

운영 정책 (documented in module doc + 각 fn 의 rustdoc):
1. strategy eval 끝단에서 `decay_before(now - trade_settings.too_many_orders_time_gap_ms)` — 매 tick O(dropped).
2. 또는 heartbeat task 에서 `decay_before_now(now, 1h)` 주기 호출 — strategy idle 중에도 stale 정리.
3. `clear()` 는 recent flag 까지 리셋 → 운영 중 호출 금지 (장애 복구 / PID 교체 전용).

추가 테스트 6개 (drops/noop/empty/preserves flag/uses window/underflow guard).

### 8.3 `services/strategy/main.rs` full wiring

주요 변경:
- **`Variant` enum**: `noop`/`v6`/`v7`/`v8` — `HFT_STRATEGY_VARIANT` 환경변수로 선택.
- **`bring_up_full(cfg, variant, cancel)`** — v6/v7/v8 공통 스택 빌드:
  1. `StrategyConfig` 조립 (`HFT_STRATEGY_LOGIN_NAME`, `HFT_STRATEGY_SYMBOLS` 또는 `AppConfig.exchanges` 의 Gate primary 심볼).
  2. `SymbolMetaCache` / `PositionCache` / `LastOrderStore` / `OrderRateTracker` / `BalanceSlot` 생성.
  3. `AccountMembership::fixed(cfg.symbols)` + `PositionOracleImpl::new(...)` 조립.
  4. `maybe_spawn_gate_poller(...)` — `GATE_API_KEY`/`GATE_API_SECRET` 있으면 Gate REST poller spawn.
  5. `InprocQueue::bounded(cfg.zmq.hwm)` + `OrderSender::bounded(1024)`.
  6. `V?Strategy::new(...).with_runtime(oracle, rate)` + `strategy::start(...)` → `StrategyHandle`.
  7. `subscriber::start(cfg, Arc::new(queue))` → market events pipe.
  8. `spawn_balance_pump(BalanceSlot → StrategyControl::SetAccountBalance)` — epsilon-gated.
  9. `spawn_rate_decay(OrderRateTracker::decay_before_now)` — 주기적 time-window 정리.
  10. `spawn_order_drain(orders_rx)` — Phase 2 D 는 tracing 로 흡수 (Phase 2 E 에서 ZMQ PUSH 로 교체).
- **`StrategyControl` enum** (`services/strategy/src/lib.rs`) — hot-path 외 `on_control(&StrategyControl)` 훅.
  - 러너 루프 매 iteration 시작부에서 `try_recv` 16 회 drain (빈 채널 ~3ns).
  - 각 변형 (v6/v7/v8) 이 `on_control` 오버라이드하여 `set_account_balance` / `set_account_net_position` 으로 forward.
- **`StrategyHandle.control_tx`** — `crossbeam_channel::Sender<StrategyControl>` 노출. `push_control()` 편의 메서드.
- **shutdown 순서**: subscriber → strategy → aux cancel → poller → aux tasks join.

환경변수 모음:
- `HFT_STRATEGY_VARIANT` (`noop`|`v6`|`v7`|`v8`, default `noop`)
- `HFT_STRATEGY_LOGIN_NAME` / `HFT_STRATEGY_SYMBOLS`
- `GATE_API_KEY` / `GATE_API_SECRET` (없으면 poller skip — 전략은 계속 동작)
- `HFT_BALANCE_PUMP_MS` (default 500) / `HFT_RATE_DECAY_MS` (default 1000)

### 8.4 성능 / 안전 노트 (D)

- **Balance pump epsilon-gate**: `|prev - cur| > 1e-9` 일 때만 `StrategyControl` 전송 → 무변화 시 control 채널 트래픽 0.
- **Control drain 상한**: iter 당 16 건 → 계정 데이터 한꺼번에 쌓여도 hot path 시간 점유 상한선.
- **Poller 3 interval `MissedTickBehavior::Delay`**: REST 지연 후 burst catch-up 차단 → rate limit 침범 0.
- **`ArcSwap::load` (hot path)**: ~1-3ns, strategy eval 에 섞여도 noise 이하. 다만 pump 는 별 task 로 분리해 eval 이 load 를 직접 하지 않는 설계.
- **`PollerHandle` Drop-safe**: cancel + abort 중복 호출 OK (token 은 idempotent).

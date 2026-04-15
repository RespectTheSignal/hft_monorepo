# Phase 2 A 트랙 — 전략 엔진 포트 (foundation)

작성: 2026-04-15. 본 문서는 Phase 2 A 트랙 (전략 엔진 Rust 포트) 의 **첫 단계 —
shared foundation crates 구축** 에 대한 변경 기록이다. 레거시
`gate_hft_rust::{config, signal_calculator, order_decision, risk_manager,
handle_chance}` 를 `hft-strategy-{config,core}` 두 크레이트로 재조립했다.

## 1. 신규 크레이트

### 1.1 `crates/strategy/hft-strategy-config`

레거시 `src/config.rs` (528L) 포트 + 리팩토링.

- `TradeSettings` 50+ 필드 1:1 port. `#[serde(default)]` + `Default` impl 로
  중앙 관리. 누락 필드는 Default 값 자동 주입.
- `validate()` — min/max 역전, 비현실적 값 검출. 기존 레거시는 validation 없음.
- `StrategyConfig` — login_name + symbols + trade_settings + `AHashMap<String,()>`
  backing 으로 O(1) `is_strategy_symbol()`. 레거시는 `HashMap<String,bool>`.
- `SupabaseConfig::from_env()` — `SUPABASE_URL` / `SUPABASE_KEY`.
- `StrategyConfigLoader::refresh(login)` — **async** (reqwest::Client, 5s timeout,
  tcp_nodelay). 레거시는 `reqwest::blocking` 으로 tokio worker 를 점유했음.
- `ArcSwap<Option<StrategyConfig>>` 캐시 → Supabase 장애 시 마지막 성공값으로
  graceful fallback.
- `deserialize_text_id` 커스텀 visitor — id 가 int/string 어느 쪽이든 흡수.

테스트 6개:
- `default_trade_settings_validates` — 기본값이 validate 를 통과
- `validate_rejects_bad_tif`
- `validate_rejects_min_gt_max_time_restrictions`
- `strategy_config_membership_is_o1`
- `deserialize_text_id_accepts_both`
- `trade_settings_deserializes_with_defaults_filled`

### 1.2 `crates/strategy/hft-strategy-core`

레거시 `signal_calculator.rs`, `order_decision.rs`, `risk_manager.rs`,
`handle_chance.rs` 4개 파일 → `signal` / `decision` / `risk` 3 모듈로 재구성.

#### `signal` 모듈
- `BookTickerSnap` / `TradeSnap` — 시그널 계산에 필요한 **최소** 필드만. 레거시
  `BookTickerC` 는 exchange/symbol 을 고정-길이 바이트배열로 들고 있어 alignment
  가 깨졌다. 계산엔 price/size/time 만 있으면 되므로 잘라냈다.
- `calculate_signal` (v0) — 레거시와 시맨틱 1:1. `#[inline]` hot path. alloc 0.
- `calculate_signal_v8` — `gate_last_trade` 가 webbook 보다 새 것이면 대응 side
  가격을 trade 가격으로 override. 원본 구조체 불변, web snap 만 copy-then-mutate.
- `SignalResult.order_side` 를 `Option<String>` → `Option<&'static str>` 로 타이트화.

테스트 7개 (buy/sell 시그널, binance validity, v8 trade override, stale trade
ignore, degenerate book).

#### `decision` 모듈
- `OrderSide::{Buy,Sell}` / `OrderLevel::{LimitOpen, LimitClose, MarketClose}` enum.
  레거시의 `String` 비교 완전 제거.
- `decide_order` — 레거시 v0 `decide_order` 의 latency gate, funding_rate
  only_close, open/close 조건 분기 1:1 port. TIF 는 `normalize_tif` 가 unknown
  을 "fok" 로 fallback.

테스트 6개.

#### `risk` 모듈
레거시의 god struct `GateOrderManager` 를 [`PositionOracle`] trait 으로 추상화:

```rust
pub trait PositionOracle {
    fn snapshot(&self, symbol: &str) -> ExposureSnapshot;
    fn record_last_order(&self, symbol: &str, order: LastOrder);
}
```

`ExposureSnapshot` 은 handle_chance 가 필요로 하는 값을 한번에 담은 POD:
- `total_long_usdt`, `total_short_usdt` (계정 전체 노출)
- `this_symbol_usdt` (심볼별)
- `is_account_symbol`, `symbol_risk_limit`, `min_order_size`
- `last_order: Option<LastOrder>`

`handle_chance` 시그니처는 oracle 호출을 모두 스냅샷으로 받아 **순수 함수에
가까운** 형태가 됨 → unit test 가능성 극대화. 레거시는 handle_chance 내부에서
Gate 객체 여러 메서드를 매번 호출 (락 경합 + cache miss).

추가 개선:
- `TimeRestrictionJitter` — 레거시는 `rand::thread_rng().gen_range()` 를
  handle_chance 안에서 4~5회 호출. 매번 TLS 접근 + branch predictor pollution.
  본 crate 는 호출부에서 한번에 draw 해 `TimeRestrictionJitter` 로 주입.
  테스트는 `TimeRestrictionJitter::midpoint(ts)` 로 deterministic fallback.
- `RiskConfig { max_position_side_ratio, leverage }` — 레거시의 매직넘버 2종
  (handle_chance=1000, risk_manager=50) 을 명시 파라미터로. 환경별 실수 방지.

테스트 6개 (계정 심볼, 마지막 주문, close 방향, time restriction, approve 경로,
net exposure cap).

## 2. 수정 파일

- `Cargo.toml` — `crates/strategy/hft-strategy-{config,core}` workspace member
  + path dep 2개 추가.
- `crates/services/strategy/Cargo.toml` — 신규 의존성 4개 (hft-strategy-shm,
  hft-strategy-config, hft-strategy-core, parking_lot).
- `crates/services/strategy/src/lib.rs` — `pub mod v8;` 추가.
- `crates/services/strategy/src/v8.rs` (신규, 스캐폴드) — `V8Strategy: Strategy`
  impl. `MarketEvent::{BookTicker, WebBookTicker, Trade}` → `SymbolCache` 매핑
  (Gate/Binance/Gate-Web 구분), `calculate_signal_v8` → `decide_order` →
  `handle_chance` 까지 호출부 완성. 현재 `DummyPositionOracle` (`is_account_symbol
  = false` 고정) 을 사용해 **실 주문 발행은 차단**. 실 oracle 은 PositionOracle
  구현을 담은 별도 crate `hft-strategy-runtime` 에서 공급 예정 (Phase 2 후반).

## 3. 설계 결정 & 근거

### 3.1 왜 services/strategy 안에 V8 을 두고 별도 crate 는 안 만들었나
레거시는 variant(v6/v7/v8) 가 `strategies/` 서브 디렉토리 안에서 `StrategyCore`
trait 를 구현했다. Rust monorepo 에서는 variant 별로 crate 를 쪼개면 ① 컴파일
의존 그래프가 비대해지고 ② 공통 state (심볼 캐시, oracle, config) 를 공유하기
어려워진다. 본 포트는 **코어 로직 = 별도 crate, variant = services/strategy 내
모듈** 구조로 정리했다. 추후 variant 가 늘어나 v6/v7/v8 이 공통 Core + 차이만
overlay 되는 형태가 명확해지면 `hft-strategy-v8` crate 로 분리 검토.

### 3.2 왜 실 주문 oracle 은 아직 없나
`GateOrderManager` 는 2705 L 이고 다음을 포함한다:
- Gate REST 클라이언트 (포지션/잔고/주문 조회)
- 심볼 메타 캐시 (risk_limit, min_order_size, quanto_multiplier)
- 최근 주문 추적 (`last_orders: RwLock<HashMap>`)
- 실시간 포지션 업데이트 (PNL, trade_count, profit_bp_ema)

이 전체를 Rust 로 한번에 재작성하는 건 Phase 2 A 의 범위를 넘는다. 따라서 A 트랙의
foundation 은 **코어 판단 로직 + trait 경계** 까지만 커버하고, 실 oracle 은 별도
`hft-strategy-runtime` crate 에서 다음 순서로 구축한다:
1. `GateAccountClient` — REST + WebSocket (user stream) 으로 포지션/잔고 캐시
2. `SymbolMetaCache` — Gate contracts API 폴링 → DashMap<Symbol, ContractMeta>
3. `PositionOracleImpl` — 위 두 캐시 + local last_orders 로 PositionOracle 구현
4. `V8Strategy::with_oracle(oracle)` 로 주입 → 실 주문 on

### 3.3 레거시와 다른 시맨틱 (주의!)
- **시그널 side 가 v0 에서 binance_bt 없으면 reject**: 레거시 v0 와 동일. v8 은
  호출부에서 binance_data_is_updated 를 false 로 잡고 open 조건에 추가로 포함
  시켜야 한다 (본 포트에서는 services/strategy 내 v8.rs 가 담당할 영역).
- **handle_chance 가 min/max jitter 를 외부에서 받음**: 레거시는 내부에서
  `rand` 호출. 호출부가 반드시 `TimeRestrictionJitter` 를 생성해 전달해야 함.
  타임리스트릭션 로직 자체는 동일.

## 4. 남은 작업 (Phase 2 A 후속)

- [ ] `hft-strategy-runtime` crate 신설 (Gate REST 계정 + 심볼 메타 + oracle).
- [ ] V8Strategy 의 `evaluate_symbol` 에서 실제 `OrderRequest` 생성 & return.
- [ ] V8 variant 의 나머지 로직 (too_many_orders 보호, gap_dissolve_exit, stale
      position close) 을 v8.rs 로 내리기.
- [ ] v6/v7 variant 포트 (동일 패턴).
- [ ] Strategy 가 SHM 기반으로 돌도록 StrategyShmClient 통합 (현재 lib.rs 는
      crossbeam Receiver 기반 consume loop — SHM 전환 필요).

## 5. 성능 가드레일

- `calculate_signal_v8` 는 inline, alloc 0. f64 연산 및 `Option` 분기만.
- `OrderSide` / `OrderLevel` enum 은 1 byte, 비교는 discriminant 1 cmp.
- `TimeRestrictionJitter` 는 POD, Copy. 매 이벤트마다 caller 가 잠깐 draw.
- V8Strategy 의 `SymbolCache` 는 `HashMap<String, SymbolCache>`. 심볼 수가
  100 이하라 충분. 필요 시 `AHashMap<Symbol, SymbolCache>` 로 교체 검토.

## 6. 테스트

```
cargo test -p hft-strategy-config   # 6 tests
cargo test -p hft-strategy-core     # 19 tests (signal 7 + decision 6 + risk 6)
cargo test -p strategy              # +2 in v8 module
```

로컬 툴체인이 없어 모든 테스트는 syntax-level 검증만 완료. 첫 `cargo check
--workspace` 시도에서 걸리는 건 즉시 fix 가능한 수준으로 설계됨.

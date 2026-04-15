# hft-config — SPEC

## 역할
모든 서비스의 런타임 설정 로드. **hot path 에 들어가는 모든 튜닝 값은 이 crate 경유**.

## 공개 API

```rust
pub struct AppConfig {
    pub service_name: String,         // "publisher-gate", "subscriber-1" 등
    pub hot_workers: usize,
    pub bg_workers: usize,

    pub zmq: ZmqConfig,
    pub exchanges: Vec<ExchangeConfig>,
    pub questdb: QuestDbConfig,
    pub supabase: SupabaseConfig,
    pub telemetry: TelemetryConfig,
    pub warmup: WarmupConfig,
}

pub struct ZmqConfig {
    pub hwm: i32,                     // default 100_000
    pub linger_ms: i32,               // default 0
    pub drain_batch_cap: usize,       // default 512
    pub push_endpoint: String,        // "ipc:///tmp/hft-push" or "inproc://push"
    pub pub_endpoint: String,         // "tcp://0.0.0.0:5555"
    pub sub_endpoint: String,         // "tcp://127.0.0.1:5555"
}

pub struct ExchangeConfig {
    pub id: ExchangeId,
    pub symbols: Vec<Symbol>,
    pub role: DataRole,
    pub ws_url: Option<String>,       // None 이면 crate 기본값
    pub reconnect_backoff_ms: u64,    // default 500
    pub ping_interval_s: u64,         // default 15
}

pub struct QuestDbConfig { pub ilp_addr: String, pub batch_rows: usize, pub batch_ms: u64, pub spool_dir: PathBuf }
pub struct SupabaseConfig { pub url: String, pub anon_key: String /* or service_role via env only */ }
pub struct TelemetryConfig { pub otlp_endpoint: Option<String>, pub prom_port: Option<u16>, pub stdout_json: bool }
pub struct WarmupConfig { pub enabled: bool, pub events: usize }

pub fn load_all() -> anyhow::Result<Arc<AppConfig>>;
pub fn validate(cfg: &AppConfig) -> anyhow::Result<()>;
```

## 로드 우선순위

1. Rust defaults (struct 의 `impl Default`)
2. TOML 파일 (`configs/{service_name}.toml` 또는 `configs/default.toml`)
3. Supabase `configs` 테이블 (bg runtime 에서 fetch, 실패 시 무시하고 경고)
4. 환경변수 (`HFT_ZMQ_HWM`, `HFT_EXCHANGES__0__ID` 등 — figment convention)

env 가 최우선. 이렇게 하면 CI/Docker 에서 env 로 override 가능.

## 계약
- 반환은 `Arc<AppConfig>` — 서비스 전역에 공유. **mutating 불가**.
- runtime 도중 바뀌면 프로세스 재시작.
- `validate()` 가 panic 아닌 `Err` 를 리턴해야 함 (CI 테스트에서 config 파일 lint).
- supabase fetch 는 startup 에서 동기적으로 시도. timeout 5s. 실패 시 `tracing::warn!` 후 file+env+default 만으로 진행.

## Phase 1 TODO

1. `figment` crate 기반 로더 구현. TOML, env (`HFT_` prefix, `__` 를 nested 구분자로), Supabase Provider (custom Provider impl).
2. `impl Default for AppConfig` — 모든 필드 합리적 기본값.
3. `validate`:
   - hot_workers > 0
   - zmq.hwm > 0
   - drain_batch_cap > 0 && <= 8192
   - exchanges 가 비어있지 않음
   - 각 ExchangeConfig 의 symbols 비어있지 않음
4. `configs/default.toml` + `configs/publisher-gate.toml` 등 서비스별 샘플 파일 (`configs/` 폴더는 이미 있음).
5. supabase provider — `postgrest-rs` 로 `configs` 테이블 read. 인증은 env 의 `HFT_SUPABASE_SERVICE_KEY`.
6. 테스트: TOML + env override 섞어서 final `AppConfig` 가 예상대로 composed 되는지.

## Anti-jitter 체크
- `Arc<AppConfig>` deref 는 pointer 1회. hot path 영향 없음.
- runtime reload 금지 → `RwLock` 피함.
- supabase 호출은 startup 동기 1회만. hot path 에서 절대 호출 안 함.

## 완료 조건
- [ ] env 로만 최소 설정 기동 가능 (TOML/supabase 없이도 validate 통과하는 default 존재)
- [ ] `HFT_ZMQ__HWM=50000` 이 TOML 의 `zmq.hwm` 를 override 하는지 테스트
- [ ] `validate()` 가 nonzero exit code 로 끝나는 unit test

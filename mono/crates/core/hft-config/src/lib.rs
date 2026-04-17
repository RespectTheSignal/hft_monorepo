//! hft-config — 계층화 런타임 설정 로더.
//!
//! ## 설계
//! - 모든 서비스(publisher/subscriber/strategy/order-gateway)가 이 crate 를 통해
//!   tuning 값을 주입받는다. Hot path 의 상수도 예외 없이 경유한다.
//! - 반환은 `Arc<AppConfig>` — 멀티 스레드 공유, 런타임 변경 불가.
//!   변경이 필요하면 프로세스 재시작. `RwLock` 을 피해 jitter 를 없앤다.
//! - 레이어: **defaults → TOML → env**. env 가 최우선. CI/Docker 친화.
//! - Supabase 는 옵션 `refresh_from_supabase` 로 별도 async 호출.
//!   hot path 에선 절대 호출하지 않는다 (startup 한정).
//!
//! ## 계약
//! - [`load_all`] 은 panic 하지 않는다. TOML 경로가 없으면 defaults + env 로만 구성.
//! - [`validate`] 는 `Err(ConfigError)` 반환 — CI 테스트에서 config lint 가능.
//! - env prefix 는 `HFT_`, nested 구분자는 `__` (figment 관행).
//! - Symbol 은 내부 `Arc<str>` 이라 TOML/env 에서 문자열 한 개로 표현 가능.
//!
//! ## Phase 1 TODO
//! 1. ✅ figment 기반 defaults + TOML + env 로더
//! 2. ✅ `impl Default` (env 만으로 기동 가능)
//! 3. ✅ `validate()` (hot_workers>0, zmq.hwm>0, drain_batch_cap∈[1,8192], exchanges≠∅ 등)
//! 4. ✅ ExchangeConfig / QuestDbConfig / WarmupConfig / TelemetryConfig serde
//! 5. ⏸ Supabase 직접 연동 — `refresh_from_supabase` 스텁 제공, Phase 2 에서 실제 HTTP 구현
//!
//! ## Anti-jitter
//! - `Arc<AppConfig>` deref → 1 pointer 읽기, hot path 영향 없음.
//! - 어떠한 reload 경로도 hot path 에서 호출되지 않는다.

#![deny(rust_2018_idioms)]

pub mod order_egress;

use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

use hft_types::{DataRole, ExchangeId, Symbol};
pub use order_egress::OrderEgressConfig;

// ─────────────────────────────────────────────────────────────────────────────
// Error
// ─────────────────────────────────────────────────────────────────────────────

/// 설정 로드/검증 에러.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Figment composition 단계에서 발생한 에러 (TOML parse, env coerce 등).
    #[error("config load failed: {0}")]
    Load(Box<figment::Error>),

    /// I/O 에러 (파일 접근 실패 등).
    #[error("config IO error: {0}")]
    Io(#[from] std::io::Error),

    /// validate() 가 정책 위반을 감지한 경우.
    #[error("config validation failed: {0}")]
    Invalid(String),
}

/// 결과 타입 별칭.
pub type ConfigResult<T> = Result<T, ConfigError>;

impl From<figment::Error> for ConfigError {
    fn from(value: figment::Error) -> Self {
        Self::Load(Box::new(value))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AppConfig — 최상위 설정
// ─────────────────────────────────────────────────────────────────────────────

/// 모든 서비스가 공유하는 최상위 설정.
///
/// 필드 추가 시:
/// 1. `Default` 구현 갱신 (합리적 기본값 필수).
/// 2. `validate` 에 invariant 추가.
/// 3. 필요하면 `configs/{service}.toml` 샘플에도 반영.
///
/// Note: `deny_unknown_fields` 는 사용하지 않는다 — env (`HFT_LOG` 등) 가
/// 충돌 없이 통과할 수 있어야 하고, 서비스 확장 시 forward-compat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 서비스 식별자 (tracing 태그·로그에 사용).
    pub service_name: String,
    /// hot path 작업 스레드 수. Publisher 는 거래소 feed worker 수.
    pub hot_workers: usize,
    /// bg 스레드 수 (QuestDB write, supabase refresh 등).
    pub bg_workers: usize,

    /// ZMQ 전송 설정.
    #[serde(default)]
    pub zmq: ZmqConfig,
    /// 구독할 거래소·심볼 목록.
    #[serde(default)]
    pub exchanges: Vec<ExchangeConfig>,
    /// QuestDB ILP writer 설정.
    #[serde(default)]
    pub questdb: QuestDbConfig,
    /// Supabase runtime config 테이블 접근.
    #[serde(default)]
    pub supabase: SupabaseConfig,
    /// tracing / OTel / prometheus 설정.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// WarmUp 설정 (JIT / 캐시 워밍).
    #[serde(default)]
    pub warmup: WarmupConfig,
    /// SHM (shared memory) transport 설정 — intra-host sub-μs fast path.
    #[serde(default)]
    pub shm: ShmConfig,
    /// 주문 request egress 설정 — SHM 정상 경로 + ZMQ fallback 선택.
    #[serde(default)]
    pub order_egress: OrderEgressConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            service_name: "hft-service".into(),
            hot_workers: 1,
            bg_workers: 1,
            zmq: ZmqConfig::default(),
            exchanges: Vec::new(),
            questdb: QuestDbConfig::default(),
            supabase: SupabaseConfig::default(),
            telemetry: TelemetryConfig::default(),
            warmup: WarmupConfig::default(),
            shm: ShmConfig::default(),
            order_egress: OrderEgressConfig::default(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ZmqConfig
// ─────────────────────────────────────────────────────────────────────────────

/// ZeroMQ 소켓 공통 튜닝.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZmqConfig {
    /// High-water mark (socket send/recv buffer 용량, messages).
    /// default 100_000.
    pub hwm: i32,
    /// LINGER 옵션 (ms). 0 이면 close 즉시 drop, graceful shutdown 에 적합.
    pub linger_ms: i32,
    /// aggregator drain loop 의 1-tick 당 최대 drain 메시지 수.
    /// 1..=8192 범위를 벗어나면 validate 에서 reject.
    pub drain_batch_cap: usize,
    /// PUSH (publisher inproc aggregator) endpoint.
    pub push_endpoint: String,
    /// PUB (aggregator → external) endpoint.
    pub pub_endpoint: String,
    /// SUB (subscriber → aggregator) endpoint.
    pub sub_endpoint: String,
    /// order-gateway ZMQ ingress bind endpoint. `None` 이면 SHM-only.
    #[serde(default)]
    pub order_ingress_bind: Option<String>,
    /// gateway → strategy order result reverse path bind endpoint. `None` 이면 비활성.
    #[serde(default)]
    pub result_egress_bind: Option<String>,
    /// gateway → strategy heartbeat 주기 (ms). 기본 1000ms (1초).
    /// result_egress_bind 가 켜져 있을 때만 유효하다.
    #[serde(default = "default_result_heartbeat_interval_ms")]
    pub result_heartbeat_interval_ms: u64,
}

impl Default for ZmqConfig {
    fn default() -> Self {
        Self {
            hwm: 100_000,
            linger_ms: 0,
            drain_batch_cap: 512,
            push_endpoint: "inproc://hft-push".into(),
            pub_endpoint: "tcp://0.0.0.0:5555".into(),
            sub_endpoint: "tcp://127.0.0.1:5555".into(),
            order_ingress_bind: None,
            result_egress_bind: None,
            result_heartbeat_interval_ms: default_result_heartbeat_interval_ms(),
        }
    }
}

fn default_result_heartbeat_interval_ms() -> u64 {
    1000
}

// ─────────────────────────────────────────────────────────────────────────────
// ExchangeConfig
// ─────────────────────────────────────────────────────────────────────────────

/// 특정 거래소(+역할) 구독 설정.
///
/// 동일 거래소라도 `role` 이 다르면 별도 엔트리로 기술한다.
/// 예: Gate Primary (API bookticker) + Gate WebOrderBook 두 건.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeConfig {
    /// 거래소.
    pub id: ExchangeId,
    /// 구독 심볼 목록. 최소 1개 (validate).
    pub symbols: Vec<Symbol>,
    /// 채널 역할 (Primary / WebOrderBook).
    #[serde(default = "default_role")]
    pub role: DataRole,
    /// WS endpoint override. None 이면 거래소 crate 기본값 사용.
    #[serde(default)]
    pub ws_url: Option<String>,
    /// 재접속 backoff 시작값 (ms). 지수적 backoff 의 기준.
    #[serde(default = "default_reconnect_ms")]
    pub reconnect_backoff_ms: u64,
    /// keepalive ping 간격 (s).
    #[serde(default = "default_ping_s")]
    pub ping_interval_s: u64,
    /// 구독 시 제외할 심볼 목록 (블랙리스트). `symbols` 와 교집합 후 빠진다.
    ///
    /// 용도: 특정 심볼이 매매 정지/상장 폐지 등으로 publisher 가동 중 계속 에러를
    ///뱉는 경우, 전체 시스템 재배포 없이 이 목록만 갱신해 해당 심볼을 제외할 수
    /// 있게 한다. Phase 2 Track B §1.6 "publisher symbol intersection" 도입.
    #[serde(default)]
    pub ignore_symbols: Vec<Symbol>,
}

fn default_role() -> DataRole {
    DataRole::Primary
}
fn default_reconnect_ms() -> u64 {
    500
}
fn default_ping_s() -> u64 {
    15
}

// ─────────────────────────────────────────────────────────────────────────────
// QuestDbConfig
// ─────────────────────────────────────────────────────────────────────────────

/// QuestDB ILP writer 설정.
///
/// `spool_dir` 은 DB 연결 끊김 시 ILP 바이트를 임시 파일로 쌓는다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestDbConfig {
    /// ILP endpoint (host:port). 빈 문자열이면 writer 비활성.
    pub ilp_addr: String,
    /// 한 번에 flush 하는 최대 row 수.
    pub batch_rows: usize,
    /// flush interval 상한 (ms). batch_rows 에 미달해도 이 시간 지나면 flush.
    pub batch_ms: u64,
    /// 연결 끊김 시 임시 파일 디렉터리.
    pub spool_dir: PathBuf,
}

impl Default for QuestDbConfig {
    fn default() -> Self {
        Self {
            ilp_addr: String::new(),
            batch_rows: 1000,
            batch_ms: 500,
            spool_dir: PathBuf::from("./spool"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SupabaseConfig
// ─────────────────────────────────────────────────────────────────────────────

/// Supabase runtime config 테이블 접근.
///
/// 실제 service role 키는 env `HFT_SUPABASE_SERVICE_KEY` 로만 공급한다.
/// TOML 에는 절대 넣지 말 것.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupabaseConfig {
    /// Supabase project URL. 빈 문자열이면 비활성.
    #[serde(default)]
    pub url: String,
    /// Anon key (공개 가능). 쓰기 권한이 필요하면 service key 를 따로 사용.
    #[serde(default)]
    pub anon_key: String,
    /// Service role key. **TOML 에 저장 금지** — env `HFT_SUPABASE__SERVICE_KEY`
    /// 로만 공급한다 (nested 구분자는 `__`).
    /// Debug 출력에선 길이만 보이도록 별도 처리해야 한다 (TODO Phase 2).
    #[serde(default)]
    pub service_key: String,
    /// refresh 간격 (초). 기본 300s = 5분.
    #[serde(default = "default_supabase_refresh")]
    pub refresh_interval_s: u64,
    /// fetch timeout (초). startup 단계 hang 방지.
    #[serde(default = "default_supabase_timeout")]
    pub fetch_timeout_s: u64,
}

fn default_supabase_refresh() -> u64 {
    300
}
fn default_supabase_timeout() -> u64 {
    5
}

impl Default for SupabaseConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            anon_key: String::new(),
            service_key: String::new(),
            refresh_interval_s: default_supabase_refresh(),
            fetch_timeout_s: default_supabase_timeout(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TelemetryConfig
// ─────────────────────────────────────────────────────────────────────────────

/// tracing / OTel / Prometheus 라우팅.
///
/// 이 구조체는 hft-config 가 소유. 서비스는 이 값을 읽어
/// `hft_telemetry::TelemetryConfig` 를 생성해 init 한다 (1 단방향 매핑).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// OTLP gRPC endpoint. None / 빈 문자열이면 OTel 비활성.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    /// health + metrics HTTP endpoint port override.
    /// None 이면 서비스 기본 포트(예: strategy=9100, order-gateway=9101)를 사용한다.
    #[serde(default)]
    pub prom_port: Option<u16>,
    /// fmt layer 를 JSON 으로 출력. false 면 pretty.
    #[serde(default)]
    pub stdout_json: bool,
    /// 기본 log level (env `HFT_LOG` 이 있으면 그것이 우선).
    #[serde(default = "default_log_level")]
    pub default_level: String,
}

fn default_log_level() -> String {
    "info".into()
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: None,
            prom_port: None,
            stdout_json: false,
            default_level: default_log_level(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// WarmupConfig
// ─────────────────────────────────────────────────────────────────────────────

/// JIT / 캐시 워밍 설정.
///
/// publisher 가 live feed 전에 가짜 메시지를 N 개 pipeline 에 흘려 cold path 를 제거.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WarmupConfig {
    /// 워밍 활성 여부.
    pub enabled: bool,
    /// 워밍 단계에서 주입할 가짜 이벤트 수.
    pub events: usize,
}

impl Default for WarmupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            events: 5_000,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ShmConfig — intra-host shared memory fast path
// ─────────────────────────────────────────────────────────────────────────────

/// SHM (shared memory, `/dev/shm` tmpfs) 기반 IPC 설정.
///
/// ## 역할
/// publisher → subscriber → strategy → order-gateway 사이의 **intra-host** 데이터
/// path 를 ZMQ TCP (p50 ~15μs) 대신 mmap + atomic (p50 ~100ns) 으로 대체한다.
///
/// ## 파일 배치 (기본)
/// - `quote_path`  : `/dev/shm/hft_quotes_v2`  — 10,000 slot × 128B (+ header) ≈ 1.3MB
/// - `trade_path`  : `/dev/shm/hft_trades_v2`  — 2^20 frame × 128B (+ header) = 128MB
/// - `order_path`  : `/dev/shm/hft_orders_v2`  — 16,384 frame × 128B (+ header) ≈ 2MB
/// - `symtab_path` : `/dev/shm/hft_symtab_v2`  — 16,384 entry × 64B (+ header) ≈ 1MB
///
/// 디스크(tmpfs) 총 용량 ≈ 132.5MB — 32GB 메모리 장비 기준 무시 가능.
///
/// ## Capacity 규칙
/// - `trade_ring_capacity`, `order_ring_capacity` 는 **2의 거듭제곱** 이어야 한다
///   (ring mask 연산을 위해). `validate()` 에서 거부.
/// - `quote_slot_count` 는 운영 중 등록할 전체 symbol 수 상한. Exchanges × 10 여유.
/// - `symbol_table_capacity` 는 `quote_slot_count` 와 동일하게 두는 것이 안전.
///
/// ## 활성/비활성
/// `enabled=false` 이면 publisher/subscriber/strategy 는 SHM writer/reader 를 열지
/// 않고 legacy ZMQ/UDS 경로로 폴백한다. 로컬 개발/테스트 환경을 위해 제공.
///
/// ## v2 Multi-VM 레이어
/// [`backend`](ShmConfig::backend) + [`shared_path`](ShmConfig::shared_path) +
/// [`role`](ShmConfig::role) + [`vm_id`](ShmConfig::vm_id) 가 v2 필드.
/// backend ≠ `LegacyMultiFile` 이면 runtime 은 hft-shm `SharedRegion` 을 열고
/// 단일 파일 안에서 sub_region 을 마운트한다. backend = `LegacyMultiFile` 이면
/// 기존 경로 4개(`quote_path`/`trade_path`/`order_path`/`symtab_path`) 로 폴백.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShmConfig {
    /// SHM transport 활성화 여부. false 면 legacy ZMQ 경로만 사용.
    #[serde(default = "default_shm_enabled")]
    pub enabled: bool,

    // ── v2 multi-VM 필드 ───────────────────────────────────────────────
    /// Backend 선택. `DevShm` / `Hugetlbfs` / `PciBar` = 단일 shared file 모드,
    /// `LegacyMultiFile` = 기존 4-파일 모드 (backward compat).
    #[serde(default)]
    pub backend: ShmBackendKind,
    /// v2: 단일 shared 파일 경로 (backend=DevShm/Hugetlbfs/PciBar 에서만 사용).
    /// backend 가 LegacyMultiFile 이면 무시되고 아래 4개 경로가 사용된다.
    #[serde(default = "default_shared_path")]
    pub shared_path: PathBuf,
    /// 이 프로세스가 shared region 에서 수행할 역할.
    #[serde(default)]
    pub role: ShmRoleKind,
    /// `role=Strategy` 일 때의 vm_id (0..n_max). 그 외 역할에선 무시.
    #[serde(default)]
    pub vm_id: u32,
    /// order ring 슬롯 개수 (N_MAX) — strategy VM 최대 수. 2..=MAX_ORDER_RINGS.
    #[serde(default = "default_n_max")]
    pub n_max: u32,

    // ── Legacy 4-파일 필드 (backend=LegacyMultiFile 에서만 유효) ─────────
    /// BookTicker seqlock quote slot 파일 경로.
    #[serde(default = "default_quote_path")]
    pub quote_path: PathBuf,
    /// SPMC trade ring 파일 경로.
    #[serde(default = "default_trade_path")]
    pub trade_path: PathBuf,
    /// SPSC order ring 파일 경로 (Python strategy → Rust order-gateway).
    #[serde(default = "default_order_path")]
    pub order_path: PathBuf,
    /// Symbol table 파일 경로 — `(ExchangeId, symbol)` → `u32` 매핑.
    #[serde(default = "default_symtab_path")]
    pub symtab_path: PathBuf,

    // ── 용량 관련 필드 (모든 backend 공통) ───────────────────────────────
    /// quote slot 수. symbol 총 수 상한.
    #[serde(default = "default_quote_slot_count")]
    pub quote_slot_count: u32,
    /// trade ring capacity (power-of-two).
    #[serde(default = "default_trade_ring_capacity")]
    pub trade_ring_capacity: u64,
    /// order ring capacity (power-of-two). Python → Rust 주문 burst 수용량.
    #[serde(default = "default_order_ring_capacity")]
    pub order_ring_capacity: u64,
    /// symbol table capacity. quote_slot_count 이상 권장.
    #[serde(default = "default_symbol_table_capacity")]
    pub symbol_table_capacity: u32,
}

/// SHM backend 종류. 어느 *파일 시스템/디바이스* 위에 shared region 을 놓을지.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShmBackendKind {
    /// 기본. `/dev/shm` 하나의 파일에 모든 sub-region 을 담는다.
    #[default]
    DevShm,
    /// `hugetlbfs` 마운트된 경로. 2MB/1GB 페이지로 TLB miss 감소.
    Hugetlbfs,
    /// ivshmem PCI BAR 경로 (`/sys/bus/pci/devices/.../resource2`).
    /// guest-VM 안에서 동일 물리 영역을 공유.
    PciBar,
    /// 레거시 모드 — quote/trade/order/symtab 파일 4개 분리. v1 호환 경로.
    LegacyMultiFile,
}

/// Shared region 위에서 이 프로세스의 역할.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShmRoleKind {
    /// Publisher (market data) — quote_slot / trade_ring / symbol_table writer.
    #[default]
    Publisher,
    /// Order-Gateway — 모든 order_ring 의 reader 이자 ack 응답자.
    OrderGateway,
    /// Strategy VM — 자기 `vm_id` 의 order_ring writer, quote/trade reader.
    Strategy,
    /// 읽기 전용 관찰자 (모니터/디버거).
    ReadOnly,
}

fn default_shm_enabled() -> bool {
    true
}
fn default_quote_path() -> PathBuf {
    PathBuf::from("/dev/shm/hft_quotes_v2")
}
fn default_trade_path() -> PathBuf {
    PathBuf::from("/dev/shm/hft_trades_v2")
}
fn default_order_path() -> PathBuf {
    PathBuf::from("/dev/shm/hft_orders_v2")
}
fn default_symtab_path() -> PathBuf {
    PathBuf::from("/dev/shm/hft_symtab_v2")
}
fn default_quote_slot_count() -> u32 {
    10_000
}
fn default_trade_ring_capacity() -> u64 {
    1 << 20 // 1,048,576
}
fn default_order_ring_capacity() -> u64 {
    16_384
}
fn default_symbol_table_capacity() -> u32 {
    16_384
}
fn default_shared_path() -> PathBuf {
    PathBuf::from("/dev/shm/hft_shared_v2")
}
fn default_n_max() -> u32 {
    16
}

impl Default for ShmConfig {
    fn default() -> Self {
        Self {
            enabled: default_shm_enabled(),
            backend: ShmBackendKind::default(),
            shared_path: default_shared_path(),
            role: ShmRoleKind::default(),
            vm_id: 0,
            n_max: default_n_max(),
            quote_path: default_quote_path(),
            trade_path: default_trade_path(),
            order_path: default_order_path(),
            symtab_path: default_symtab_path(),
            quote_slot_count: default_quote_slot_count(),
            trade_ring_capacity: default_trade_ring_capacity(),
            order_ring_capacity: default_order_ring_capacity(),
            symbol_table_capacity: default_symbol_table_capacity(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 로더
// ─────────────────────────────────────────────────────────────────────────────

/// 기본 전체 로드: defaults → TOML(찾으면) → env.
///
/// - TOML 경로: `configs/{service_name}.toml` 또는 `configs/default.toml`.
///   `service_name` 은 env `HFT_SERVICE_NAME` 로 override 가능.
/// - env prefix: `HFT_`, nested 구분자: `__` (예: `HFT_ZMQ__HWM=50000`).
///
/// 성공 시 `Arc<AppConfig>`. panic 하지 않는다.
pub fn load_all() -> ConfigResult<Arc<AppConfig>> {
    // .env 파일이 있으면 로드 (없어도 무시).
    let _ = dotenvy::dotenv();

    load_with(LoaderOptions::default())
}

/// 로더 옵션. 테스트/런타임에서 경로와 env prefix 를 명시하고 싶을 때.
#[derive(Debug, Clone)]
pub struct LoaderOptions {
    /// TOML 검색 루트 (기본 `./configs`). 존재하지 않으면 skip.
    pub toml_dir: PathBuf,
    /// env prefix.
    pub env_prefix: String,
    /// nested 구분자.
    pub env_nested_sep: String,
    /// 명시적 service_name override. None 이면 env `HFT_SERVICE_NAME` 또는 default.
    pub service_override: Option<String>,
}

impl Default for LoaderOptions {
    fn default() -> Self {
        Self {
            toml_dir: PathBuf::from("./configs"),
            env_prefix: "HFT_".into(),
            env_nested_sep: "__".into(),
            service_override: None,
        }
    }
}

/// 명시적 옵션으로 로드.
pub fn load_with(opts: LoaderOptions) -> ConfigResult<Arc<AppConfig>> {
    let default_cfg = AppConfig::default();

    // service_name 결정 우선순위: CLI override > env > toml(나중에 주입) > default
    let env_service = std::env::var(format!("{}SERVICE_NAME", opts.env_prefix)).ok();
    let service_name = opts
        .service_override
        .clone()
        .or(env_service)
        .unwrap_or_else(|| default_cfg.service_name.clone());

    // Figment 합성: default → (service 전용 TOML) → (default.toml) → env
    let mut fig = Figment::new().merge(Serialized::defaults(&default_cfg));

    // default.toml 먼저, 그 위에 service 전용 TOML 이 덮어쓴다.
    let default_toml = opts.toml_dir.join("default.toml");
    if default_toml.is_file() {
        fig = fig.merge(Toml::file(&default_toml));
    }
    let service_toml = opts.toml_dir.join(format!("{}.toml", service_name));
    if service_toml.is_file() {
        fig = fig.merge(Toml::file(&service_toml));
    }

    // env (최우선). `HFT_ZMQ__HWM` → `zmq.hwm`.
    fig = fig.merge(
        Env::prefixed(&opts.env_prefix)
            .split(&opts.env_nested_sep)
            .lowercase(true),
    );

    let mut cfg: AppConfig = fig.extract()?;
    // service_name 은 opts.override 가 최우선.
    if let Some(explicit) = opts.service_override {
        cfg.service_name = explicit;
    }

    Ok(Arc::new(cfg))
}

/// 별도 TOML 문자열만으로 로드 (테스트 전용 entry point).
///
/// defaults + 주어진 TOML + env 합성. env 는 실 환경의 `HFT_*` 를 그대로 사용.
pub fn load_from_toml_str(toml_str: &str) -> ConfigResult<Arc<AppConfig>> {
    let default_cfg = AppConfig::default();
    let fig = Figment::new()
        .merge(Serialized::defaults(&default_cfg))
        .merge(Toml::string(toml_str))
        .merge(Env::prefixed("HFT_").split("__").lowercase(true));
    let cfg: AppConfig = fig.extract()?;
    Ok(Arc::new(cfg))
}

// ─────────────────────────────────────────────────────────────────────────────
// 검증
// ─────────────────────────────────────────────────────────────────────────────

/// invariant 검증. 실패 시 `ConfigError::Invalid`.
///
/// - `hot_workers > 0`
/// - `bg_workers > 0`
/// - `zmq.hwm > 0`
/// - `1 ≤ zmq.drain_batch_cap ≤ 8192`
/// - `zmq.linger_ms ≥ 0`
/// - `exchanges` 비어있지 않음
/// - 각 `ExchangeConfig.symbols` 비어있지 않음
/// - `telemetry.default_level` 가 tracing-subscriber 가 허용하는 값
pub fn validate(cfg: &AppConfig) -> ConfigResult<()> {
    macro_rules! ensure {
        ($cond:expr, $msg:expr) => {
            if !$cond {
                return Err(ConfigError::Invalid($msg.to_string()));
            }
        };
    }

    ensure!(cfg.hot_workers > 0, "hot_workers must be > 0");
    ensure!(cfg.bg_workers > 0, "bg_workers must be > 0");
    ensure!(
        !cfg.service_name.is_empty(),
        "service_name must not be empty"
    );

    ensure!(cfg.zmq.hwm > 0, "zmq.hwm must be > 0");
    ensure!(cfg.zmq.linger_ms >= 0, "zmq.linger_ms must be >= 0");
    ensure!(
        cfg.zmq.drain_batch_cap >= 1 && cfg.zmq.drain_batch_cap <= 8192,
        "zmq.drain_batch_cap must be in 1..=8192"
    );
    ensure!(
        !cfg.zmq.push_endpoint.is_empty(),
        "zmq.push_endpoint must not be empty"
    );
    if let Some(bind) = cfg.zmq.order_ingress_bind.as_ref() {
        ensure!(
            !bind.trim().is_empty(),
            "zmq.order_ingress_bind must not be empty when set"
        );
        let ok_prefix = bind.starts_with("tcp://") || bind.starts_with("inproc://");
        ensure!(
            ok_prefix,
            "zmq.order_ingress_bind must start with tcp:// or inproc://"
        );
    }
    if let Some(bind) = cfg.zmq.result_egress_bind.as_ref() {
        ensure!(
            !bind.trim().is_empty(),
            "zmq.result_egress_bind must not be empty when set"
        );
        ensure!(
            bind.starts_with("tcp://") || bind.starts_with("inproc://"),
            "zmq.result_egress_bind must start with tcp:// or inproc://"
        );
    }

    ensure!(!cfg.exchanges.is_empty(), "exchanges must not be empty");
    for (i, ex) in cfg.exchanges.iter().enumerate() {
        if ex.symbols.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "exchanges[{}] ({:?}) has empty symbols list",
                i, ex.id
            )));
        }
        for (j, sym) in ex.symbols.iter().enumerate() {
            if sym.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "exchanges[{}].symbols[{}] is empty string",
                    i, j
                )));
            }
        }
    }

    // 로그 레벨 단순 체크 (tracing-subscriber 는 "off"/"trace"/... 만 수용).
    let lvl = cfg.telemetry.default_level.to_ascii_lowercase();
    let ok = matches!(
        lvl.as_str(),
        "off" | "error" | "warn" | "info" | "debug" | "trace"
    );
    ensure!(
        ok,
        "telemetry.default_level must be one of off|error|warn|info|debug|trace"
    );

    if let Some(port) = cfg.telemetry.prom_port {
        ensure!(port > 0, "telemetry.prom_port must be > 0");
    }

    // ── SHM ──
    if cfg.shm.enabled {
        // 용량 (공통).
        ensure!(
            cfg.shm.quote_slot_count >= 1,
            "shm.quote_slot_count must be >= 1"
        );
        ensure!(
            cfg.shm.symbol_table_capacity >= 1,
            "shm.symbol_table_capacity must be >= 1"
        );
        ensure!(
            cfg.shm.trade_ring_capacity >= 2 && cfg.shm.trade_ring_capacity.is_power_of_two(),
            "shm.trade_ring_capacity must be a power of two and >= 2"
        );
        ensure!(
            cfg.shm.order_ring_capacity >= 2 && cfg.shm.order_ring_capacity.is_power_of_two(),
            "shm.order_ring_capacity must be a power of two and >= 2"
        );
        ensure!(
            cfg.shm.symbol_table_capacity >= cfg.shm.quote_slot_count,
            "shm.symbol_table_capacity must be >= shm.quote_slot_count"
        );

        // Backend 분기.
        match cfg.shm.backend {
            ShmBackendKind::LegacyMultiFile => {
                ensure!(
                    !cfg.shm.quote_path.as_os_str().is_empty(),
                    "shm.quote_path must not be empty"
                );
                ensure!(
                    !cfg.shm.trade_path.as_os_str().is_empty(),
                    "shm.trade_path must not be empty"
                );
                ensure!(
                    !cfg.shm.order_path.as_os_str().is_empty(),
                    "shm.order_path must not be empty"
                );
                ensure!(
                    !cfg.shm.symtab_path.as_os_str().is_empty(),
                    "shm.symtab_path must not be empty"
                );
            }
            ShmBackendKind::DevShm | ShmBackendKind::Hugetlbfs | ShmBackendKind::PciBar => {
                ensure!(
                    !cfg.shm.shared_path.as_os_str().is_empty(),
                    "shm.shared_path must not be empty for v2 backend"
                );
                ensure!(
                    cfg.shm.n_max >= 1 && cfg.shm.n_max <= 256,
                    "shm.n_max must be in 1..=256 (MAX_ORDER_RINGS)"
                );
                if matches!(cfg.shm.role, ShmRoleKind::Strategy) {
                    ensure!(
                        cfg.shm.vm_id < cfg.shm.n_max,
                        "shm.vm_id must be < shm.n_max when role=Strategy"
                    );
                }
            }
        }
    }

    cfg.order_egress.validate()?;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Supabase (Phase 1: 스텁 / Phase 2: 실제 HTTP)
// ─────────────────────────────────────────────────────────────────────────────

/// Supabase `configs` 테이블에서 runtime override 를 가져와 기존 cfg 위에 덮어씀.
///
/// Phase 1 에선 **NOP stub**: url 이 비어있으면 즉시 Ok 반환, 아니면
/// `tracing::warn!` 로 "deferred to Phase 2" 경고만 남긴다. 이는 SPEC 의
/// "실패 시 warn 후 계속 진행" 계약과 호환된다.
///
/// Phase 2 에서는 아래 기능을 채운다:
/// - reqwest 로 `{url}/rest/v1/configs?service_name=eq.{name}` GET
/// - `fetch_timeout_s` 적용
/// - JSON 응답을 figment `Serialized` provider 로 merge
/// - 실패 시 warn + 기존 cfg 유지
pub async fn refresh_from_supabase(cfg: &Arc<AppConfig>) -> ConfigResult<Arc<AppConfig>> {
    if cfg.supabase.url.is_empty() {
        return Ok(Arc::clone(cfg));
    }
    tracing::warn!(
        target: "hft_config",
        service = %cfg.service_name,
        url = %cfg.supabase.url,
        "refresh_from_supabase: Phase 1 stub — remote config fetch deferred to Phase 2"
    );
    Ok(Arc::clone(cfg))
}

/// service_name 에 맞춰 TOML 파일이 `toml_dir` 에 있는지 체크 (테스트/CI 용 헬퍼).
pub fn has_service_toml(toml_dir: &Path, service_name: &str) -> bool {
    toml_dir.join(format!("{}.toml", service_name)).is_file()
}

// ─────────────────────────────────────────────────────────────────────────────
// 테스트
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_types::ExchangeId;

    /// 여러 env 변수를 동시에 설정하는 테스트 간 경쟁을 막기 위한 공용 뮤텍스.
    /// env 는 프로세스 전역이라 #[test] 가 병렬 실행되면 서로 간섭한다.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LK: OnceLock<Mutex<()>> = OnceLock::new();
        LK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn clear_hft_env() {
        // iterator + remove 를 같이 쓰면 UB 위험이 있으니 먼저 collect.
        let keys: Vec<String> = std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| k.starts_with("HFT_"))
            .collect();
        for k in keys {
            std::env::remove_var(&k);
        }
    }

    fn minimal_valid_cfg() -> AppConfig {
        AppConfig {
            service_name: "test".into(),
            hot_workers: 1,
            bg_workers: 1,
            exchanges: vec![ExchangeConfig {
                id: ExchangeId::Gate,
                symbols: vec![Symbol::from("BTC_USDT")],
                role: DataRole::Primary,
                ws_url: None,
                reconnect_backoff_ms: 500,
                ping_interval_s: 15,
                ignore_symbols: Vec::new(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn defaults_are_sensible() {
        let d = AppConfig::default();
        assert_eq!(d.hot_workers, 1);
        assert_eq!(d.zmq.hwm, 100_000);
        assert_eq!(d.zmq.drain_batch_cap, 512);
        assert!(d.exchanges.is_empty());
        assert!(d.warmup.enabled);
        assert_eq!(d.warmup.events, 5_000);
        assert_eq!(d.supabase.refresh_interval_s, 300);
        assert_eq!(d.order_egress.mode, order_egress::OrderEgressMode::Shm);
    }

    #[test]
    fn validate_rejects_empty_exchanges() {
        let cfg = AppConfig::default();
        let err = validate(&cfg).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
        assert!(err.to_string().contains("exchanges must not be empty"));
    }

    #[test]
    fn validate_rejects_zero_hot_workers() {
        let mut cfg = minimal_valid_cfg();
        cfg.hot_workers = 0;
        let err = validate(&cfg).unwrap_err();
        assert!(err.to_string().contains("hot_workers"));
    }

    #[test]
    fn validate_rejects_drain_batch_cap_out_of_range() {
        let mut cfg = minimal_valid_cfg();
        cfg.zmq.drain_batch_cap = 10_000;
        assert!(validate(&cfg).is_err());
        cfg.zmq.drain_batch_cap = 0;
        assert!(validate(&cfg).is_err());
        cfg.zmq.drain_batch_cap = 8192;
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_rejects_bad_order_ingress_bind_uri() {
        let mut cfg = minimal_valid_cfg();
        cfg.zmq.order_ingress_bind = Some("".into());
        assert!(validate(&cfg).is_err());
        cfg.zmq.order_ingress_bind = Some("ipc://bad".into());
        assert!(validate(&cfg).is_err());
        cfg.zmq.order_ingress_bind = Some("tcp://127.0.0.1:7010".into());
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_rejects_bad_result_egress_bind_uri() {
        let mut cfg = minimal_valid_cfg();
        cfg.zmq.result_egress_bind = Some("".into());
        assert!(validate(&cfg).is_err());
        cfg.zmq.result_egress_bind = Some("ipc://bad".into());
        assert!(validate(&cfg).is_err());
        cfg.zmq.result_egress_bind = Some("inproc://order-result".into());
        validate(&cfg).unwrap();
    }

    #[test]
    fn zmq_result_heartbeat_interval_default_is_sensible() {
        let cfg = minimal_valid_cfg();
        assert_eq!(cfg.zmq.result_heartbeat_interval_ms, 1000);
    }

    #[test]
    fn validate_rejects_empty_symbol_list() {
        let mut cfg = minimal_valid_cfg();
        cfg.exchanges[0].symbols.clear();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn validate_rejects_bad_log_level() {
        let mut cfg = minimal_valid_cfg();
        cfg.telemetry.default_level = "LOUD".into();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn validate_accepts_minimal_valid() {
        let cfg = minimal_valid_cfg();
        validate(&cfg).unwrap();
    }

    #[test]
    fn load_from_toml_overrides_defaults() {
        let _g = env_lock();
        clear_hft_env();
        let toml = r#"
            service_name = "publisher-gate"
            hot_workers = 4
            bg_workers = 2

            [zmq]
            hwm = 200000
            linger_ms = 0
            drain_batch_cap = 1024
            push_endpoint = "inproc://push"
            pub_endpoint = "tcp://0.0.0.0:5555"
            sub_endpoint = "tcp://127.0.0.1:5555"
            order_ingress_bind = "tcp://127.0.0.1:7010"
            result_egress_bind = "tcp://127.0.0.1:7060"
            result_heartbeat_interval_ms = 1500

            [[exchanges]]
            id = "gate"
            symbols = ["BTC_USDT", "ETH_USDT"]
            role = "primary"
        "#;
        let cfg = load_from_toml_str(toml).unwrap();
        assert_eq!(cfg.service_name, "publisher-gate");
        assert_eq!(cfg.hot_workers, 4);
        assert_eq!(cfg.zmq.hwm, 200_000);
        assert_eq!(cfg.zmq.drain_batch_cap, 1024);
        assert_eq!(
            cfg.zmq.order_ingress_bind.as_deref(),
            Some("tcp://127.0.0.1:7010")
        );
        assert_eq!(
            cfg.zmq.result_egress_bind.as_deref(),
            Some("tcp://127.0.0.1:7060")
        );
        assert_eq!(cfg.zmq.result_heartbeat_interval_ms, 1500);
        assert_eq!(cfg.exchanges.len(), 1);
        assert_eq!(cfg.exchanges[0].id, ExchangeId::Gate);
        assert_eq!(cfg.exchanges[0].symbols.len(), 2);
        // default 가 유지되는지
        assert_eq!(cfg.warmup.events, 5_000);
    }

    #[test]
    fn env_overrides_toml() {
        let _g = env_lock();
        clear_hft_env();
        // `HFT_ZMQ__HWM` 이 TOML 의 zmq.hwm 을 override 하는지 (SPEC 완료 조건).
        std::env::set_var("HFT_ZMQ__HWM", "50000");
        std::env::set_var("HFT_HOT_WORKERS", "8");

        let toml = r#"
            service_name = "publisher-gate"
            hot_workers = 4
            bg_workers = 2

            [zmq]
            hwm = 200000
            linger_ms = 0
            drain_batch_cap = 1024
            push_endpoint = "inproc://push"
            pub_endpoint = "tcp://0.0.0.0:5555"
            sub_endpoint = "tcp://127.0.0.1:5555"
            order_ingress_bind = "inproc://order-ingress"
            result_egress_bind = "inproc://order-result"

            [[exchanges]]
            id = "gate"
            symbols = ["BTC_USDT"]
        "#;
        let cfg = load_from_toml_str(toml).unwrap();
        assert_eq!(cfg.zmq.hwm, 50_000, "env must beat TOML");
        assert_eq!(cfg.hot_workers, 8);

        clear_hft_env();
    }

    #[test]
    fn env_nested_sep_double_underscore() {
        let _g = env_lock();
        clear_hft_env();
        std::env::set_var("HFT_TELEMETRY__STDOUT_JSON", "true");
        std::env::set_var("HFT_WARMUP__ENABLED", "false");
        std::env::set_var("HFT_WARMUP__EVENTS", "42");

        let toml = r#"
            service_name = "svc"
            hot_workers = 1
            bg_workers = 1

            [[exchanges]]
            id = "binance"
            symbols = ["BTC_USDT"]
        "#;
        let cfg = load_from_toml_str(toml).unwrap();
        assert!(cfg.telemetry.stdout_json);
        assert!(!cfg.warmup.enabled);
        assert_eq!(cfg.warmup.events, 42);

        clear_hft_env();
    }

    #[test]
    fn load_with_no_configs_dir_succeeds_from_env_only() {
        let _g = env_lock();
        clear_hft_env();
        std::env::set_var("HFT_HOT_WORKERS", "2");
        std::env::set_var("HFT_SERVICE_NAME", "env-only");

        let opts = LoaderOptions {
            toml_dir: PathBuf::from("/definitely/does/not/exist"),
            ..Default::default()
        };
        let cfg = load_with(opts).unwrap();
        assert_eq!(cfg.service_name, "env-only");
        assert_eq!(cfg.hot_workers, 2);

        clear_hft_env();
    }

    #[test]
    fn load_with_picks_up_service_specific_toml() {
        let _g = env_lock();
        clear_hft_env();

        // 임시 디렉터리에 default.toml + {service}.toml 두 개 써서 override 확인.
        let tmp = std::env::temp_dir().join(format!("hft-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        std::fs::write(
            tmp.join("default.toml"),
            r#"
                hot_workers = 2
                bg_workers = 2
                [[exchanges]]
                id = "binance"
                symbols = ["X"]
            "#,
        )
        .unwrap();
        std::fs::write(
            tmp.join("publisher-gate.toml"),
            r#"
                hot_workers = 16
            "#,
        )
        .unwrap();

        let opts = LoaderOptions {
            toml_dir: tmp.clone(),
            service_override: Some("publisher-gate".into()),
            ..Default::default()
        };
        let cfg = load_with(opts).unwrap();
        assert_eq!(cfg.service_name, "publisher-gate");
        assert_eq!(cfg.hot_workers, 16, "service toml beats default toml");
        assert_eq!(cfg.bg_workers, 2, "default toml still contributes");
        assert_eq!(cfg.exchanges.len(), 1);

        let _ = std::fs::remove_dir_all(&tmp);
        clear_hft_env();
    }

    #[test]
    fn unknown_env_does_not_break_loading() {
        // 사용자가 HFT_LOG (tracing EnvFilter 용) 같은 우리 schema 밖 env 를
        // 갖고 있어도 extract 가 실패하면 안 된다 (deny_unknown_fields 없음).
        let _g = env_lock();
        clear_hft_env();
        std::env::set_var("HFT_LOG", "info,hft=debug");
        std::env::set_var("HFT_TOTALLY_UNRELATED", "whatever");
        std::env::set_var("HFT_HOT_WORKERS", "3");

        let toml = r#"
            service_name = "svc"
            bg_workers = 1
            [[exchanges]]
            id = "okx"
            symbols = ["BTC_USDT"]
        "#;
        let cfg = load_from_toml_str(toml).unwrap();
        assert_eq!(cfg.hot_workers, 3);

        clear_hft_env();
    }

    #[test]
    fn supabase_service_key_from_env() {
        let _g = env_lock();
        clear_hft_env();
        std::env::set_var("HFT_SUPABASE__SERVICE_KEY", "s3cr3t");

        let toml = r#"
            service_name = "svc"
            hot_workers = 1
            bg_workers = 1
            [[exchanges]]
            id = "bybit"
            symbols = ["BTC_USDT"]
            [supabase]
            url = "https://x.supabase.co"
            anon_key = "anon"
        "#;
        let cfg = load_from_toml_str(toml).unwrap();
        assert_eq!(cfg.supabase.service_key, "s3cr3t");
        assert_eq!(cfg.supabase.url, "https://x.supabase.co");

        clear_hft_env();
    }

    #[tokio::test]
    async fn supabase_stub_noop_when_url_empty() {
        let cfg = Arc::new(minimal_valid_cfg());
        let back = refresh_from_supabase(&cfg).await.unwrap();
        assert!(Arc::ptr_eq(&cfg, &back));
    }

    #[tokio::test]
    async fn supabase_stub_warns_when_url_present() {
        let mut cfg = minimal_valid_cfg();
        cfg.supabase.url = "https://example.supabase.co".into();
        let arc = Arc::new(cfg);
        let back = refresh_from_supabase(&arc).await.unwrap();
        // 현재는 그대로 돌려줌 (Phase 1 stub).
        assert_eq!(back.supabase.url, "https://example.supabase.co");
    }

    // ── SHM ───────────────────────────────────────────────────────────────

    #[test]
    fn shm_defaults_are_production_ready() {
        let s = ShmConfig::default();
        assert!(s.enabled);
        assert_eq!(s.quote_path, PathBuf::from("/dev/shm/hft_quotes_v2"));
        assert_eq!(s.trade_path, PathBuf::from("/dev/shm/hft_trades_v2"));
        assert_eq!(s.order_path, PathBuf::from("/dev/shm/hft_orders_v2"));
        assert_eq!(s.symtab_path, PathBuf::from("/dev/shm/hft_symtab_v2"));
        assert_eq!(s.quote_slot_count, 10_000);
        assert_eq!(s.trade_ring_capacity, 1 << 20);
        assert_eq!(s.order_ring_capacity, 16_384);
        assert_eq!(s.symbol_table_capacity, 16_384);
        // ring capacity 는 반드시 power-of-two.
        assert!(s.trade_ring_capacity.is_power_of_two());
        assert!(s.order_ring_capacity.is_power_of_two());
    }

    #[test]
    fn validate_rejects_non_power_of_two_trade_ring() {
        let mut cfg = minimal_valid_cfg();
        cfg.shm.trade_ring_capacity = 1000; // not power-of-two
        let e = validate(&cfg).unwrap_err();
        assert!(e.to_string().contains("trade_ring_capacity"));
    }

    #[test]
    fn validate_rejects_non_power_of_two_order_ring() {
        let mut cfg = minimal_valid_cfg();
        cfg.shm.order_ring_capacity = 100;
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn validate_accepts_power_of_two_ring_sizes() {
        let mut cfg = minimal_valid_cfg();
        cfg.shm.trade_ring_capacity = 1 << 16;
        cfg.shm.order_ring_capacity = 1 << 10;
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_rejects_symbol_table_smaller_than_quote_slots() {
        let mut cfg = minimal_valid_cfg();
        cfg.shm.quote_slot_count = 1000;
        cfg.shm.symbol_table_capacity = 100;
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn validate_skips_shm_when_disabled() {
        let mut cfg = minimal_valid_cfg();
        // 말이 안 되는 값이지만 disabled 상태면 통과해야 함.
        cfg.shm.enabled = false;
        cfg.shm.trade_ring_capacity = 7;
        cfg.shm.order_ring_capacity = 3;
        cfg.shm.symbol_table_capacity = 0;
        validate(&cfg).unwrap();
    }

    // ── v2 Multi-VM 검증 ─────────────────────────────────────────────────

    #[test]
    fn validate_rejects_v2_with_empty_shared_path() {
        let mut cfg = minimal_valid_cfg();
        cfg.shm.backend = ShmBackendKind::DevShm;
        cfg.shm.shared_path = PathBuf::new();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn validate_rejects_v2_with_bad_n_max() {
        let mut cfg = minimal_valid_cfg();
        cfg.shm.backend = ShmBackendKind::DevShm;
        cfg.shm.n_max = 0;
        assert!(validate(&cfg).is_err());
        cfg.shm.n_max = 300;
        assert!(validate(&cfg).is_err());
        cfg.shm.n_max = 16;
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_rejects_strategy_with_vm_id_out_of_range() {
        let mut cfg = minimal_valid_cfg();
        cfg.shm.backend = ShmBackendKind::DevShm;
        cfg.shm.role = ShmRoleKind::Strategy;
        cfg.shm.n_max = 4;
        cfg.shm.vm_id = 4;
        assert!(validate(&cfg).is_err());
        cfg.shm.vm_id = 3;
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_legacy_backend_still_requires_all_paths() {
        let mut cfg = minimal_valid_cfg();
        cfg.shm.backend = ShmBackendKind::LegacyMultiFile;
        // legacy 모드에선 shared_path 는 무시되고, 4개 경로만 검사.
        cfg.shm.quote_path = PathBuf::new();
        assert!(validate(&cfg).is_err());
    }

    #[test]
    fn shm_defaults_use_v2_devshm_backend() {
        let s = ShmConfig::default();
        assert_eq!(s.backend, ShmBackendKind::DevShm);
        assert_eq!(s.role, ShmRoleKind::Publisher);
        assert_eq!(s.vm_id, 0);
        assert_eq!(s.n_max, 16);
        assert_eq!(s.shared_path, PathBuf::from("/dev/shm/hft_shared_v2"));
    }

    #[test]
    fn shm_env_override_works() {
        let _g = env_lock();
        clear_hft_env();
        std::env::set_var("HFT_SHM__ENABLED", "false");
        std::env::set_var("HFT_SHM__TRADE_RING_CAPACITY", "65536");
        std::env::set_var("HFT_SHM__QUOTE_PATH", "/tmp/custom_quotes");

        let toml = r#"
            service_name = "svc"
            hot_workers = 1
            bg_workers = 1
            [[exchanges]]
            id = "gate"
            symbols = ["BTC_USDT"]
        "#;
        let cfg = load_from_toml_str(toml).unwrap();
        assert!(!cfg.shm.enabled);
        assert_eq!(cfg.shm.trade_ring_capacity, 65_536);
        assert_eq!(cfg.shm.quote_path, PathBuf::from("/tmp/custom_quotes"));

        clear_hft_env();
    }

    #[test]
    fn order_egress_env_overlay_works() {
        let _g = env_lock();
        clear_hft_env();
        std::env::set_var("HFT_ORDER_EGRESS__MODE", "zmq");
        std::env::set_var("HFT_ORDER_EGRESS__SHM__STRATEGY_RING_ID", "3");
        std::env::set_var("HFT_ORDER_EGRESS__ZMQ__ENDPOINT", "tcp://infra-vm:5560");
        std::env::set_var("HFT_ORDER_EGRESS__ZMQ__SEND_HWM", "4096");
        std::env::set_var("HFT_ORDER_EGRESS__BACKPRESSURE__MODE", "retry");
        std::env::set_var("HFT_ORDER_EGRESS__BACKPRESSURE__MAX_RETRIES", "3");
        std::env::set_var("HFT_ORDER_EGRESS__BACKPRESSURE__BACKOFF_NS", "1000");
        std::env::set_var("HFT_ORDER_EGRESS__BACKPRESSURE__TOTAL_TIMEOUT_NS", "3000");

        let toml = r#"
            service_name = "svc"
            hot_workers = 1
            bg_workers = 1
            [[exchanges]]
            id = "gate"
            symbols = ["BTC_USDT"]
        "#;
        let cfg = load_from_toml_str(toml).unwrap();
        assert_eq!(cfg.order_egress.mode, order_egress::OrderEgressMode::Zmq);
        assert_eq!(cfg.order_egress.shm.strategy_ring_id, 3);
        let zmq = cfg.order_egress.zmq.as_ref().expect("zmq config");
        assert_eq!(zmq.endpoint, "tcp://infra-vm:5560");
        assert_eq!(zmq.send_hwm, 4096);
        assert_eq!(
            cfg.order_egress.backpressure,
            order_egress::BackpressurePolicy::RetryWithTimeout {
                max_retries: 3,
                backoff_ns: 1000,
                total_timeout_ns: 3000,
            }
        );

        clear_hft_env();
    }
}

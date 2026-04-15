//! config_layering — `hft_config` 의 **defaults → TOML → env** 우선순위 검증.
//!
//! # 왜 integration 레벨에서 또 하는가
//! - `hft-config` unit 테스트는 `load_from_toml_str` 을 호출해 문자열 TOML + env 조합만 본다.
//! - 실제 서비스(publisher/subscriber/...) 기동 시 문제 되는 시나리오는
//!   **"TOML 은 한 값인데 env 가 덮어써야 한다"**, **"TOML 생략 필드는 default 로 fallback"**,
//!   **"service_override 는 env 보다 우선"** 같은 조합이다.
//!   hft-config 단독 테스트만으로는 이 조합이 integration 빌드에서 회귀 없이 동작하는지
//!   보장하기 어렵다 (env 동시 접근·precedence 실수 등).
//! - 따라서 이 파일은 process-wide env 를 조작하는 **serialized integration test** 로 다룬다.
//!
//! # 동시성 안전
//! - Rust integration 테스트는 한 binary 안에서 기본적으로 병렬 실행된다.
//! - env 는 process global 이므로, 이 파일의 모든 테스트는 파일-로컬 `Mutex` 를 통해
//!   **직렬화**한다. `std::sync::Mutex` 를 `static` 으로 두고 각 테스트 시작 시 lock 획득.
//! - 또, 테스트 시작 시 현재 프로세스의 모든 `HFT_*` 변수를 snapshot 해서 RAII 가드로 복원한다.
//!   실 사용자 shell 에 `HFT_*` 가 떠 있어도 테스트가 영향을 주고/받지 않는다.
//!
//! # 검증 항목
//! 1. default 만 (TOML 없음, env 없음) → `AppConfig::default()` 그대로.
//! 2. TOML 만 → TOML 값이 default 를 덮어쓴다.
//! 3. TOML + env → env 가 TOML 을 덮어쓴다 (최우선).
//! 4. TOML 에 없는 필드 → default 로 fallback.
//! 5. `load_with(LoaderOptions { service_override })` → env `HFT_SERVICE_NAME` 보다 우선.
//! 6. `validate()` 가 잘못된 조합을 reject.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use hft_config::{load_from_toml_str, load_with, validate, ConfigError, LoaderOptions};

// ─────────────────────────────────────────────────────────────────────────────
// 테스트 직렬화 & env 스냅샷
// ─────────────────────────────────────────────────────────────────────────────

/// 파일 전역 직렬화 mutex. env 를 만지는 동안 다른 테스트가 끼어들면 안 됨.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    // poison 무시 — 직전 테스트가 panic 해도 이후 테스트는 진행해야 함.
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `HFT_*` env 를 스냅샷하고, drop 시 원래 상태로 복원하는 RAII 가드.
///
/// - 생성 시점의 모든 `HFT_*` 변수 (key=value) 를 기록한다.
/// - 그런 다음 **현재 프로세스의 모든 `HFT_*` 를 제거**해 clean slate 를 만든다.
/// - drop 시: 테스트 중에 새로 생긴 `HFT_*` 를 지우고, 스냅샷된 값을 되돌린다.
struct EnvGuard {
    snapshot: HashMap<String, String>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn new() -> Self {
        let lock = env_lock();
        let snapshot: HashMap<String, String> = std::env::vars()
            .filter(|(k, _)| k.starts_with("HFT_"))
            .collect();
        // clean slate.
        for k in snapshot.keys() {
            std::env::remove_var(k);
        }
        Self {
            snapshot,
            _lock: lock,
        }
    }

    /// 테스트 안에서 값 설정.
    fn set(&self, k: &str, v: &str) {
        assert!(k.starts_with("HFT_"), "test-set env must start with HFT_");
        std::env::set_var(k, v);
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // 테스트가 심은 `HFT_*` 전부 제거.
        let current_hft: Vec<String> = std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| k.starts_with("HFT_"))
            .collect();
        for k in current_hft {
            std::env::remove_var(&k);
        }
        // 스냅샷 복원.
        for (k, v) in &self.snapshot {
            std::env::set_var(k, v);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 테스트 TOML 샘플
// ─────────────────────────────────────────────────────────────────────────────

/// 전 필드를 default 와 다른 값으로 설정한 TOML.
/// 이 TOML 이 먹히면 defaults 가 전부 overwrite 된다.
const FULL_TOML: &str = r#"
service_name = "from-toml"
hot_workers = 4
bg_workers = 2

[zmq]
hwm = 50000
linger_ms = 100
drain_batch_cap = 256
push_endpoint = "inproc://toml-push"
pub_endpoint = "tcp://0.0.0.0:6000"
sub_endpoint = "tcp://127.0.0.1:6000"

[[exchanges]]
id = "gate"
symbols = ["BTC_USDT", "ETH_USDT"]
role = "primary"

[questdb]
ilp_addr = "127.0.0.1:9000"
batch_rows = 500
batch_ms = 250
spool_dir = "./toml-spool"

[telemetry]
default_level = "debug"
stdout_json = true

[warmup]
enabled = false
events = 100
"#;

/// 일부 필드만 설정한 TOML — 빠진 필드는 default 로 fallback 되어야.
const PARTIAL_TOML: &str = r#"
service_name = "partial"

[zmq]
hwm = 12345
linger_ms = 0
drain_batch_cap = 512
push_endpoint = "inproc://partial-push"
pub_endpoint = "tcp://0.0.0.0:7000"
sub_endpoint = "tcp://127.0.0.1:7000"

[[exchanges]]
id = "gate"
symbols = ["BTC_USDT"]
role = "primary"
"#;

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// (1) 아무 TOML/env 가 없으면 모든 값이 `AppConfig::default()` 와 일치해야 한다.
#[test]
fn defaults_only_matches_app_default() {
    let _guard = EnvGuard::new();

    let cfg = load_from_toml_str("").expect("empty TOML should still load defaults");

    assert_eq!(cfg.service_name, "hft-service");
    assert_eq!(cfg.hot_workers, 1);
    assert_eq!(cfg.bg_workers, 1);
    assert_eq!(cfg.zmq.hwm, 100_000);
    assert_eq!(cfg.zmq.drain_batch_cap, 512);
    assert_eq!(cfg.zmq.linger_ms, 0);
    assert_eq!(cfg.zmq.push_endpoint, "inproc://hft-push");
    assert!(cfg.exchanges.is_empty());
    assert_eq!(cfg.telemetry.default_level, "info");
    assert!(!cfg.telemetry.stdout_json);
    assert!(cfg.warmup.enabled);
    assert_eq!(cfg.warmup.events, 5_000);
}

/// (2) TOML 이 default 를 덮어쓴다.
#[test]
fn toml_overrides_defaults() {
    let _guard = EnvGuard::new();

    let cfg = load_from_toml_str(FULL_TOML).expect("full TOML should parse");

    assert_eq!(cfg.service_name, "from-toml");
    assert_eq!(cfg.hot_workers, 4);
    assert_eq!(cfg.bg_workers, 2);
    assert_eq!(cfg.zmq.hwm, 50_000);
    assert_eq!(cfg.zmq.drain_batch_cap, 256);
    assert_eq!(cfg.zmq.linger_ms, 100);
    assert_eq!(cfg.zmq.push_endpoint, "inproc://toml-push");
    assert_eq!(cfg.zmq.pub_endpoint, "tcp://0.0.0.0:6000");
    assert_eq!(cfg.zmq.sub_endpoint, "tcp://127.0.0.1:6000");
    assert_eq!(cfg.exchanges.len(), 1);
    assert_eq!(cfg.exchanges[0].symbols.len(), 2);
    assert_eq!(cfg.questdb.ilp_addr, "127.0.0.1:9000");
    assert_eq!(cfg.questdb.batch_rows, 500);
    assert_eq!(cfg.telemetry.default_level, "debug");
    assert!(cfg.telemetry.stdout_json);
    assert!(!cfg.warmup.enabled);
    assert_eq!(cfg.warmup.events, 100);

    // sanity: TOML 이 통과했으면 validate 도 OK.
    validate(&cfg).expect("full TOML must validate");
}

/// (3) env 가 TOML 을 덮어쓴다 (최우선).
#[test]
fn env_overrides_toml() {
    let guard = EnvGuard::new();

    // TOML 은 hwm=50_000 으로 설정. env 로 77_777 을 밀어넣어 TOML 을 덮어쓴다.
    guard.set("HFT_ZMQ__HWM", "77777");
    // 평탄한 필드도 덮어써지는지 확인.
    guard.set("HFT_HOT_WORKERS", "8");
    // 중첩 문자열 필드도 확인.
    guard.set("HFT_TELEMETRY__DEFAULT_LEVEL", "trace");

    let cfg = load_from_toml_str(FULL_TOML).expect("full TOML + env should parse");

    // env 가 이겼는지 확인.
    assert_eq!(cfg.zmq.hwm, 77_777, "env HFT_ZMQ__HWM must override TOML");
    assert_eq!(cfg.hot_workers, 8, "env HFT_HOT_WORKERS must override TOML");
    assert_eq!(
        cfg.telemetry.default_level, "trace",
        "env HFT_TELEMETRY__DEFAULT_LEVEL must override TOML"
    );

    // env 가 건드리지 않은 TOML 값은 그대로.
    assert_eq!(cfg.zmq.drain_batch_cap, 256);
    assert_eq!(cfg.service_name, "from-toml");
}

/// (4) TOML 에 빠진 섹션/필드는 default 로 fallback.
#[test]
fn partial_toml_falls_back_to_defaults() {
    let _guard = EnvGuard::new();

    let cfg = load_from_toml_str(PARTIAL_TOML).expect("partial TOML should parse");

    // TOML 에서 준 값.
    assert_eq!(cfg.service_name, "partial");
    assert_eq!(cfg.zmq.hwm, 12_345);
    assert_eq!(cfg.zmq.push_endpoint, "inproc://partial-push");

    // TOML 에서 안 준 값 → default 로 fallback.
    // hot_workers 는 TOML 에 없음.
    assert_eq!(cfg.hot_workers, 1, "missing hot_workers → default 1");
    assert_eq!(cfg.bg_workers, 1, "missing bg_workers → default 1");
    // questdb 섹션 전체 생략 → default.
    assert_eq!(cfg.questdb.batch_rows, 1000);
    assert_eq!(cfg.questdb.batch_ms, 500);
    // telemetry 섹션 생략 → default.
    assert_eq!(cfg.telemetry.default_level, "info");
    assert!(!cfg.telemetry.stdout_json);
    // warmup 섹션 생략 → default (enabled=true, events=5000).
    assert!(cfg.warmup.enabled);
    assert_eq!(cfg.warmup.events, 5_000);
}

/// (5) `LoaderOptions::service_override` 가 env `HFT_SERVICE_NAME` 를 이긴다.
///
/// `load_with` 는 TOML 경로 검색까지 수행하는데, 이 테스트는 존재하지 않는 디렉터리를 줘서
/// TOML 을 skip 시키고 defaults + env + override 조합만 본다.
#[test]
fn service_override_beats_env() {
    let guard = EnvGuard::new();

    guard.set("HFT_SERVICE_NAME", "from-env");

    let opts = LoaderOptions {
        toml_dir: std::path::PathBuf::from("/nonexistent-dir-for-test"),
        env_prefix: "HFT_".into(),
        env_nested_sep: "__".into(),
        service_override: Some("from-override".into()),
    };
    let cfg = load_with(opts).expect("load_with should succeed");

    assert_eq!(
        cfg.service_name, "from-override",
        "service_override must beat env HFT_SERVICE_NAME"
    );
}

/// (5b) override 가 없을 때는 env `HFT_SERVICE_NAME` 이 default 를 덮는다.
#[test]
fn env_service_name_beats_default() {
    let guard = EnvGuard::new();

    guard.set("HFT_SERVICE_NAME", "from-env-only");

    let opts = LoaderOptions {
        toml_dir: std::path::PathBuf::from("/nonexistent-dir-for-test"),
        env_prefix: "HFT_".into(),
        env_nested_sep: "__".into(),
        service_override: None,
    };
    let cfg = load_with(opts).expect("load_with should succeed");

    assert_eq!(cfg.service_name, "from-env-only");
}

/// (6) `validate` 가 알려진 invariant 위반을 거절한다.
///
/// env 로 `hot_workers = 0` 을 강제 주입하고 validate 가 Err 를 반환하는지 확인.
/// hot path 규칙이 CI 회귀 없이 유지되는지 증명.
#[test]
fn validate_rejects_zero_hot_workers() {
    let guard = EnvGuard::new();

    guard.set("HFT_HOT_WORKERS", "0");
    // exchanges 도 채워야 validate 의 다른 항목이 먼저 걸리지 않음.
    let toml = r#"
[[exchanges]]
id = "gate"
symbols = ["BTC_USDT"]
role = "primary"
"#;

    let cfg = load_from_toml_str(toml).expect("toml parses");
    let err = validate(&cfg).expect_err("zero hot_workers must fail validate");
    match err {
        ConfigError::Invalid(msg) => assert!(
            msg.contains("hot_workers"),
            "expected hot_workers message, got: {msg}"
        ),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

/// (6b) exchanges 가 비면 validate 가 거절한다.
#[test]
fn validate_rejects_empty_exchanges() {
    let _guard = EnvGuard::new();

    let cfg = load_from_toml_str("").expect("empty TOML parses");
    let err = validate(&cfg).expect_err("empty exchanges must fail validate");
    match err {
        ConfigError::Invalid(msg) => assert!(
            msg.to_lowercase().contains("exchange"),
            "expected exchange-related message, got: {msg}"
        ),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

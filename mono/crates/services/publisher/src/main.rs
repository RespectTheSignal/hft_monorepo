//! publisher 바이너리 엔트리포인트.
//!
//! # 기동 순서 (SPEC.md §Startup)
//! 1. `hft_config::load_all()` — defaults → TOML → env 계층 병합, `Arc<AppConfig>` 획득.
//! 2. `hft_telemetry::init()` — tracing/fmt subscriber 조립. **TelemetryHandle 은 main
//!    의 scope 가 끝날 때까지 drop 되지 않아야** OTel flush 가 정상 동작한다.
//! 3. (옵션) Linux CPU affinity — Phase 2 에서 도입 예정. 현재는 로그만 남기고 skip.
//! 4. `publisher::start(cfg, readiness_port)` — 모든 subsystem(feed tasks / Aggregator /
//!    readiness probe) 를 async 스폰하고 `PublisherHandle` 반환.
//! 5. `ctrlc` 핸들러 설치 — SIGINT/SIGTERM 발생 시 `handle.shutdown()` 호출.
//! 6. `handle.join().await` — 모든 task 종료까지 blocking.
//!
//! # 안정성 메모
//! - mimalloc 을 global allocator 로 지정 (저지연 malloc/free, 50ms 예산 준수에 필수).
//! - Tokio 런타임은 `#[tokio::main]` 기본 (multi-thread). Phase 2 에서 `current_thread`
//!   + 명시적 worker core pinning 으로 전환 검토.
//! - panic 시 stderr 로 명확히 로그 후 non-zero exit. telemetry handle drop 으로 flush.
//! - ctrlc 설치 실패는 치명적이지 않다 (log + 계속). 단 kill 시 graceful drain 이 불가.

#![deny(rust_2018_idioms)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use hft_exchange_api::CancellationToken;
use tracing::{error, info, warn};

use hft_config::AppConfig;

/// 전역 allocator: mimalloc (hot path 지연 균일화).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// 환경변수: readiness probe 바인딩 포트. 없으면 disable.
const ENV_READINESS_PORT: &str = "HFT_PUBLISHER_READINESS_PORT";

/// 환경변수: readiness probe 포트 파싱 실패 시 로그만 남기고 disable (hard-fail X).
fn readiness_port_from_env() -> Option<u16> {
    let raw = std::env::var(ENV_READINESS_PORT).ok()?;
    match raw.trim().parse::<u16>() {
        Ok(port) if port > 0 => Some(port),
        Ok(_) => {
            warn!(
                env = ENV_READINESS_PORT,
                value = %raw,
                "readiness port must be > 0, disabling readiness probe"
            );
            None
        }
        Err(e) => {
            warn!(
                env = ENV_READINESS_PORT,
                value = %raw,
                error = %e,
                "failed to parse readiness port, disabling readiness probe"
            );
            None
        }
    }
}

/// 설정 → telemetry 초기화.
///
/// hft-config 의 `TelemetryConfig` 를 hft-telemetry 의 `TelemetryConfig` 로 1:1 매핑.
fn init_telemetry(cfg: &AppConfig) -> Result<hft_telemetry::TelemetryHandle> {
    let tcfg = hft_telemetry::TelemetryConfig {
        otlp_endpoint: cfg.telemetry.otlp_endpoint.clone(),
        default_level: cfg.telemetry.default_level.clone(),
        json_logs: cfg.telemetry.stdout_json,
    };
    hft_telemetry::init(&tcfg, &cfg.service_name).context("telemetry init failed")
}

/// SIGINT/SIGTERM 수신 시 `cancel.cancel()` 을 호출하도록 ctrlc 설치.
///
/// `PublisherHandle::join` 이 `self` 를 소비하기 때문에 handle 을 Arc 로 감싸는 대신
/// `CancellationToken` 만 복제해서 넘긴다. token 은 `Clone` 이며 내부 state 공유.
///
/// ctrlc crate 는 signal handler 를 1회만 등록할 수 있다. 중복 등록 시 에러가 반환되며,
/// 이 함수는 에러를 log 로만 남기고 swallow 한다 (ex: 테스트 환경에서 pre-install 된 경우).
fn install_signal_handler(cancel: CancellationToken) {
    // cancel() 은 idempotent 하지만 재진입 시 "second signal → abort" 로직을 위해
    // 별도 플래그 유지.
    let fired = Arc::new(AtomicBool::new(false));

    let install_result = ctrlc::set_handler(move || {
        if fired.swap(true, Ordering::SeqCst) {
            // 2 번째 신호: 사용자가 강제 종료를 원한다고 판단. 즉시 abort.
            eprintln!("[publisher] second signal received — aborting");
            std::process::exit(130);
        }
        eprintln!("[publisher] shutdown signal received — draining");
        cancel.cancel();
    });

    if let Err(e) = install_result {
        warn!(error = %e, "failed to install ctrlc handler (already installed?)");
    }
}

/// 실제 run 로직. 에러는 main 에서 exitcode 로 변환.
async fn run() -> Result<()> {
    // 1) config
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;

    // 2) telemetry (main scope 에서 살아있어야 함 — ? 로 early-return 해도 drop 은 여기서
    //    안 일어나고 main 의 컴파일된 match 에서 발생 → OK)
    let _tele = init_telemetry(&cfg)?;

    info!(
        service = %cfg.service_name,
        exchanges = cfg.exchanges.len(),
        push = %cfg.zmq.push_endpoint,
        pub_ = %cfg.zmq.pub_endpoint,
        hwm = cfg.zmq.hwm,
        drain_cap = cfg.zmq.drain_batch_cap,
        "publisher starting"
    );

    // 3) CPU affinity — Phase 2 TODO.
    //    현재는 `hft_telemetry::pin_current_thread` 를 쓰지 않고 tokio 에 맡긴다.

    // 4) publisher::start
    let readiness_port = readiness_port_from_env();
    if let Some(port) = readiness_port {
        info!(port = port, "readiness probe enabled");
    }

    let handle = publisher::start(cfg.clone(), readiness_port)
        .await
        .context("publisher::start failed")?;

    // 5) signal handler — CancellationToken 복제해 넘긴다. handle 은 join 으로 소비.
    install_signal_handler(handle.cancel.clone());

    // 6) 모든 background task 종료까지 blocking.
    handle.join().await;

    info!("publisher exited cleanly");
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // telemetry 가 초기화되기 전 에러일 수도 있으므로 stderr 병행.
            eprintln!("[publisher] fatal: {e:#}");
            error!(error = %format!("{e:#}"), "publisher fatal error");
            ExitCode::from(1)
        }
    }
}

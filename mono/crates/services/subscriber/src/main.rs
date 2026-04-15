//! subscriber 바이너리 엔트리포인트.
//!
//! # 기동 순서
//! 1. `hft_config::load_all()` → `Arc<AppConfig>`
//! 2. `hft_telemetry::init()`
//! 3. `subscriber::start(cfg, downstream)` — 이때 downstream 은 Phase 1 의 default
//!    구성(InprocQueue) 으로 **strategy 프로세스가 없는 단독 실행** 모드에선 그냥
//!    이벤트를 drop 한다. Strategy 와 같은 프로세스로 합치는 것은 Phase 2
//!    또는 최상위 wrapper 바이너리의 역할.
//! 4. ctrlc 핸들러 설치 → cancel token.
//! 5. handle.join().await.
//!
//! # 독립 실행 모드
//! 이 바이너리 단독으로 구동할 때는 downstream 을 DirectFn(noop) 로 꽂아 단순히
//! PUB 수신을 검증하는 용도로 동작한다. Phase 1 의 **latency-probe** 가 대신 실제
//! 소비 책임을 맡는다. 따라서 이 바이너리의 실질적 가치는
//! (a) 구독 prefix 설정 검증, (b) counter/log 관찰이다.

#![deny(rust_2018_idioms)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use hft_exchange_api::CancellationToken;
use tracing::{error, info, warn};

use hft_config::AppConfig;
use subscriber::DirectFn;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// telemetry 초기화 — publisher main.rs 와 동일한 1:1 매핑.
fn init_telemetry(cfg: &AppConfig) -> Result<hft_telemetry::TelemetryHandle> {
    let tcfg = hft_telemetry::TelemetryConfig {
        otlp_endpoint: cfg.telemetry.otlp_endpoint.clone(),
        default_level: cfg.telemetry.default_level.clone(),
        json_logs: cfg.telemetry.stdout_json,
    };
    hft_telemetry::init(&tcfg, &cfg.service_name).context("telemetry init failed")
}

fn install_signal_handler(cancel: CancellationToken) {
    let fired = Arc::new(AtomicBool::new(false));
    let install_result = ctrlc::set_handler(move || {
        if fired.swap(true, Ordering::SeqCst) {
            eprintln!("[subscriber] second signal — aborting");
            std::process::exit(130);
        }
        eprintln!("[subscriber] shutdown signal received");
        cancel.cancel();
    });
    if let Err(e) = install_result {
        warn!(error = %e, "failed to install ctrlc handler");
    }
}

async fn run() -> Result<()> {
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;
    let _tele = init_telemetry(&cfg)?;

    info!(
        service = %cfg.service_name,
        sub_endpoint = %cfg.zmq.sub_endpoint,
        drain_cap = cfg.zmq.drain_batch_cap,
        exchanges = cfg.exchanges.len(),
        "subscriber starting"
    );

    // 독립 실행 모드: counter 만 세고 버린다.
    // Phase 1 의 실 트래픽 소비는 latency-probe 가 담당.
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_for_fn = seen.clone();
    let downstream = Arc::new(DirectFn::new(move |_ev, _ls| {
        seen_for_fn.fetch_add(1, Ordering::Relaxed);
    }));

    subscriber::install_panic_hook();

    let handle = subscriber::start(cfg.clone(), downstream)
        .await
        .context("subscriber::start failed")?;

    install_signal_handler(handle.cancel.clone());

    // 10s 마다 관찰 로그 (cold path).
    let report_cancel = handle.cancel.clone();
    let report_seen = seen.clone();
    let report_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            tokio::select! {
                _ = report_cancel.cancelled() => break,
                _ = interval.tick() => {
                    let n = report_seen.load(Ordering::Relaxed);
                    info!(target: "subscriber::report", seen = n, "subscriber seen total");
                }
            }
        }
    });

    handle.join().await;
    let _ = report_task.await;

    let total = seen.load(Ordering::Relaxed);
    info!(target: "subscriber::report", seen = total, "subscriber exited");
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[subscriber] fatal: {e:#}");
            error!(error = %format!("{e:#}"), "subscriber fatal error");
            ExitCode::from(1)
        }
    }
}

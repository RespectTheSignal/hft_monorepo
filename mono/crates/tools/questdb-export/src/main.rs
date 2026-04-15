//! questdb-export — SUB 스트림을 소비해 QuestDB ILP 로 flush.
//!
//! # 구조
//! 1. `subscriber::start(..., InprocQueue)` 로 SUB 가 decode 한 (MarketEvent,
//!    LatencyStamps) 를 crossbeam 채널로 받아 온다.
//! 2. 별도 exporter task 가 loop 돌며 `Receiver::recv_timeout` 로 한 건씩 꺼내
//!    `QuestDbSink::push_bookticker` / `push_trade` 를 호출.
//! 3. batch_rows / batch_ms 에 도달하면 `maybe_flush` → TCP ILP 전송.
//! 4. 채널 drain 중에도 `batch_ms` 주기로 flush 하도록 recv_timeout 기반 tick.
//!
//! # Hot path 고려
//! - exporter 는 SUB decode 가 느려지지 않게 **별 thread/task** 에서 소비.
//! - QuestDbSink 내부에서 TCP write 는 blocking I/O → `tokio::task::spawn_blocking`
//!   으로 감싸 런타임 worker 를 막지 않는다.
//! - 연결 장애 시 `StorageError::NotConnected` 로 떨어져도 spool_dir 에 저장되므로
//!   exporter 는 에러를 `warn!` 후 계속 진행한다.
//!
//! # 종료
//! - SIGINT/SIGTERM → `cancel.cancel()` → subscriber handle 이 먼저 정리되고
//!   채널이 닫히면 exporter 가 남은 이벤트 drain 후 `force_flush` + `shutdown`.

#![deny(rust_2018_idioms)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use hft_config::AppConfig;
use hft_exchange_api::CancellationToken;
use hft_storage::{QuestDbSink, StorageError};
use hft_time::LatencyStamps;
use hft_types::MarketEvent;
use subscriber::{start as subscriber_start, InprocQueue, INPROC_QUEUE_CAPACITY};
use tracing::{error, info, warn};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// exporter recv_timeout — 이보다 빠르면 flush tick, 느리면 shutdown 지연.
const RECV_TIMEOUT_MS: u64 = 50;

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
            eprintln!("[questdb-export] second signal — aborting");
            std::process::exit(130);
        }
        eprintln!("[questdb-export] shutdown signal — draining");
        cancel.cancel();
    });
    if let Err(e) = install_result {
        warn!(error = %e, "failed to install ctrlc handler");
    }
}

/// (MarketEvent, LatencyStamps) 한 건을 sink buffer 에 push.
fn push_event(sink: &mut QuestDbSink, ev: &MarketEvent, stamps: &LatencyStamps) {
    let res = match ev {
        MarketEvent::BookTicker(b) | MarketEvent::WebBookTicker(b) => {
            sink.push_bookticker(b, stamps)
        }
        MarketEvent::Trade(t) => sink.push_trade(t, stamps),
    };
    if let Err(e) = res {
        // push 단계의 에러는 format 문제뿐 — IO 는 flush 에서 잡힘.
        warn!(error = %e, "ilp encode failed — skipping row");
    }
}

/// blocking 영역: recv → encode → maybe_flush. `spawn_blocking` 에서 호출.
fn exporter_loop(
    rx: Receiver<(MarketEvent, LatencyStamps)>,
    cancel: CancellationToken,
    mut sink: QuestDbSink,
) {
    info!(target: "questdb_export", "exporter loop started");
    loop {
        if cancel.is_cancelled() && rx.is_empty() {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(RECV_TIMEOUT_MS)) {
            Ok((ev, stamps)) => {
                push_event(&mut sink, &ev, &stamps);
                // 한 번에 많이 들어와 있을 수 있으니 connected 상태에서는 연속 drain.
                while let Ok((ev, stamps)) = rx.try_recv() {
                    push_event(&mut sink, &ev, &stamps);
                }
                match sink.maybe_flush() {
                    Ok(()) => {}
                    Err(StorageError::NotConnected) => {
                        // spool 에만 기록된 상태 — 다음 기회에 replay.
                    }
                    Err(e) => warn!(error = %e, "questdb flush error"),
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // 이벤트가 없어도 batch_ms 경계를 맞추기 위해 flush 시도.
                match sink.maybe_flush() {
                    Ok(()) => {}
                    Err(StorageError::NotConnected) => {}
                    Err(e) => warn!(error = %e, "questdb flush (idle tick) error"),
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                info!(target: "questdb_export", "inproc channel disconnected — exiting");
                break;
            }
        }
    }

    info!(target: "questdb_export", "exporter loop exiting — final flush");
    if let Err(e) = sink.shutdown() {
        warn!(error = %e, "questdb shutdown flush error");
    }
}

async fn run() -> Result<()> {
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;
    let _tele = init_telemetry(&cfg)?;

    info!(
        service = %cfg.service_name,
        ilp_addr = %cfg.questdb.ilp_addr,
        batch_rows = cfg.questdb.batch_rows,
        batch_ms = cfg.questdb.batch_ms,
        spool_dir = %cfg.questdb.spool_dir.display(),
        "questdb-export starting"
    );

    if cfg.questdb.ilp_addr.is_empty() {
        warn!(
            "questdb.ilp_addr is empty — running in spool-only mode (all writes go to spool_dir)"
        );
    }

    let sink = QuestDbSink::new(&cfg.questdb).context("QuestDbSink init failed")?;

    // SUB → inproc queue → exporter.
    let (queue, rx) = InprocQueue::bounded(INPROC_QUEUE_CAPACITY);
    let downstream = Arc::new(queue);

    let sub_handle = subscriber_start(cfg.clone(), downstream)
        .await
        .context("subscriber::start failed")?;

    install_signal_handler(sub_handle.cancel.clone());

    // exporter 는 blocking I/O (TCP write, file append) 를 수행하므로 별 thread.
    let exporter_cancel = sub_handle.cancel.child_token();
    let exporter_task = tokio::task::spawn_blocking(move || {
        exporter_loop(rx, exporter_cancel, sink);
    });

    sub_handle.join().await;

    // subscriber 가 끝난 뒤 queue 의 tx 가 drop 되면 exporter 의 recv_timeout 이
    // Disconnected 로 떨어져 자연스럽게 종료. 혹시 cancel 먼저 왔다면 drain 루프가
    // 남은 이벤트까지 삼킨 후 종료.
    if let Err(e) = exporter_task.await {
        warn!(error = ?e, "exporter task join error");
    }

    info!("questdb-export exited cleanly");
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[questdb-export] fatal: {e:#}");
            error!(error = %format!("{e:#}"), "questdb-export fatal");
            ExitCode::from(1)
        }
    }
}

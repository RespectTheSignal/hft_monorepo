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
use hft_time::{LatencyStamps, Stage};
use hft_types::MarketEvent;
use postgres::{Client, NoTls};
use subscriber::{start as subscriber_start, InprocQueue, INPROC_QUEUE_CAPACITY};
use tracing::{debug, error, info, warn};

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
    push_latency_deltas(sink, stamps);
}

/// 인접 stage delta + e2e delta 를 latency_stages 테이블에 기록한다.
///
/// 일부 stage 가 비어 있으면 해당 delta 는 조용히 skip 한다.
fn push_latency_deltas(sink: &mut QuestDbSink, stamps: &LatencyStamps) {
    const STAGE_PAIRS: &[(Stage, Stage)] = &[
        (Stage::ExchangeServer, Stage::WsReceived),
        (Stage::WsReceived, Stage::Serialized),
        (Stage::Serialized, Stage::Pushed),
        (Stage::Pushed, Stage::Published),
        (Stage::Published, Stage::Subscribed),
        (Stage::Subscribed, Stage::Consumed),
    ];

    for &(from, to) in STAGE_PAIRS {
        if let Some(nanos) = stamps.delta_ns(from, to) {
            if let Err(e) = sink.push_latency_sample(to, nanos) {
                warn!(stage = ?to, error = %e, "latency sample encode failed");
            }
        }
    }

    if let Some(e2e_ms) = stamps.end_to_end_ms() {
        if e2e_ms > 0 {
            let nanos = (e2e_ms as u64).saturating_mul(1_000_000);
            if let Err(e) = sink.push_latency_sample(Stage::EndToEnd, nanos) {
                warn!(error = %e, "e2e latency sample encode failed");
            }
        }
    }
}

/// QuestDB PG wire 로 필요한 테이블을 startup 시 보장한다.
///
/// ILP auto-create fallback 이 있으므로 실패는 warn 처리 후 계속 진행 가능하다.
fn ensure_questdb_schema(pg_addr: &str) -> Result<()> {
    let parts: Vec<&str> = pg_addr.splitn(2, ':').collect();
    let (host, port) = match parts.as_slice() {
        [host, port] => (*host, *port),
        _ => return Err(anyhow!("invalid pg_addr format: expected host:port")),
    };

    let conn_string = format!("host={host} port={port} user=admin dbname=qdb sslmode=disable");
    let mut client =
        Client::connect(&conn_string, NoTls).context("questdb PG wire connect failed")?;

    let ddl = [
        "CREATE TABLE IF NOT EXISTS bookticker (\
            symbol SYMBOL, exchange SYMBOL, \
            bid_price DOUBLE, ask_price DOUBLE, \
            bid_size DOUBLE, ask_size DOUBLE, \
            event_time_ms LONG, server_time_ms LONG, \
            ts TIMESTAMP \
         ) timestamp(ts) PARTITION BY DAY",
        "CREATE TABLE IF NOT EXISTS trade (\
            symbol SYMBOL, exchange SYMBOL, \
            price DOUBLE, size DOUBLE, \
            trade_id LONG, create_time_ms LONG, server_time_ms LONG, \
            is_internal BOOLEAN, \
            ts TIMESTAMP \
         ) timestamp(ts) PARTITION BY DAY",
        "CREATE TABLE IF NOT EXISTS latency_stages (\
            stage SYMBOL, nanos LONG, \
            ts TIMESTAMP \
         ) timestamp(ts) PARTITION BY HOUR",
    ];

    for stmt in &ddl {
        match client.simple_query(stmt) {
            Ok(_) => debug!(ddl = %stmt, "questdb DDL executed"),
            Err(e) => warn!(ddl = %stmt, error = %e, "questdb DDL failed — continuing"),
        }
    }

    info!("QuestDB schema init done");
    Ok(())
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

    if !cfg.questdb.pg_addr.is_empty() {
        match ensure_questdb_schema(&cfg.questdb.pg_addr) {
            Ok(()) => info!("QuestDB schema initialized via PG wire"),
            Err(e) => warn!(error = %e, "QuestDB DDL init failed — ILP auto-create fallback"),
        }
    } else {
        info!("questdb.pg_addr empty — skipping DDL init, relying on ILP auto-create");
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

#[cfg(test)]
mod tests {
    use super::*;
    use hft_config::QuestDbConfig;
    use hft_time::Stamp;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn rand_suffix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    fn test_sink() -> (QuestDbSink, PathBuf) {
        let spool_dir = std::env::temp_dir().join(format!(
            "questdb-export-test-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&spool_dir).expect("create spool dir");
        let sink = QuestDbSink::new(&QuestDbConfig {
            ilp_addr: String::new(),
            pg_addr: String::new(),
            batch_rows: 100,
            batch_ms: 1000,
            spool_dir: spool_dir.clone(),
        })
        .expect("test sink");
        (sink, spool_dir)
    }

    fn shutdown_and_read_lines(sink: QuestDbSink, spool_dir: &PathBuf) -> Vec<String> {
        let _ = sink.shutdown();
        let mut lines = Vec::new();
        let entries = std::fs::read_dir(spool_dir).expect("read spool dir");
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("ilp") {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("read spool file");
            lines.extend(body.lines().map(ToOwned::to_owned));
        }
        lines
    }

    #[test]
    fn latency_deltas_all_stages() {
        let (mut sink, spool_dir) = test_sink();
        let stamps = LatencyStamps {
            exchange_server: Stamp {
                wall_ms: 100,
                mono_ns: 0,
            },
            ws_received: Stamp {
                wall_ms: 110,
                mono_ns: 1_000,
            },
            serialized: Stamp {
                wall_ms: 111,
                mono_ns: 2_000,
            },
            pushed: Stamp {
                wall_ms: 112,
                mono_ns: 3_000,
            },
            published: Stamp {
                wall_ms: 113,
                mono_ns: 4_000,
            },
            subscribed: Stamp {
                wall_ms: 114,
                mono_ns: 5_000,
            },
            consumed: Stamp {
                wall_ms: 120,
                mono_ns: 6_000,
            },
        };

        push_latency_deltas(&mut sink, &stamps);

        let lines = shutdown_and_read_lines(sink, &spool_dir);
        assert_eq!(lines.len(), 7);
        assert!(lines.iter().any(|line| line.contains("stage=ws_received")));
        assert!(lines.iter().any(|line| line.contains("stage=serialized")));
        assert!(lines.iter().any(|line| line.contains("stage=pushed")));
        assert!(lines.iter().any(|line| line.contains("stage=published")));
        assert!(lines.iter().any(|line| line.contains("stage=subscribed")));
        assert!(lines.iter().any(|line| line.contains("stage=consumed")));
        assert!(lines.iter().any(|line| line.contains("stage=e2e")));
    }

    #[test]
    fn latency_deltas_partial_stamps() {
        let (mut sink, spool_dir) = test_sink();
        let stamps = LatencyStamps {
            ws_received: Stamp {
                wall_ms: 110,
                mono_ns: 1_000,
            },
            subscribed: Stamp {
                wall_ms: 114,
                mono_ns: 5_000,
            },
            consumed: Stamp {
                wall_ms: 120,
                mono_ns: 6_000,
            },
            ..LatencyStamps::new()
        };

        push_latency_deltas(&mut sink, &stamps);

        let lines = shutdown_and_read_lines(sink, &spool_dir);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("stage=consumed"));
    }

    #[test]
    fn latency_deltas_empty_stamps() {
        let (mut sink, spool_dir) = test_sink();
        push_latency_deltas(&mut sink, &LatencyStamps::new());
        let lines = shutdown_and_read_lines(sink, &spool_dir);
        assert!(lines.is_empty());
    }

    #[test]
    fn ensure_schema_invalid_addr() {
        let err = ensure_questdb_schema("not-a-hostport").expect_err("invalid addr must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid pg_addr format"));
    }
}

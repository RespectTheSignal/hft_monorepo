//! Executor → Collector feedback subscriber (Phase 2).
//!
//! Counterpart to `executor::fill_publisher`. Connects to the executor's
//! ZMQ PUB on `ipc:///tmp/flipster_kattpish_fill.sock` (same-instance
//! Unix domain socket, override with `FILL_SUB_ADDR`), parses
//! `pairs_core::ExecutorEvent` frames, and re-emits them on a tokio
//! broadcast channel so any number of in-process consumers (Phase 3+
//! coordinator, ILP writer, dashboards) can subscribe.
//!
//! Phase 2 keeps it minimal: log every fill / abort and (optionally) write
//! a row to QuestDB `position_log` with `source="live_executor"` so Grafana
//! can plot live fills next to paper variants.

use anyhow::Result;
use pairs_core::ExecutorEvent;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{info, warn};

const SUB_ADDR_DEFAULT: &str = "ipc:///tmp/flipster_kattpish_fill.sock";

/// Spawn the subscriber thread and return a broadcast receiver factory.
///
/// Returns the `broadcast::Sender` so the main task can `.subscribe()` for
/// each consumer (coordinator, ILP writer, ...). The subscriber thread runs
/// until the process exits; ZMQ recv errors are logged but never fatal.
pub fn spawn() -> broadcast::Sender<ExecutorEvent> {
    let (tx, _) = broadcast::channel::<ExecutorEvent>(4096);
    let tx_thread = tx.clone();
    std::thread::spawn(move || {
        if let Err(e) = run_blocking(tx_thread) {
            warn!(error = %e, "fill_subscriber: terminated");
        }
    });
    tx
}

fn run_blocking(tx: broadcast::Sender<ExecutorEvent>) -> Result<()> {
    let addr = std::env::var("FILL_SUB_ADDR").unwrap_or_else(|_| SUB_ADDR_DEFAULT.to_string());
    let ctx = zmq::Context::new();
    let socket = ctx.socket(zmq::SUB)?;
    socket.set_rcvhwm(100_000)?;
    socket.set_subscribe(b"")?;
    // Connect (vs bind) so the executor binds and we connect — mirrors the
    // collector→executor direction where the publisher binds.
    socket.connect(&addr)?;
    info!(addr = %addr, "fill_subscriber: SUB connected");

    let mut received: u64 = 0;
    let mut bad_json: u64 = 0;
    loop {
        let msg = match socket.recv_bytes(0) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "fill_subscriber: recv error");
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        received += 1;
        let event: ExecutorEvent = match serde_json::from_slice(&msg) {
            Ok(e) => e,
            Err(e) => {
                bad_json += 1;
                if bad_json <= 5 || bad_json % 1000 == 0 {
                    warn!(error = %e, n = bad_json, "fill_subscriber: bad JSON");
                }
                continue;
            }
        };
        match &event {
            ExecutorEvent::Fill(f) => info!(
                account_id = %f.account_id,
                base = %f.base,
                action = %f.action,
                side = %f.side,
                size_usd = f.size_usd,
                f_price = f.flipster_price,
                g_price = f.gate_price,
                f_slip_bp = f.flipster_slip_bp,
                lag_ms = f.signal_lag_ms,
                "[fill_sub] FILL"
            ),
            ExecutorEvent::Abort(a) => info!(
                account_id = %a.account_id,
                base = %a.base,
                action = %a.action,
                reason = %a.reason,
                "[fill_sub] ABORT"
            ),
        }
        // Best-effort fan-out to in-process consumers; lag is non-fatal here
        // because fills/aborts are valuable but not on the critical path.
        let _ = tx.send(event);
        let _ = received; // silence unused if logging level cuts info!
    }
}

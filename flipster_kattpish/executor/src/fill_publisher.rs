//! ZMQ PUB for executor → collector feedback.
//!
//! Mirrors the design of `collector::signal_publisher` (collector→executor
//! TradeSignal feed) but in the opposite direction: this side emits actual
//! fills (with measured slippage) and aborts that the executor decided to
//! reject. The collector's `fill_subscriber` consumes the same envelope.
//!
//! Bind address defaults to `ipc:///tmp/flipster_kattpish_fill.sock`
//! (same-instance traffic uses Unix domain sockets); override with
//! `FILL_PUB_ADDR` — e.g. `tcp://0.0.0.0:7501` for cross-host runs.
//! Wire format is the JSON serialization of `pairs_core::ExecutorEvent`
//! (tagged union: `event="fill" | "abort"`).
//!
//! Non-fatal: bind failures and send errors are logged but never propagated
//! — execution must not be blocked waiting on a slow collector.

use std::sync::OnceLock;

use anyhow::Result;
use chrono::{DateTime, Utc};
use pairs_core::{AbortReport, ExecutorEvent, FillReport};
use tracing::{info, warn};

struct Publisher {
    socket: std::sync::Mutex<zmq::Socket>,
}

static PUBLISHER: OnceLock<Publisher> = OnceLock::new();

const DEFAULT_ADDR: &str = "ipc:///tmp/flipster_kattpish_fill.sock";

/// Initialize the global publisher. Idempotent — second call is a no-op.
pub fn init() -> Result<()> {
    if PUBLISHER.get().is_some() {
        return Ok(());
    }
    let addr = std::env::var("FILL_PUB_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let ctx = zmq::Context::new();
    let socket = ctx.socket(zmq::PUB)?;
    socket.set_sndhwm(10_000)?;
    socket.set_linger(100)?;
    socket.bind(&addr)?;
    info!(addr = %addr, "fill_publisher: PUB bound");
    let _ = PUBLISHER.set(Publisher {
        socket: std::sync::Mutex::new(socket),
    });
    Ok(())
}

fn send_event(ev: &ExecutorEvent) {
    let pub_ref = match PUBLISHER.get() {
        Some(p) => p,
        None => return,
    };
    let payload = match serde_json::to_vec(ev) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "fill_publisher: serialize failed");
            return;
        }
    };
    let guard = match pub_ref.socket.lock() {
        Ok(g) => g,
        Err(e) => {
            warn!(error = %e, "fill_publisher: poisoned mutex");
            return;
        }
    };
    if let Err(e) = guard.send(&payload, zmq::DONTWAIT) {
        warn!(error = %e, "fill_publisher: send failed");
    }
}

#[allow(clippy::too_many_arguments)]
pub fn publish_fill(
    account_id: &str,
    position_id: i64,
    base: &str,
    action: &str,
    side: &str,
    size_usd: f64,
    flipster_price: f64,
    gate_price: f64,
    flipster_slip_bp: f64,
    gate_slip_bp: f64,
    paper_flipster_price: f64,
    paper_gate_price: f64,
    signal_lag_ms: i64,
    ts: DateTime<Utc>,
) {
    let ev = ExecutorEvent::Fill(FillReport {
        account_id: account_id.to_string(),
        position_id,
        base: base.to_string(),
        action: action.to_string(),
        side: side.to_string(),
        size_usd,
        flipster_price,
        gate_price,
        flipster_slip_bp,
        gate_slip_bp,
        paper_flipster_price,
        paper_gate_price,
        signal_lag_ms,
        timestamp: ts.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    });
    send_event(&ev);
}

pub fn publish_abort(
    account_id: &str,
    position_id: i64,
    base: &str,
    action: &str,
    reason: &str,
    paper_flipster_price: f64,
    paper_gate_price: f64,
    ts: DateTime<Utc>,
) {
    let ev = ExecutorEvent::Abort(AbortReport {
        account_id: account_id.to_string(),
        position_id,
        base: base.to_string(),
        action: action.to_string(),
        reason: reason.to_string(),
        paper_flipster_price,
        paper_gate_price,
        timestamp: ts.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    });
    send_event(&ev);
}

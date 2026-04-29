//! ZMQ PUB for trade_signal events so live executors can receive them
//! in-process (<10ms) instead of polling QuestDB (1-2s ILP commit lag).
//!
//! Bind address defaults to `tcp://127.0.0.1:7500`; override with
//! `SIGNAL_PUB_ADDR`. Wire format is a single-frame UTF-8 JSON object:
//!
//! ```json
//! {"account_id":"T04_es35","base":"BTC","action":"entry",
//!  "side":"long","size_usd":10.0,"flipster_price":12345.6,
//!  "gate_price":12346.1,"position_id":12345,
//!  "timestamp":"2026-04-24T12:34:56.789012Z"}
//! ```
//!
//! Non-fatal if the bind fails or a send errors — paper bot must never be
//! blocked by the publisher.
//!
//! The wire-format struct is `pairs_core::TradeSignal`. We keep the manual
//! string-formatted JSON in `publish` so the bytes on the wire are
//! byte-for-byte identical to the previous build (existing live executors
//! parse the exact field order); the shared struct is what consumers
//! deserialize into.

use std::sync::OnceLock;

use anyhow::Result;
use chrono::{DateTime, Utc};
use tracing::{info, warn};

pub use pairs_core::{SignalAction, SignalSide, TradeSignal};

/// Thread-safe PUB socket. Initialized on first call to `init` and reused.
/// zmq::Socket is not Send, so we keep it in a Mutex<Option<_>> and hold it
/// only for the duration of a single `send`.
struct Publisher {
    socket: std::sync::Mutex<zmq::Socket>,
}

static PUBLISHER: OnceLock<Publisher> = OnceLock::new();

/// Initialize the global publisher. Safe to call more than once — subsequent
/// calls are no-ops. Returns an error only if the first bind fails.
pub fn init() -> Result<()> {
    if PUBLISHER.get().is_some() {
        return Ok(());
    }
    let addr = std::env::var("SIGNAL_PUB_ADDR")
        .unwrap_or_else(|_| "tcp://127.0.0.1:7500".to_string());
    let ctx = zmq::Context::new();
    let socket = ctx.socket(zmq::PUB)?;
    socket.set_sndhwm(10_000)?;
    // Short linger so the process can exit promptly even with pending frames.
    socket.set_linger(100)?;
    socket.bind(&addr)?;
    info!(addr = %addr, "signal_publisher: PUB bound");
    let _ = PUBLISHER.set(Publisher {
        socket: std::sync::Mutex::new(socket),
    });
    Ok(())
}

/// Publish a trade_signal event. Non-fatal on error (logs + returns).
#[allow(clippy::too_many_arguments)]
pub fn publish(
    account_id: &str,
    base: &str,
    action: &str,
    flipster_side: &str,
    size_usd: f64,
    flipster_price: f64,
    gate_price: f64,
    position_id: u64,
    ts: DateTime<Utc>,
) {
    let pub_ref = match PUBLISHER.get() {
        Some(p) => p,
        None => return,
    };
    let payload = format!(
        "{{\"account_id\":\"{}\",\"base\":\"{}\",\"action\":\"{}\",\"side\":\"{}\",\"size_usd\":{},\"flipster_price\":{},\"gate_price\":{},\"position_id\":{},\"timestamp\":\"{}\"}}",
        account_id, base, action, flipster_side, size_usd, flipster_price, gate_price, position_id,
        ts.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
    );
    // DONTWAIT so we never block a strategy thread if subscribers are slow.
    let guard = match pub_ref.socket.lock() {
        Ok(g) => g,
        Err(e) => {
            warn!(error = %e, "signal_publisher: poisoned mutex");
            return;
        }
    };
    if let Err(e) = guard.send(payload.as_bytes(), zmq::DONTWAIT) {
        warn!(error = %e, "signal_publisher: send failed");
    }
}

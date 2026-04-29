//! ZMQ subscriber for the paper bot's trade_signal PUB feed.
//!
//! Wire format: single-frame UTF-8 JSON object. See
//! `collector/src/signal_publisher.rs` for the publisher side. Frames whose
//! `account_id` doesn't match the configured variant are dropped without
//! deserializing further.
//!
//! The blocking `recv` loop runs on a dedicated thread; parsed events are
//! pushed onto an `mpsc::UnboundedSender<TradeSignal>` for the executor's
//! tokio dispatcher.

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

const SUB_ADDR_DEFAULT: &str = "tcp://127.0.0.1:7500";

/// Paper-bot trade_signal payload. Field names match the JSON wire format
/// emitted by `signal_publisher::publish` and consumed by the Python
/// executor's `_dispatch_event`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeSignal {
    pub account_id: String,
    pub base: String,
    pub action: String,   // "entry" | "exit"
    pub side: String,     // "long" | "short" — Flipster side
    pub size_usd: f64,
    pub flipster_price: f64,
    pub gate_price: f64,
    pub position_id: i64,
    pub timestamp: String, // ISO8601 with microseconds
}

/// Blocking subscriber loop. Intended to run on `tokio::task::spawn_blocking`.
/// Returns only on fatal error (socket creation/bind failure).
pub fn run_subscriber(variant: &str, tx: UnboundedSender<TradeSignal>) {
    let addr = std::env::var("SIGNAL_SUB_ADDR")
        .unwrap_or_else(|_| SUB_ADDR_DEFAULT.to_string());

    let ctx = zmq::Context::new();
    let socket = match ctx.socket(zmq::SUB) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "zmq_sub: failed to create SUB socket");
            return;
        }
    };

    if let Err(e) = socket.set_rcvhwm(100_000) {
        warn!(error = %e, "zmq_sub: set_rcvhwm failed");
    }
    if let Err(e) = socket.set_subscribe(b"") {
        warn!(error = %e, "zmq_sub: subscribe(empty) failed");
        return;
    }
    if let Err(e) = socket.connect(&addr) {
        warn!(error = %e, addr = %addr, "zmq_sub: connect failed");
        return;
    }
    info!(addr = %addr, variant = %variant, "zmq_sub: connected");

    let mut received: u64 = 0;
    let mut matched: u64 = 0;
    let mut bad_json: u64 = 0;

    loop {
        let msg = match socket.recv_bytes(0) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "zmq_sub: recv error");
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
        };
        received += 1;

        // Cheap pre-filter: only deserialize if this looks like our variant.
        // Avoids JSON parsing overhead on the ~30 other paper variants
        // sharing the same PUB socket.
        if !contains_variant(&msg, variant) {
            continue;
        }

        let event: TradeSignal = match serde_json::from_slice(&msg) {
            Ok(e) => e,
            Err(e) => {
                bad_json += 1;
                if bad_json <= 5 || bad_json % 1000 == 0 {
                    warn!(error = %e, n = bad_json, "zmq_sub: bad JSON frame");
                }
                continue;
            }
        };
        if event.account_id != variant {
            continue;
        }
        matched += 1;

        if tx.send(event).is_err() {
            // Receiver gone — caller is shutting down.
            info!(received, matched, "zmq_sub: receiver dropped, exiting");
            return;
        }
    }
}

/// Sub-string match for `account_id`. We look for `"account_id":"<variant>"`
/// but tolerate the publisher's exact spacing. Used as a cheap pre-filter
/// before full JSON parse.
fn contains_variant(msg: &[u8], variant: &str) -> bool {
    let needle = format!("\"account_id\":\"{}\"", variant);
    msg.windows(needle.len()).any(|w| w == needle.as_bytes())
}

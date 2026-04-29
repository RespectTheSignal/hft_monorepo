//! ZMQ trade-signal wire format.
//!
//! Single source of truth for the JSON contract between
//! `collector::signal_publisher` (PUB) and `executor::zmq_sub` (SUB). Field
//! shape and names match the existing wire format byte-for-byte so a rolling
//! upgrade does not require touching consumers.
//!
//! Wire format (single-frame UTF-8):
//! ```json
//! {"account_id":"T04_es35","base":"BTC","action":"entry","side":"long",
//!  "size_usd":10.0,"flipster_price":12345.6,"gate_price":12346.1,
//!  "position_id":12345,"timestamp":"2026-04-24T12:34:56.789012Z"}
//! ```

use serde::{Deserialize, Serialize};

/// Entry vs exit. The wire format uses lowercase string literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalAction {
    Entry,
    Exit,
}

impl SignalAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Entry => "entry",
            Self::Exit => "exit",
        }
    }
}

/// Flipster side: `Long` means we are long the Flipster leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalSide {
    Long,
    Short,
}

impl SignalSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Long => "long",
            Self::Short => "short",
        }
    }
    pub fn from_i32(side: i32) -> Self {
        if side >= 0 {
            Self::Long
        } else {
            Self::Short
        }
    }
}

/// Trade signal payload.
///
/// Action / side are kept as strings (matching the previously hand-rolled
/// JSON in `signal_publisher::publish`) so existing Python and Rust
/// consumers parse the same bytes. Strongly-typed callers can use
/// [`SignalAction`] / [`SignalSide`] and convert via `.as_str()`.
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
    pub timestamp: String, // ISO8601 micros, e.g. "2026-04-24T12:34:56.789012Z"
}

// -----------------------------------------------------------------------------
// executor → collector feedback channel (Phase 2)
// -----------------------------------------------------------------------------
//
// Default ZMQ socket: `tcp://127.0.0.1:7501` (override with `FILL_PUB_ADDR` /
// `FILL_SUB_ADDR`). Single-frame UTF-8 JSON; envelope tag in `event` so a
// single subscriber socket can fan out to two handlers without a second port.

/// Live execution event. Wraps either a fill report or an abort report so
/// both flow through one PUB socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ExecutorEvent {
    Fill(FillReport),
    Abort(AbortReport),
}

/// Successful entry or exit fill on the executor side. Echoes back enough of
/// the original signal that the collector can stitch it to the paper
/// position by `(account_id, position_id)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillReport {
    pub account_id: String,
    pub position_id: i64,
    pub base: String,
    pub action: String, // "entry" | "exit"
    pub side: String,   // "long" | "short" (Flipster side)
    pub size_usd: f64,
    /// Actual Flipster fill avg price.
    pub flipster_price: f64,
    /// Actual Gate hedge fill avg price (`0.0` if `--flipster-only`).
    pub gate_price: f64,
    /// Measured slippage vs the paper signal price, in bp. Positive = adverse.
    pub flipster_slip_bp: f64,
    pub gate_slip_bp: f64,
    /// Paper signal prices, stored verbatim so the collector can recompute
    /// edge / fee accounting without re-querying.
    pub paper_flipster_price: f64,
    pub paper_gate_price: f64,
    /// Time from signal emission to fill, in ms (executor-side latency).
    pub signal_lag_ms: i64,
    pub timestamp: String,
}

/// Entry was refused at the executor stage. Used for filters that the
/// collector should learn from (so the paper/live divergence can be
/// quantified without scraping logs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortReport {
    pub account_id: String,
    pub position_id: i64,
    pub base: String,
    pub action: String, // "entry" | "exit"
    /// Free-form short tag. Examples: "edge_recheck_adverse", "wide_spread",
    /// "hedge_mismatch", "stale_signal", "invalid_price", "chop_detect".
    pub reason: String,
    pub paper_flipster_price: f64,
    pub paper_gate_price: f64,
    pub timestamp: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_json() {
        let s = TradeSignal {
            account_id: "T04_es35".into(),
            base: "BTC".into(),
            action: "entry".into(),
            side: "long".into(),
            size_usd: 10.0,
            flipster_price: 12345.6,
            gate_price: 12346.1,
            position_id: 12345,
            timestamp: "2026-04-24T12:34:56.789012Z".into(),
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"account_id\":\"T04_es35\""));
        let back: TradeSignal = serde_json::from_str(&j).unwrap();
        assert_eq!(back.base, "BTC");
        assert_eq!(back.position_id, 12345);
    }

    #[test]
    fn executor_event_envelope() {
        let fill = ExecutorEvent::Fill(FillReport {
            account_id: "BINANCE_LEAD_v1".into(),
            position_id: 42,
            base: "BTC".into(),
            action: "entry".into(),
            side: "long".into(),
            size_usd: 80.0,
            flipster_price: 100.05,
            gate_price: 0.0,
            flipster_slip_bp: 1.5,
            gate_slip_bp: 0.0,
            paper_flipster_price: 100.0,
            paper_gate_price: 100.02,
            signal_lag_ms: 23,
            timestamp: "2026-04-29T07:48:04Z".into(),
        });
        let j = serde_json::to_string(&fill).unwrap();
        assert!(j.contains("\"event\":\"fill\""));
        let back: ExecutorEvent = serde_json::from_str(&j).unwrap();
        match back {
            ExecutorEvent::Fill(f) => assert_eq!(f.position_id, 42),
            _ => panic!("expected Fill"),
        }
    }

    #[test]
    fn abort_envelope() {
        let abort = ExecutorEvent::Abort(AbortReport {
            account_id: "BINANCE_LEAD_v1".into(),
            position_id: 7,
            base: "MOVR".into(),
            action: "entry".into(),
            reason: "wide_spread".into(),
            paper_flipster_price: 2.46,
            paper_gate_price: 2.465,
            timestamp: "2026-04-29T07:48:04Z".into(),
        });
        let j = serde_json::to_string(&abort).unwrap();
        assert!(j.contains("\"event\":\"abort\""));
        assert!(j.contains("\"reason\":\"wide_spread\""));
    }
}

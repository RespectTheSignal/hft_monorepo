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
}

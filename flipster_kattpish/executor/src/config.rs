//! Constants ported from the head of `scripts/live_executor.py`.
//!
//! Only ones the Rust executor actually uses; values that exist purely to
//! reflect retired POCs (GATE_MAKER_ENTRY, GATE_LIMIT_WAIT, etc.) are
//! omitted — the Rust path will only implement the live, working modes
//! and add new variants explicitly.

#![allow(dead_code)]

pub const QUESTDB_URL: &str = "http://211.181.122.102:9000";
pub const SIGNAL_SUB_ADDR: &str = "tcp://127.0.0.1:7500";

/// Drop entry signals older than this — by then Gate has typically drifted
/// 30+ bp toward fair, leaving no edge worth crossing the book for.
pub const MAX_ENTRY_SIGNAL_AGE_MS: f64 = 500.0;

/// If a recent signal on the same base was the OPPOSITE side within this
/// window, skip — that's a zigzag/chop pattern, not a clean trend, and
/// momentum-follow loses on those (PRL-style).
pub const CHOP_DETECT_WINDOW_S: f64 = 15.0;

/// Pre-IOC slip cap. Compare current Gate book takable price (BID for SHORT,
/// ASK for LONG) to paper g_price; if it has moved more than this many bp
/// adverse, abort the entry (Flipster reduce-only revert).
pub const MAX_GATE_PRE_IOC_SLIP_BP: f64 = 12.0;

/// Skip entry if Flipster mid has already moved >= this bp in the signal's
/// predicted direction (i.e. reversion already happened).
pub const EDGE_RECHECK_MAX_ADVERSE_BP: f64 = 3.0;

/// Filter weak signals (current default disabled).
pub const MIN_ENTRY_SIGNAL_SPREAD_BP: f64 = 0.0;

/// Flipster LIMIT-IOC entry: place LIMIT at signal flipster_price, wait
/// briefly, cancel if not filled. Saves the avg ~8 bp slip we measured on
/// MARKET entries — at the cost of trading less often.
pub const FLIPSTER_LIMIT_ENTRY: bool = true;
pub const FLIP_LIMIT_WAIT_S: f64 = 0.4;

/// Take-profit (currently effectively disabled — `TP_MIN_PEAK_BP` huge).
pub const TP_POLL_SEC: f64 = 1.0;
pub const TP_MIN_ENTRY_SPREAD_BP: f64 = 8.0;
pub const TP_MIN_PEAK_BP: f64 = 9999.0;
pub const TP_DRAWDOWN_BP: f64 = 3.0;
pub const TP_MIN_HOLD_SEC: f64 = 5.0;
pub const TP_MAX_MID_AGE_SEC: f64 = 5.0;

/// Per-leg fee (rebate-aware): Flipster taker 0.425 bp + Gate taker 0.45 bp
/// → 0.875 bp/side. Round-trip fee = 4 × this on notional.
pub const TAKER_FEE_FRAC: f64 = 0.0000875;

//! Cross-strategy signal router (Phase 4 skeleton).
//!
//! Sits between every strategy and `signal_publisher` so portfolio-level
//! rules (rate-limit budget, global leverage cap, slot reservation, dedup,
//! cross-strategy net-position checks) have a single chokepoint to live in.
//! Phase 4 ships as **pass-through** — `route_signal` forwards every call
//! to `signal_publisher::publish` unchanged. The structural change is what
//! matters: every new rule plugs in here, none of the strategies need to
//! know about it.
//!
//! Strategies should never call `signal_publisher::publish` directly going
//! forward; route through this module so future rules cannot be bypassed.

use chrono::{DateTime, Utc};
use tracing::trace;

use crate::signal_publisher;

/// Route an entry/exit signal. Pass-through in Phase 4. Future rules:
///
/// - Rate-limit budget per account_id (Cloudflare 1015 protection).
/// - Global open-leverage cap across all strategies.
/// - Cross-strategy net-position check (e.g. strategy A long BTC while
///   strategy B short BTC → cancel both, save fees).
/// - Slot reservation against Flipster's per-symbol position-slot limit.
/// - Sticky dedup: drop exact duplicates within a short window.
/// - Kill-switch: refuse all entries when daily loss exceeds threshold.
///
/// Each rule should return a `Decision` (forward / drop / modify) and the
/// router applies them in priority order. For now the only decision is
/// "forward".
#[allow(clippy::too_many_arguments)]
pub fn route_signal(
    account_id: &str,
    base: &str,
    action: &str,        // "entry" | "exit"
    flipster_side: &str, // "long" | "short"
    size_usd: f64,
    flipster_price: f64,
    gate_price: f64,
    position_id: u64,
    ts: DateTime<Utc>,
) {
    trace!(
        account_id,
        base,
        action,
        flipster_side,
        size_usd,
        position_id,
        "[coordinator] routing"
    );
    signal_publisher::publish(
        account_id,
        base,
        action,
        flipster_side,
        size_usd,
        flipster_price,
        gate_price,
        position_id,
        ts,
    );
}

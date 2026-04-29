//! Cross-strategy signal router.
//!
//! Phase 4 introduced this as a pass-through chokepoint. Phase 3b makes it
//! the sole owner of the per-trade decision filters that used to live in
//! the executor:
//!
//! - **chop_detect**: drop entries whose base traded the opposite side
//!   within `CHOP_WINDOW_MS`. Cross-strategy by design — keyed by base
//!   only — so e.g. pairs short BTC + 3 s later gate_lead long BTC are
//!   recognised as the zigzag they are.
//! - **dyn_filter**: `pairs_core::SymbolStatsStore::check_entry` — wide
//!   spread / negative recent PnL cooldown / weak paper signal.
//! - **adjusted_size**: `SymbolStatsStore::adjusted_size` — multiplicative
//!   scale based on observed slippage and PnL.
//!
//! Exits never block (we cannot refuse to close an open position).
//!
//! Freshness gates that *should not* live here — `stale_signal_age` and
//! the executor-side mid recheck — remain in the executor because they
//! depend on network/queue lag between publish and order placement, not
//! on strategy state.
//!
//! Env knobs (failsafes for rollout, both default to "filters on at the
//! collector / off at the executor"):
//!
//! - `COORDINATOR_FILTERS=0` → coordinator becomes pure pass-through. Use
//!   if a regression slips through and you want to fall back to the old
//!   executor-decides world quickly.
//! - `CHOP_WINDOW_MS=<ms>` → override the chop window (default 15000 ms).

use std::sync::OnceLock;
use std::time::Instant;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use tracing::{debug, info, trace};

use crate::signal_publisher;
use pairs_core::symbol_stats::{FilterDecision, SymbolStatsStore};
use std::sync::Arc;

const CHOP_WINDOW_MS_DEFAULT: u64 = 15_000;

struct Coordinator {
    sym_stats: Arc<SymbolStatsStore>,
    /// (base) → (last entry side as +1/-1, instant). Cross-strategy.
    last_entry: DashMap<String, (i32, Instant)>,
    enabled: bool,
    chop_window_ms: u64,
}

static COORDINATOR: OnceLock<Coordinator> = OnceLock::new();

/// Initialise the coordinator with the live-stats store. Idempotent —
/// second call is a no-op. Strategies must not start emitting signals
/// before this has been called; if they do, `route_signal` falls back to
/// pure pass-through (which is the Phase 4 behaviour).
pub fn init(sym_stats: Arc<SymbolStatsStore>) {
    if COORDINATOR.get().is_some() {
        return;
    }
    let enabled = std::env::var("COORDINATOR_FILTERS").as_deref() != Ok("0");
    let chop_window_ms = std::env::var("CHOP_WINDOW_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(CHOP_WINDOW_MS_DEFAULT);
    info!(
        enabled,
        chop_window_ms,
        "coordinator: initialised"
    );
    let _ = COORDINATOR.set(Coordinator {
        sym_stats,
        last_entry: DashMap::new(),
        enabled,
        chop_window_ms,
    });
}

/// Route an entry/exit signal. Applies filters when enabled; pure
/// pass-through otherwise. Returns silently — strategies don't need to
/// know whether the signal was forwarded or filtered (the coordinator
/// logs the outcome for visibility).
///
/// `flipster_spread_bp` is the live Flipster bid-ask spread (bp) at
/// decision time. Pass `None` if the strategy genuinely can't compute it
/// — the wide-spread filter is the only one that depends on it and
/// `None` makes that branch a no-op.
#[allow(clippy::too_many_arguments)]
pub fn route_signal(
    account_id: &str,
    base: &str,
    action: &str,
    flipster_side: &str,
    size_usd: f64,
    flipster_price: f64,
    gate_price: f64,
    position_id: u64,
    ts: DateTime<Utc>,
    flipster_spread_bp: Option<f64>,
) {
    let coord = COORDINATOR.get();
    let Some(coord) = coord else {
        // init() not called — pure pass-through. Defensive only.
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
        return;
    };

    // Exits and disabled-coordinator: forward as-is.
    if !coord.enabled || action != "entry" {
        trace!(account_id, base, action, "[coord] passthrough");
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
        return;
    }

    let side_int = if flipster_side == "long" { 1 } else { -1 };
    let now_inst = Instant::now();

    // 1. Chop detect (cross-strategy). Drop if opposite side fired within
    //    the chop window; allow same-side or expired entries through.
    if let Some(prev) = coord.last_entry.get(base) {
        let (prev_side, prev_ts) = *prev;
        let elapsed_ms = now_inst.duration_since(prev_ts).as_millis() as u64;
        if prev_side != side_int && elapsed_ms < coord.chop_window_ms {
            info!(
                account_id,
                base,
                side = flipster_side,
                prev_side = if prev_side > 0 { "long" } else { "short" },
                elapsed_ms,
                "[coord] skip chop"
            );
            return;
        }
    }

    // 2. Symbol-stats filter: wide spread / negative cooldown / weak paper.
    match coord.sym_stats.check_entry(base, flipster_spread_bp) {
        FilterDecision::Skip(reason) => {
            info!(
                account_id,
                base,
                reason = reason.as_str(),
                "[coord] skip dyn_filter"
            );
            return;
        }
        FilterDecision::Probe => {
            debug!(account_id, base, "[coord] cooldown probe");
        }
        FilterDecision::Pass => {}
    }

    // 3. Adjusted size — clamp to [SCALE_MIN, SCALE_MAX] of caller's size.
    let adj_size = coord.sym_stats.adjusted_size(base, size_usd);
    if (adj_size - size_usd).abs() > 1e-9 {
        debug!(
            account_id,
            base,
            requested = size_usd,
            adjusted = adj_size,
            "[coord] sized"
        );
    }

    // 4. Record entry side for future chop checks.
    coord
        .last_entry
        .insert(base.to_string(), (side_int, now_inst));

    signal_publisher::publish(
        account_id,
        base,
        "entry",
        flipster_side,
        adj_size,
        flipster_price,
        gate_price,
        position_id,
        ts,
    );
}

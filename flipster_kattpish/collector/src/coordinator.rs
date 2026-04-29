//! Cross-strategy signal router — sole owner of every per-entry
//! decision rule.
//!
//! Filters applied (in order, all on entries; exits forward as-is):
//!
//! - **chop_detect**: drop if the same base just fired the opposite side
//!   within `CHOP_WINDOW_MS`. Cross-strategy on purpose — pairs short
//!   BTC + 3 s later gate_lead long BTC is the zigzag we want to catch.
//! - **min_spread**: drop if the publish-time `|flipster - gate|` cross-
//!   venue spread is smaller than `MIN_ENTRY_SIGNAL_SPREAD_BP`. Was
//!   `MIN_ENTRY_SIGNAL_SPREAD_BP` in the executor.
//! - **dyn_filter**: `pairs_core::SymbolStatsStore::check_entry` — wide
//!   own-spread / negative recent PnL cooldown / weak paper signal.
//! - **adjusted_size**: `SymbolStatsStore::adjusted_size` — slippage- and
//!   PnL-aware multiplicative scaling on the strategy's requested size.
//!
//! Freshness — i.e. "did the signal sit in queue too long, did the mid
//! drift adversely between publish and order placement" — is the
//! executor's job (it knows network/queue lag); coordinator does not
//! gate on freshness.
//!
//! `route_signal` takes a `SignalQuotes` carrying the freshest BBO at
//! decision time. The forwarded `TradeSignal` includes those bid/ask
//! values so the executor places LIMIT orders without an extra QuestDB
//! round-trip (~50–200 ms saved per entry).
//!
//! Env knobs (rollout failsafes):
//!
//! - `COORDINATOR_FILTERS=0` → pure pass-through (no chop, no dyn, no
//!   sizing, no min_spread).
//! - `CHOP_WINDOW_MS=<ms>` → override the chop window (default 15000).
//! - `MIN_ENTRY_SIGNAL_SPREAD_BP=<bp>` → override min cross-venue spread
//!   (default 0.0 = off, matches the executor's previous default).

use std::sync::OnceLock;
use std::time::Instant;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use tracing::{debug, info, trace};

use crate::signal_publisher::{self, SignalQuotes};
use pairs_core::symbol_stats::{FilterDecision, SymbolStatsStore};
use std::sync::Arc;

const CHOP_WINDOW_MS_DEFAULT: u64 = 15_000;
const MIN_ENTRY_SPREAD_BP_DEFAULT: f64 = 0.0;

struct Coordinator {
    sym_stats: Arc<SymbolStatsStore>,
    /// (base) → (last entry side as +1/-1, instant). Cross-strategy.
    last_entry: DashMap<String, (i32, Instant)>,
    enabled: bool,
    chop_window_ms: u64,
    min_entry_spread_bp: f64,
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
    let min_entry_spread_bp = std::env::var("MIN_ENTRY_SIGNAL_SPREAD_BP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(MIN_ENTRY_SPREAD_BP_DEFAULT);
    info!(
        enabled,
        chop_window_ms,
        min_entry_spread_bp,
        "coordinator: initialised"
    );
    let _ = COORDINATOR.set(Coordinator {
        sym_stats,
        last_entry: DashMap::new(),
        enabled,
        chop_window_ms,
        min_entry_spread_bp,
    });
}

/// Route an entry/exit signal. Applies filters when enabled; pure
/// pass-through otherwise. Returns silently — strategies don't need to
/// know whether the signal was forwarded or filtered (the coordinator
/// logs the outcome for visibility).
///
/// `quotes` carries the freshest BBO the strategy observed at decision
/// time. They feed two places:
///   - the `min_entry_spread_bp` filter (uses |flipster_mid − gate_mid|),
///   - the published `TradeSignal` itself, so the executor doesn't need
///     to re-fetch BBO from QuestDB before sending a LIMIT order.
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
    quotes: SignalQuotes,
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
            quotes,
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
            quotes,
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

    // 2. Min cross-venue spread filter (was MIN_ENTRY_SIGNAL_SPREAD_BP in
    //    executor.rs). Skip if |flipster_mid - gate_mid| / gate_mid is
    //    smaller than the configured threshold.
    if coord.min_entry_spread_bp > 0.0 && gate_price > 0.0 && flipster_price > 0.0 {
        let sig_sprd_bp = (flipster_price - gate_price).abs() / gate_price * 1e4;
        if sig_sprd_bp < coord.min_entry_spread_bp {
            info!(
                account_id,
                base,
                sig_sprd_bp = format!("{:.2}", sig_sprd_bp),
                threshold = coord.min_entry_spread_bp,
                "[coord] skip weak signal (cross-venue spread)"
            );
            return;
        }
    }

    // 3. Symbol-stats filter: wide own-spread / negative cooldown /
    //    weak paper. Derive Flipster's own bid-ask spread from BBO when
    //    available (None makes the wide-spread sub-filter a no-op).
    let own_spread_bp = match (quotes.flipster_bid, quotes.flipster_ask) {
        (Some(bid), Some(ask)) if bid > 0.0 && ask > 0.0 && ask >= bid => {
            let mid = 0.5 * (bid + ask);
            if mid > 0.0 {
                Some((ask - bid) / mid * 1e4)
            } else {
                None
            }
        }
        _ => None,
    };
    match coord.sym_stats.check_entry(base, own_spread_bp) {
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

    // 4. Adjusted size — clamp to [SCALE_MIN, SCALE_MAX] of caller's size.
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

    // 5. Record entry side for future chop checks.
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
        quotes,
    );
}

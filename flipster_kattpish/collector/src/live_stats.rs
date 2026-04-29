//! Collector-side learner for live executor fills (Phase 3a).
//!
//! Subscribes to the `fill_subscriber` broadcast (executor → collector
//! channel established in Phase 2), pairs entry/exit fills by
//! `(account_id, position_id)`, and feeds the resulting per-trade
//! `(pnl_bp, paper_bp, slip_bp_total)` triple into a
//! `pairs_core::SymbolStatsStore`.
//!
//! This duplicates the executor's existing `sym_stats` learning by design.
//! Phase 3a is a parallel-learning checkpoint: with both stores running
//! against the same fills, we can compare their evolution and validate that
//! the collector-side picture matches reality before Phase 3b moves the
//! filtering decision onto the collector.
//!
//! Disk persistence path is intentionally separate from the executor's so
//! the two never race on the same file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pairs_core::{symbol_stats::SymbolStatsStore, ExecutorEvent, FillReport};
use tokio::sync::{broadcast, Mutex};
use tracing::{info, warn};

const PERSIST_DEFAULT: &str = "/tmp/flipster_kattpish/live_stats_collector.json";

/// Cached entry-side fill data, kept until the matching exit arrives.
#[derive(Clone)]
struct PendingEntry {
    flipster_entry: f64,
    paper_flipster_entry: f64,
    flipster_slip_bp: f64,
    side: i32, // +1 long, -1 short (Flipster side)
    base: String,
}

/// Spawn the live_stats consumer task.
///
/// `events_rx` should be the receiver returned from
/// `fill_subscriber::spawn().subscribe()`. The returned `Arc<SymbolStatsStore>`
/// is the same shape as the executor's so future code can call
/// `check_entry`, `adjusted_size`, `report` from collector strategies.
pub fn spawn(mut events_rx: broadcast::Receiver<ExecutorEvent>) -> Arc<SymbolStatsStore> {
    let persist = std::env::var("LIVE_STATS_PATH")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(PERSIST_DEFAULT));
    if let Some(parent) = persist.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let store = SymbolStatsStore::new(Some(persist.clone()));

    // Open-position cache, keyed by (account_id, position_id). Bounded
    // implicitly by trade churn — we GC stale entries every minute.
    let pending: Arc<Mutex<HashMap<(String, i64), PendingEntry>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Periodic save of collector-side stats (every 30s, mirrors executor).
    {
        let store_save = store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.tick().await;
            loop {
                tick.tick().await;
                store_save.save().await;
            }
        });
    }

    // Periodic stale-pending GC. Drops entries whose exit never arrived
    // (collector restart, executor abort path mid-trade, etc.) so the map
    // doesn't grow unbounded.
    {
        let pending_gc = pending.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                let mut p = pending_gc.lock().await;
                let before = p.len();
                if before > 4096 {
                    p.clear();
                    warn!(dropped = before, "[live_stats] pending map > 4096; cleared");
                }
            }
        });
    }

    // Periodic report (every 5 min, similar to executor's DYN-REPORT).
    {
        let store_r = store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(300));
            tick.tick().await;
            loop {
                tick.tick().await;
                let report = store_r.report();
                if report.is_empty() {
                    continue;
                }
                let mut lines = String::from("[live_stats] collector-side sym_stats:\n");
                for (sym, n, avg_pnl, avg_paper, avg_slip, scale, cool) in
                    report.iter().take(25)
                {
                    lines.push_str(&format!(
                        "  {:<10} n={:<4} pnl={:+5.2}bp paper={:+5.2}bp slip={:5.2}bp scale={:.2}{}\n",
                        sym, n, avg_pnl, avg_paper, avg_slip, scale,
                        if *cool { " [COOL]" } else { "" }
                    ));
                }
                info!("{}", lines);
            }
        });
    }

    // Main consumer loop.
    let store_consume = store.clone();
    tokio::spawn(async move {
        info!(persist = %persist.display(), "[live_stats] consumer started");
        loop {
            let ev = match events_rx.recv().await {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(missed = n, "[live_stats] broadcast lagged");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    warn!("[live_stats] broadcast closed; exiting");
                    return;
                }
            };
            match ev {
                ExecutorEvent::Fill(f) => {
                    handle_fill(&store_consume, &pending, f).await;
                }
                ExecutorEvent::Abort(_) => {
                    // Phase 3a does not consume aborts. They will be useful
                    // in Phase 3b for "skipped at executor" diagnostics.
                }
            }
        }
    });

    store
}

async fn handle_fill(
    store: &Arc<SymbolStatsStore>,
    pending: &Arc<Mutex<HashMap<(String, i64), PendingEntry>>>,
    f: FillReport,
) {
    let key = (f.account_id.clone(), f.position_id);
    let side = if f.side == "long" { 1 } else { -1 };
    match f.action.as_str() {
        "entry" => {
            let mut p = pending.lock().await;
            p.insert(
                key,
                PendingEntry {
                    flipster_entry: f.flipster_price,
                    paper_flipster_entry: f.paper_flipster_price,
                    flipster_slip_bp: f.flipster_slip_bp,
                    side,
                    base: f.base.clone(),
                },
            );
        }
        "exit" => {
            let entry = {
                let mut p = pending.lock().await;
                p.remove(&key)
            };
            let Some(entry) = entry else {
                // Exit without entry — most likely a position opened before
                // the collector restarted. Skipping is correct: we can't
                // compute pnl_bp without entry price, and the executor
                // already covers this through its own sym_stats.
                return;
            };
            // Single-leg PnL on Flipster, signed by side.
            let pnl_bp = pairs_core::single_leg_pnl_bp(
                entry.flipster_entry,
                f.flipster_price,
                entry.side,
            );
            // Paper signal "what should have happened" PnL: same direction
            // applied to paper signal prices. Mirrors executor's
            // paper_bp_for_dyn computed at executor.rs:1042.
            let paper_bp = pairs_core::single_leg_pnl_bp(
                entry.paper_flipster_entry,
                f.paper_flipster_price,
                entry.side,
            );
            // Total slip = entry_slip + exit_slip, both as costs (>=0).
            let slip_bp_total =
                entry.flipster_slip_bp.max(0.0) + f.flipster_slip_bp.max(0.0);
            store.record_trade(&entry.base, pnl_bp, paper_bp, slip_bp_total);
        }
        other => {
            warn!(action = other, "[live_stats] unknown fill action");
        }
    }
}

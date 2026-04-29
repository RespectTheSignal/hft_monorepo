//! Slow-moving market-gap watcher.
//!
//! Runs as a background task: every `update_interval_secs` queries QuestDB for
//! the per-symbol mean `(flipster_mid - hedge_mid) / hedge_mid` over the last
//! `window_minutes` minutes, 1-second aligned via SAMPLE BY. Results are
//! published into a shared `Arc<RwLock<GapState>>` that strategy hot-paths can
//! read cheaply.
//!
//! Two key properties vs an in-memory EWMA:
//! 1. The baseline is computed over historical DB rows, so a single live tick
//!    cannot drag it — solves the "mean chases entry" drift that caused
//!    fee-only converged exits.
//! 2. The window is long enough (tens of minutes) that structural biases are
//!    captured, but short enough to adapt to regime shifts over the day.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{info, warn};

/// Per-symbol baseline estimates published by the watcher.
#[derive(Debug, Clone, Default)]
pub struct GapState {
    /// Mean signed gap in bp, keyed by Flipster base (e.g., "BTC", "ZEC").
    /// Positive = flipster priced above hedge venue.
    pub avg_gap_bp_by_base: HashMap<String, f64>,
    /// Mean Flipster spread in bp over the same window.
    pub avg_spread_bp_by_base: HashMap<String, f64>,
    pub updated_at_ms: i64,
    pub sample_count: usize,
}

impl GapState {
    pub fn get_avg_gap_bp(&self, base: &str) -> Option<f64> {
        self.avg_gap_bp_by_base.get(base).copied()
    }
    pub fn get_avg_spread_bp(&self, base: &str) -> Option<f64> {
        self.avg_spread_bp_by_base.get(base).copied()
    }
}

pub type SharedGapState = Arc<RwLock<GapState>>;

pub fn new_shared() -> SharedGapState {
    Arc::new(RwLock::new(GapState::default()))
}

/// Spawn the watcher as a Tokio background task. The watcher will be silent
/// (no error spam) until QuestDB has enough rows on both tables.
pub fn spawn(
    state: SharedGapState,
    questdb_http_url: String,
    hedge_table: String,
    window_minutes: i64,
    update_interval_secs: u64,
) {
    tokio::spawn(async move {
        // Initial stagger so the collector has a chance to populate some rows
        // first — otherwise the first few queries return empty datasets.
        tokio::time::sleep(Duration::from_secs(15)).await;
        let client = reqwest::Client::new();
        loop {
            match fetch_once(&client, &questdb_http_url, &hedge_table, window_minutes).await {
                Ok(new_state) => {
                    let n = new_state.avg_gap_bp_by_base.len();
                    let top = top_gaps(&new_state.avg_gap_bp_by_base, 5);
                    if let Ok(mut w) = state.write() {
                        *w = new_state;
                    }
                    info!(
                        symbols = n,
                        top = %top,
                        "market_watcher: gap snapshot refreshed"
                    );
                }
                Err(e) => {
                    warn!(error = %e, "market_watcher: refresh failed, keeping old state");
                }
            }
            tokio::time::sleep(Duration::from_secs(update_interval_secs.max(1))).await;
        }
    });
}

async fn fetch_once(
    client: &reqwest::Client,
    base_url: &str,
    hedge_table: &str,
    window_minutes: i64,
) -> Result<GapState> {
    // Flipster symbol is "BASEUSDT.PERP" in the DB, hedge is "BASEUSDT".
    // We strip the ".PERP" in flipster CTE so the join key matches gate's
    // "BASEUSDT". QuestDB rejects GROUP BY on column aliases when the SELECT
    // uses aggregates, so we rely on implicit grouping (no GROUP BY clause)
    // and strip the trailing "USDT" in Rust to get the plain base key.
    let sql = format!(
        "WITH \
         f AS (SELECT replace(symbol, '.PERP', '') stripped, \
                       last((ask_price+bid_price)/2.0) fm, \
                       last((ask_price-bid_price)/((ask_price+bid_price)/2.0)) fsp, \
                       timestamp \
               FROM flipster_bookticker \
               WHERE timestamp > dateadd('m', -{w}, now()) AND symbol LIKE '%USDT.PERP' \
               SAMPLE BY 1s), \
         g AS (SELECT replace(symbol, '_', '') normalized, \
                       last((ask_price+bid_price)/2.0) gm, \
                       timestamp \
               FROM {hedge} \
               WHERE timestamp > dateadd('m', -{w}, now()) \
               SAMPLE BY 1s) \
         SELECT f.stripped sym, \
                round(avg((f.fm - g.gm)/g.gm)*10000, 4) avg_gap_bp, \
                round(avg(f.fsp)*10000, 4) avg_spread_bp, \
                count() n \
         FROM f JOIN g ON f.stripped = g.normalized AND f.timestamp = g.timestamp",
        w = window_minutes,
        hedge = hedge_table
    );
    let url = format!("{}/exec", base_url.trim_end_matches('/'));
    let resp = client.get(&url).query(&[("query", sql)]).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("questdb {}: {}", resp.status(), resp.text().await.unwrap_or_default());
    }
    let v: serde_json::Value = resp.json().await?;
    if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
        anyhow::bail!("questdb error: {}", err);
    }
    let dataset = v
        .get("dataset")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();

    let mut gaps = HashMap::new();
    let mut spreads = HashMap::new();
    let mut total_samples = 0usize;
    for row in dataset {
        let r = row.as_array().cloned().unwrap_or_default();
        let sym = r.first().and_then(|x| x.as_str()).unwrap_or("").to_string();
        if sym.is_empty() {
            continue;
        }
        let gap_bp = r.get(1).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let spread_bp = r.get(2).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let n = r.get(3).and_then(|x| x.as_i64()).unwrap_or(0) as usize;
        // Reject implausible gaps (10% = 1000 bp) that signal broken/delisted rows
        if gap_bp.abs() >= 1000.0 || !gap_bp.is_finite() {
            continue;
        }
        if n < 10 {
            continue; // need at least 10 aligned 1s samples
        }
        // Strip trailing "USDT" to match the `base` used by try_enter_pairs.
        let base = sym
            .strip_suffix("USDT")
            .map(|s| s.to_string())
            .unwrap_or(sym.clone());
        if base.is_empty() {
            continue;
        }
        gaps.insert(base.clone(), gap_bp);
        spreads.insert(base, spread_bp);
        total_samples += n;
    }

    Ok(GapState {
        avg_gap_bp_by_base: gaps,
        avg_spread_bp_by_base: spreads,
        updated_at_ms: chrono::Utc::now().timestamp_millis(),
        sample_count: total_samples,
    })
}

fn top_gaps(map: &HashMap<String, f64>, n: usize) -> String {
    let mut v: Vec<(&String, &f64)> = map.iter().collect();
    v.sort_by(|a, b| {
        b.1.abs()
            .partial_cmp(&a.1.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    v.into_iter()
        .take(n.max(1))
        .map(|(s, g)| format!("{}={:+.2}", s, g))
        .collect::<Vec<_>>()
        .join(",")
}

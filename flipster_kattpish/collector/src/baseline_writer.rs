//! Per-base baseline aggregator (in-memory).
//!
//! Every `interval_s` (default 5 s) reads the last 30 min of
//! `binance_bookticker` and `flipster_bookticker` from QuestDB,
//! computes per-base mid-gap + per-venue avg spread, and stores the
//! result in a shared `Arc<RwLock<HashMap<base, BaselinePoint>>>`.
//!
//! The point: when `spread_revert::run()` starts, it reads the most
//! recent baseline rows from the same shared map and is ready to trade
//! immediately — no 5 min in-memory warm-up. No QuestDB hop because the
//! producer (this writer) and the only consumer (spread_revert) live in
//! the same process.
//!
//! Long window (gap): 30 min. Short window (spread): 10 min. Both are
//! averaged in SQL with one query per (venue × window).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{info, warn};

const DEFAULT_QDB_HTTP: &str = "http://211.181.122.102:9000";
const DEFAULT_INTERVAL_S: u64 = 5;
const GAP_WINDOW_MIN: i64 = 30;
const SPREAD_WINDOW_MIN: i64 = 10;
const QUERY_LIMIT_PER_TICK: usize = 5_000;

/// One row: most recent baseline for one base. `ts` lets consumers
/// detect staleness if the writer task ever stalls.
#[derive(Debug, Clone)]
pub struct BaselinePoint {
    pub ts: DateTime<Utc>,
    pub bf_avg_gap_bp: f64,
    pub flipster_avg_spread_bp: f64,
    pub binance_avg_spread_bp: f64,
}

/// Map base (e.g. "BTC") → most recent baseline. Shared between the
/// writer task and the strategy task that seeds its rolling window.
pub type SharedBaselines = Arc<RwLock<HashMap<String, BaselinePoint>>>;

/// Spawn the writer task and return the shared map handle. The map
/// starts empty; the first cycle (~`interval_s` later) populates it.
/// Callers that want to gate on "baseline ready" should poll the map
/// for a non-empty state, but in practice spread_revert just reads
/// once at startup and falls back to a cold rolling window if empty.
pub fn spawn() -> SharedBaselines {
    let qdb_http = std::env::var("BASELINE_QDB_HTTP")
        .ok()
        .or_else(|| std::env::var("QUESTDB_HTTP_URL").ok())
        .unwrap_or_else(|| DEFAULT_QDB_HTTP.to_string());
    let interval_s: u64 = std::env::var("BASELINE_INTERVAL_S")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_INTERVAL_S);
    info!(qdb_http = %qdb_http, interval_s, "baseline_writer: starting");
    let shared: SharedBaselines = Arc::new(RwLock::new(HashMap::new()));
    let writer_handle = shared.clone();
    tokio::spawn(async move {
        if let Err(e) = run_loop(writer_handle, qdb_http, interval_s).await {
            warn!(error = %e, "baseline_writer: terminated");
        }
    });
    shared
}

async fn run_loop(shared: SharedBaselines, qdb_http: String, interval_s: u64) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let mut tick = interval(Duration::from_secs(interval_s));
    loop {
        tick.tick().await;
        match compute_one_cycle(&http, &qdb_http).await {
            Ok(map) if !map.is_empty() => {
                let n = map.len();
                let mut g = shared.write().await;
                *g = map;
                info!(n_bases = n, "baseline_writer: refreshed shared baselines");
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "baseline_writer: cycle failed"),
        }
    }
}

async fn compute_one_cycle(
    http: &reqwest::Client,
    qdb_http: &str,
) -> Result<HashMap<String, BaselinePoint>> {
    let bin_long = fetch_per_base_avgs(http, qdb_http, "binance_bookticker", GAP_WINDOW_MIN).await
        .context("fetch binance long-window")?;
    let fli_long = fetch_per_base_avgs(http, qdb_http, "flipster_bookticker", GAP_WINDOW_MIN).await
        .context("fetch flipster long-window")?;
    let bin_short = fetch_per_base_avgs(http, qdb_http, "binance_bookticker", SPREAD_WINDOW_MIN).await
        .context("fetch binance short-window")?;
    let fli_short = fetch_per_base_avgs(http, qdb_http, "flipster_bookticker", SPREAD_WINDOW_MIN).await
        .context("fetch flipster short-window")?;

    let now = Utc::now();
    let mut out: HashMap<String, BaselinePoint> = HashMap::with_capacity(bin_long.len());

    for (base, bin_l) in &bin_long {
        let Some(fli_l) = fli_long.get(base) else { continue };
        if bin_l.mid <= 0.0 || fli_l.mid <= 0.0 {
            continue;
        }
        let bf_avg_gap_bp = (fli_l.mid - bin_l.mid) / bin_l.mid * 1e4;
        let bin_avg_spread_bp = bin_short.get(base).map(|v| v.spread_bp).unwrap_or(0.0);
        let fli_avg_spread_bp = fli_short.get(base).map(|v| v.spread_bp).unwrap_or(0.0);
        out.insert(
            base.clone(),
            BaselinePoint {
                ts: now,
                bf_avg_gap_bp,
                flipster_avg_spread_bp: fli_avg_spread_bp,
                binance_avg_spread_bp: bin_avg_spread_bp,
            },
        );
    }
    Ok(out)
}

#[derive(Debug, Default)]
struct VenueAvg {
    mid: f64,
    spread_bp: f64,
}

async fn fetch_per_base_avgs(
    http: &reqwest::Client,
    qdb_http: &str,
    table: &str,
    window_min: i64,
) -> Result<HashMap<String, VenueAvg>> {
    let normalize = if table == "flipster_bookticker" {
        "replace(replace(symbol, '.PERP', ''), 'USDT', '')"
    } else {
        "replace(replace(symbol, '_', ''), 'USDT', '')"
    };
    let sql = format!(
        "SELECT {norm} base, \
            avg((bid_price + ask_price)/2) mid, \
            avg(case when (bid_price+ask_price) > 0 \
                then (ask_price - bid_price) / ((bid_price+ask_price)/2) * 10000 \
                else 0 end) spread_bp \
         FROM {table} \
         WHERE timestamp > dateadd('m', -{win}, now()) \
            AND bid_price > 0 AND ask_price > 0",
        norm = normalize,
        table = table,
        win = window_min,
    );
    let url = format!("{}/exec", qdb_http.trim_end_matches('/'));
    let resp = http.get(&url).query(&[("query", &sql)]).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "QuestDB {} -> {}: {}",
            table,
            status,
            &body[..body.len().min(200)]
        ));
    }
    let body: Value = resp.json().await?;
    let dataset = body
        .get("dataset")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out: HashMap<String, VenueAvg> = HashMap::with_capacity(QUERY_LIMIT_PER_TICK / 8);
    for row in dataset {
        let arr = match row.as_array() {
            Some(a) if a.len() >= 3 => a,
            _ => continue,
        };
        let base = match arr[0].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let mid = arr[1].as_f64().unwrap_or(0.0);
        let spread_bp = arr[2].as_f64().unwrap_or(0.0);
        if !mid.is_finite() || mid <= 0.0 {
            continue;
        }
        out.insert(base, VenueAvg { mid, spread_bp });
    }
    Ok(out)
}

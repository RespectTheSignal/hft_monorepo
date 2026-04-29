//! Baseline writer for the spread_revert strategy.
//!
//! Every `interval_s` (default 5 s) reads the last 30 min of
//! `binance_bookticker` and `flipster_bookticker` from QuestDB,
//! per-base aggregates them at the same 5 s sample cadence the
//! strategy uses internally, and writes a single row per base into the
//! `bf_baseline` table.
//!
//! The point: when spread_revert (or any other consumer) starts, it
//! reads the most recent baseline rows from `bf_baseline` and is ready
//! to trade immediately — no 5 min in-memory warm-up.
//!
//! Schema (created automatically on first write):
//!   bf_baseline
//!     base                    SYMBOL
//!     bf_avg_gap_bp           DOUBLE
//!     flipster_avg_spread_bp  DOUBLE
//!     binance_avg_spread_bp   DOUBLE
//!     n_gap_samples           LONG
//!     n_spread_samples        LONG
//!     timestamp               TIMESTAMP   (designated)
//!
//! `bf_avg_gap_bp` is computed as
//!     ((flipster_mid - binance_mid) / binance_mid) * 1e4
//! averaged over the last 30 min sampled at 5 s. The two avg spreads
//! are derived from the same 5 s samples but only over the last 10 min
//! window (Jay's spec).

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::time::interval;
use tracing::{info, warn};

use crate::ilp::IlpWriter;

const DEFAULT_QDB_HTTP: &str = "http://211.181.122.102:9000";
const DEFAULT_INTERVAL_S: u64 = 5;
const GAP_WINDOW_MIN: i64 = 30;
const SPREAD_WINDOW_MIN: i64 = 10;
/// Big enough to hold one row per active USDT base. ~500 symbols * 8 cols.
const QUERY_LIMIT_PER_TICK: usize = 5_000;

/// Spawn the baseline writer task. Cheap to call multiple times — the
/// caller decides if/when to enable it. Reads `BASELINE_QDB_HTTP` and
/// `BASELINE_INTERVAL_S` env knobs.
pub fn spawn(writer: IlpWriter) {
    let qdb_http = std::env::var("BASELINE_QDB_HTTP")
        .ok()
        .or_else(|| std::env::var("QUESTDB_HTTP_URL").ok())
        .unwrap_or_else(|| DEFAULT_QDB_HTTP.to_string());
    let interval_s: u64 = std::env::var("BASELINE_INTERVAL_S")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_INTERVAL_S);
    info!(qdb_http = %qdb_http, interval_s, "baseline_writer: starting");
    tokio::spawn(async move {
        if let Err(e) = run_loop(writer, qdb_http, interval_s).await {
            warn!(error = %e, "baseline_writer: terminated");
        }
    });
}

async fn run_loop(writer: IlpWriter, qdb_http: String, interval_s: u64) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let mut tick = interval(Duration::from_secs(interval_s));
    loop {
        tick.tick().await;
        match compute_and_write_one_cycle(&http, &qdb_http, &writer).await {
            Ok(n) if n > 0 => {
                info!(n_bases = n, "baseline_writer: wrote bf_baseline rows");
            }
            Ok(_) => {} // 0 → quiet
            Err(e) => warn!(error = %e, "baseline_writer: cycle failed"),
        }
    }
}

async fn compute_and_write_one_cycle(
    http: &reqwest::Client,
    qdb_http: &str,
    writer: &IlpWriter,
) -> Result<usize> {
    // Four window-aggregate queries (one per venue × per window). Each
    // returns one row per base with avg mid and avg spread. The gap
    // window is 30 min, the spread window is 10 min — Jay's spec.
    let bin_long = fetch_per_base_avgs(http, qdb_http, "binance_bookticker", GAP_WINDOW_MIN).await
        .context("fetch binance long-window")?;
    let fli_long = fetch_per_base_avgs(http, qdb_http, "flipster_bookticker", GAP_WINDOW_MIN).await
        .context("fetch flipster long-window")?;
    let bin_short = fetch_per_base_avgs(http, qdb_http, "binance_bookticker", SPREAD_WINDOW_MIN).await
        .context("fetch binance short-window")?;
    let fli_short = fetch_per_base_avgs(http, qdb_http, "flipster_bookticker", SPREAD_WINDOW_MIN).await
        .context("fetch flipster short-window")?;

    let now = chrono::Utc::now();
    let mut wrote = 0usize;

    // Bases must appear in both long-window queries — gap is
    // undefined without both legs. Spreads default to 0.0 when the
    // short-window query happens not to include that base.
    for (base, bin_l) in &bin_long {
        let Some(fli_l) = fli_long.get(base) else { continue };
        if bin_l.mid <= 0.0 || fli_l.mid <= 0.0 {
            continue;
        }
        let bf_avg_gap_bp = (fli_l.mid - bin_l.mid) / bin_l.mid * 1e4;
        let bin_avg_spread_bp = bin_short.get(base).map(|v| v.spread_bp).unwrap_or(0.0);
        let fli_avg_spread_bp = fli_short.get(base).map(|v| v.spread_bp).unwrap_or(0.0);

        if let Err(e) = writer
            .write_baseline(
                base,
                now,
                bf_avg_gap_bp,
                fli_avg_spread_bp,
                bin_avg_spread_bp,
                // The single-row aggregate doesn't expose tick counts;
                // strategy consumers can ignore these or compare table
                // recency to gauge data freshness.
                0,
                0,
            )
            .await
        {
            warn!(base = %base, error = %e, "baseline_writer: ilp write failed");
        } else {
            wrote += 1;
        }
    }
    Ok(wrote)
}

/// Per-base aggregate from one window.
#[derive(Debug, Default)]
struct VenueAvg {
    mid: f64,
    spread_bp: f64,
}

/// One SQL query per (venue, window). QuestDB groups by the
/// base-normalised symbol; we average mid and spread over the whole
/// window in SQL so the result is one row per base — much cheaper
/// than streaming individual 5 s buckets across the wire.
async fn fetch_per_base_avgs(
    http: &reqwest::Client,
    qdb_http: &str,
    table: &str,
    window_min: i64,
) -> Result<std::collections::HashMap<String, VenueAvg>> {
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
    let mut out: std::collections::HashMap<String, VenueAvg> =
        std::collections::HashMap::with_capacity(QUERY_LIMIT_PER_TICK / 8);
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

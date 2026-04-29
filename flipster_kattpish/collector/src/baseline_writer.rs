//! Per-base baseline aggregator (in-memory).
//!
//! Every `interval_s` (default 5 s) computes per-base
//!   - `bf_avg_gap_bp`: 30 min avg of (flipster_mid - binance_mid)/binance_mid
//!   - `flipster_avg_spread_bp` / `binance_avg_spread_bp`: 10 min per-venue
//!     avg bid-ask spread
//! and writes them into a shared `Arc<RwLock<HashMap<base, BaselinePoint>>>`.
//!
//! Aggregation method (the part that matters):
//!   * 5 s SAMPLE BY buckets on each venue.
//!   * INNER JOIN on (base, timestamp) — only buckets present on **both**
//!     venues count toward the average. Without this, naive
//!     `avg(binance) - avg(flipster)` averages two differently-weighted
//!     time series and drifts away from the real cross-venue gap.
//!   * `last((bid+ask)/2)` per bucket (cheaper than avg, ≈ same value
//!     because the price barely moves within 5 s).
//!   * Short-window (10 min spread) uses `FILL(PREV)` + a 15 min lookback
//!     so sparse symbols don't lose buckets. Long-window (30 min gap)
//!     doesn't need it — 360 buckets is plenty even for thin alts.

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
const SPREAD_LOOKBACK_MIN: i64 = 15; // FILL(PREV) seed slack for sparse symbols.

/// Most recent baseline for one base.
#[derive(Debug, Clone)]
pub struct BaselinePoint {
    pub ts: DateTime<Utc>,
    pub bf_avg_gap_bp: f64,
    pub flipster_avg_spread_bp: f64,
    pub binance_avg_spread_bp: f64,
}

pub type SharedBaselines = Arc<RwLock<HashMap<String, BaselinePoint>>>;

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
        .timeout(Duration::from_secs(60))
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
    // Two queries — one matched-bucket aggregation each — instead of
    // four naive `avg()`s. Both INNER JOIN on (base, timestamp) so only
    // buckets present on both venues count.
    let gaps = fetch_gap_30m(http, qdb_http)
        .await
        .context("fetch 30m gap")?;
    let spreads = fetch_spread_10m(http, qdb_http)
        .await
        .context("fetch 10m spread")?;

    let now = Utc::now();
    let mut out: HashMap<String, BaselinePoint> = HashMap::with_capacity(gaps.len());
    for (base, gap_bp) in gaps {
        let (fli_sp, bin_sp) = spreads.get(&base).copied().unwrap_or((0.0, 0.0));
        out.insert(
            base,
            BaselinePoint {
                ts: now,
                bf_avg_gap_bp: gap_bp,
                flipster_avg_spread_bp: fli_sp,
                binance_avg_spread_bp: bin_sp,
            },
        );
    }
    Ok(out)
}

/// 30 min `bf_avg_gap_bp` per base — bucket-matched JOIN, no FILL.
async fn fetch_gap_30m(
    http: &reqwest::Client,
    qdb_http: &str,
) -> Result<HashMap<String, f64>> {
    let sql = format!(
        r#"
WITH bn AS (
  SELECT replace(replace(symbol,'_',''),'USDT','') base, timestamp,
         last((ask_price+bid_price)/2.0) mid
  FROM binance_bookticker
  WHERE timestamp > dateadd('m', -{w}, now())
    AND bid_price > 0 AND ask_price > 0
  SAMPLE BY 5s
),
fl AS (
  SELECT replace(replace(symbol,'.PERP',''),'USDT','') base, timestamp,
         last((ask_price+bid_price)/2.0) mid
  FROM flipster_bookticker
  WHERE timestamp > dateadd('m', -{w}, now())
    AND bid_price > 0 AND ask_price > 0
  SAMPLE BY 5s
)
SELECT bn.base,
       avg((fl.mid - bn.mid) / bn.mid * 10000.0) gap_bp
FROM bn JOIN fl ON bn.base = fl.base AND bn.timestamp = fl.timestamp
GROUP BY bn.base
"#,
        w = GAP_WINDOW_MIN,
    );
    let body = exec(http, qdb_http, &sql).await?;
    let mut out = HashMap::new();
    for row in dataset(&body) {
        let arr = match row.as_array() {
            Some(a) if a.len() >= 2 => a,
            _ => continue,
        };
        let base = arr[0].as_str().unwrap_or("").to_string();
        let gap = arr[1].as_f64().unwrap_or(0.0);
        if !base.is_empty() && gap.is_finite() {
            out.insert(base, gap);
        }
    }
    Ok(out)
}

/// 10 min `(fli_avg_spread_bp, bin_avg_spread_bp)` per base. Uses
/// FILL(PREV) + 15 min lookback so sparse symbols don't lose buckets;
/// the outer WHERE then trims to the actual 10 min window.
async fn fetch_spread_10m(
    http: &reqwest::Client,
    qdb_http: &str,
) -> Result<HashMap<String, (f64, f64)>> {
    let sql = format!(
        r#"
WITH bn AS (
  SELECT replace(replace(symbol,'_',''),'USDT','') base, timestamp,
         last((ask_price-bid_price)/((ask_price+bid_price)/2.0)*10000.0) sp_bp
  FROM binance_bookticker
  WHERE timestamp > dateadd('m', -{lb}, now())
    AND bid_price > 0 AND ask_price > 0
  SAMPLE BY 5s FILL(PREV)
),
fl AS (
  SELECT replace(replace(symbol,'.PERP',''),'USDT','') base, timestamp,
         last((ask_price-bid_price)/((ask_price+bid_price)/2.0)*10000.0) sp_bp
  FROM flipster_bookticker
  WHERE timestamp > dateadd('m', -{lb}, now())
    AND bid_price > 0 AND ask_price > 0
  SAMPLE BY 5s FILL(PREV)
)
SELECT bn.base,
       avg(fl.sp_bp) fli_sp_bp,
       avg(bn.sp_bp) bin_sp_bp
FROM bn JOIN fl ON bn.base = fl.base AND bn.timestamp = fl.timestamp
WHERE bn.timestamp > dateadd('m', -{w}, now())
GROUP BY bn.base
"#,
        lb = SPREAD_LOOKBACK_MIN,
        w = SPREAD_WINDOW_MIN,
    );
    let body = exec(http, qdb_http, &sql).await?;
    let mut out = HashMap::new();
    for row in dataset(&body) {
        let arr = match row.as_array() {
            Some(a) if a.len() >= 3 => a,
            _ => continue,
        };
        let base = arr[0].as_str().unwrap_or("").to_string();
        let fli = arr[1].as_f64().unwrap_or(0.0);
        let bin = arr[2].as_f64().unwrap_or(0.0);
        if !base.is_empty() && fli.is_finite() && bin.is_finite() {
            out.insert(base, (fli, bin));
        }
    }
    Ok(out)
}

async fn exec(http: &reqwest::Client, qdb_http: &str, sql: &str) -> Result<Value> {
    let url = format!("{}/exec", qdb_http.trim_end_matches('/'));
    let resp = http.get(&url).query(&[("query", sql)]).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "QuestDB {}: {}",
            status,
            &body[..body.len().min(300)]
        ));
    }
    Ok(resp.json().await?)
}

fn dataset(body: &Value) -> Vec<Value> {
    body.get("dataset")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

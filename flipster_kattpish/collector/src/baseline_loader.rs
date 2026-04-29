//! Read pre-computed baselines from the `bf_baseline` table so
//! spread_revert (and any future consumer) can start without waiting
//! for an in-memory rolling window to fill.
//!
//! Schema produced by `baseline_writer`. We only need the most recent
//! 30 min slice — that's the longest window the strategy uses.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;

const DEFAULT_QDB_HTTP: &str = "http://211.181.122.102:9000";

/// One pre-aggregated point ready to seed a strategy's rolling buffer.
#[derive(Debug, Clone)]
pub struct BaselinePoint {
    pub base: String,
    pub ts: DateTime<Utc>,
    pub bf_avg_gap_bp: f64,
    pub flipster_avg_spread_bp: f64,
    pub binance_avg_spread_bp: f64,
}

/// Fetch the last 30 min × 5 s = 360 baseline points per base from
/// `bf_baseline`. Returns one Vec per base, oldest → newest. Bases
/// without any rows are absent from the map.
///
/// Caller is expected to fall back to live ticks if the map is empty
/// (cold-start situation: baseline_writer hasn't run yet).
pub async fn fetch_recent_per_base() -> Result<HashMap<String, Vec<BaselinePoint>>> {
    let qdb_http = std::env::var("BASELINE_QDB_HTTP")
        .ok()
        .or_else(|| std::env::var("QUESTDB_HTTP_URL").ok())
        .unwrap_or_else(|| DEFAULT_QDB_HTTP.to_string());

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let sql =
        "SELECT base, timestamp, bf_avg_gap_bp, flipster_avg_spread_bp, binance_avg_spread_bp \
         FROM bf_baseline \
         WHERE timestamp > dateadd('m', -30, now()) \
         ORDER BY timestamp ASC";
    let url = format!("{}/exec", qdb_http.trim_end_matches('/'));
    let resp = http.get(&url).query(&[("query", sql)]).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        // Table-doesn't-exist (first run) shows up as a 4xx with
        // "table does not exist" — bubble it up so the caller can fall
        // back to a clean cold start.
        return Err(anyhow::anyhow!(
            "bf_baseline query failed: {}: {}",
            status,
            &body[..body.len().min(200)]
        ));
    }
    let body: Value = resp.json().await.context("parse bf_baseline json")?;
    let dataset = body
        .get("dataset")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut by_base: HashMap<String, Vec<BaselinePoint>> = HashMap::new();
    for row in dataset {
        let arr = match row.as_array() {
            Some(a) if a.len() >= 5 => a,
            _ => continue,
        };
        let base = match arr[0].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let ts_str = match arr[1].as_str() {
            Some(s) => s,
            None => continue,
        };
        let ts = match DateTime::parse_from_rfc3339(ts_str) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => continue,
        };
        let bf_avg_gap_bp = arr[2].as_f64().unwrap_or(0.0);
        let flipster_avg_spread_bp = arr[3].as_f64().unwrap_or(0.0);
        let binance_avg_spread_bp = arr[4].as_f64().unwrap_or(0.0);
        if !bf_avg_gap_bp.is_finite() {
            continue;
        }
        by_base.entry(base.clone()).or_default().push(BaselinePoint {
            base,
            ts,
            bf_avg_gap_bp,
            flipster_avg_spread_bp,
            binance_avg_spread_bp,
        });
    }
    Ok(by_base)
}

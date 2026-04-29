//! Thin client over the QuestDB HTTP `/exec` endpoint. Used only for the
//! edge-recheck and TP loops — order placement uses ZMQ-delivered signals
//! and live exchange responses, never QDB.

use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;

use crate::config::QUESTDB_URL;

#[derive(Debug, Deserialize)]
struct ExecResp {
    #[serde(default)]
    dataset: Vec<Vec<serde_json::Value>>,
}

/// Returns the latest (bid, ask) for `symbol` in `table`, or None.
pub async fn fetch_latest_mid(
    http: &Client,
    table: &str,
    symbol: &str,
) -> Option<(f64, f64)> {
    let sql = format!(
        "SELECT bid_price, ask_price FROM {table} \
         WHERE symbol='{symbol}' ORDER BY timestamp DESC LIMIT 1"
    );
    run_query(http, &sql).await.and_then(|rows| {
        let row = rows.into_iter().next()?;
        let bid = value_as_f64(row.get(0)?)?;
        let ask = value_as_f64(row.get(1)?)?;
        Some((bid, ask))
    })
}

/// Returns the latest (bid, ask, age_seconds) for `symbol`, or None.
pub async fn fetch_latest_mid_age(
    http: &Client,
    table: &str,
    symbol: &str,
) -> Option<(f64, f64, f64)> {
    let sql = format!(
        "SELECT bid_price, ask_price, timestamp FROM {table} \
         WHERE symbol='{symbol}' ORDER BY timestamp DESC LIMIT 1"
    );
    let rows = run_query(http, &sql).await?;
    let row = rows.into_iter().next()?;
    let bid = value_as_f64(row.get(0)?)?;
    let ask = value_as_f64(row.get(1)?)?;
    let ts_str = row.get(2)?.as_str()?;
    let ts: DateTime<Utc> = ts_str
        .replace('Z', "+00:00")
        .parse::<DateTime<chrono::FixedOffset>>()
        .ok()?
        .with_timezone(&Utc);
    let age = (Utc::now() - ts).num_milliseconds() as f64 / 1000.0;
    Some((bid, ask, age))
}

async fn run_query(
    http: &Client,
    sql: &str,
) -> Option<Vec<Vec<serde_json::Value>>> {
    let url = format!(
        "{QUESTDB_URL}/exec?query={}",
        urlencoding::encode(sql)
    );
    let resp = http.get(&url).send().await.ok()?;
    let parsed: ExecResp = resp.json().await.ok()?;
    Some(parsed.dataset)
}

fn value_as_f64(v: &serde_json::Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    v.as_str().and_then(|s| s.parse::<f64>().ok())
}

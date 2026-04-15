//! Stream bookticker rows out of a shared QuestDB into the local broadcast
//! channel, bypassing exchange WebSockets.
//!
//! Used when the collector runs on a strategy host that shares a central
//! QuestDB with an upstream feed pipeline (e.g. gate1). Each configured
//! exchange table gets its own background task polling
//! `WHERE timestamp > cursor ORDER BY timestamp` on a short interval and
//! re-emitting parsed `BookTick`s exactly as if they had come from a WS
//! subscriber. The rest of the collector (paper_bot strategies, latency
//! engine) sees an identical tick stream.
//!
//! Tradeoffs:
//! - No Flipster API key required on the strategy host (feed owns the WS).
//! - Added latency = poll interval + QuestDB query round-trip (~20–100 ms).
//! - Paper bot mean-reversion at 60s hold is fine with this delay; sub-second
//!   latency arb is NOT.

use anyhow::{anyhow, Result};
use chrono::{DateTime, TimeZone, Utc};
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::model::{BookTick, ExchangeName};

#[derive(Clone, Debug)]
pub struct ReaderConfig {
    pub http_url: String,
    pub poll_interval_ms: u64,
    /// How far back to start the cursor at startup (avoids replaying old
    /// history, but keeps a safety margin to not miss just-landed rows).
    pub start_lookback_secs: i64,
    /// Per-query LIMIT. Don't set too low or we'll miss rows at high throughput.
    pub batch_limit: usize,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            http_url: "http://127.0.0.1:9000".into(),
            poll_interval_ms: 200,
            start_lookback_secs: 3,
            batch_limit: 20_000,
        }
    }
}

/// Spawn one reader task per (exchange, table). Sends parsed `BookTick`s into
/// the broadcast channel the paper bot already subscribes to.
pub fn spawn(
    exchange: ExchangeName,
    table: &'static str,
    cfg: ReaderConfig,
    tx: broadcast::Sender<BookTick>,
) {
    info!(
        exchange = exchange.as_str(),
        table,
        url = %cfg.http_url,
        poll_ms = cfg.poll_interval_ms,
        "questdb_reader: starting"
    );
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        // Start cursor slightly in the past so we don't miss rows that landed
        // moments before startup.
        let mut cursor_ns: i64 = Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .saturating_sub(cfg.start_lookback_secs * 1_000_000_000);
        let mut iv = tokio::time::interval(Duration::from_millis(cfg.poll_interval_ms.max(10)));
        loop {
            iv.tick().await;
            match poll_once(&client, &cfg, exchange, table, cursor_ns).await {
                Ok((rows, max_ts_ns)) => {
                    let n = rows.len();
                    for tick in rows {
                        let _ = tx.send(tick);
                    }
                    if max_ts_ns > cursor_ns {
                        cursor_ns = max_ts_ns;
                    }
                    if n > 0 {
                        tracing::debug!(exchange = exchange.as_str(), n, "questdb_reader: emitted");
                    }
                }
                Err(e) => {
                    warn!(
                        exchange = exchange.as_str(),
                        table,
                        error = %e,
                        "questdb_reader: poll failed"
                    );
                }
            }
        }
    });
}

async fn poll_once(
    client: &reqwest::Client,
    cfg: &ReaderConfig,
    exchange: ExchangeName,
    table: &str,
    cursor_ns: i64,
) -> Result<(Vec<BookTick>, i64)> {
    // Use an ISO8601 literal in the WHERE clause — QuestDB auto-casts it.
    let cursor_iso = Utc
        .timestamp_nanos(cursor_ns)
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string();

    // Flipster bookticker doesn't have bid_size/ask_size columns; fill with 0.
    let sql = match exchange {
        ExchangeName::Flipster => format!(
            "SELECT symbol, bid_price, ask_price, 0.0 bid_size, 0.0 ask_size, timestamp \
             FROM {table} WHERE timestamp > '{cursor_iso}' ORDER BY timestamp LIMIT {lim}",
            lim = cfg.batch_limit
        ),
        _ => format!(
            "SELECT symbol, bid_price, ask_price, bid_size, ask_size, timestamp \
             FROM {table} WHERE timestamp > '{cursor_iso}' ORDER BY timestamp LIMIT {lim}",
            lim = cfg.batch_limit
        ),
    };
    let url = format!("{}/exec", cfg.http_url.trim_end_matches('/'));
    let resp = client.get(&url).query(&[("query", sql)]).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!(
            "questdb {}: {}",
            status,
            resp.text().await.unwrap_or_default()
        ));
    }
    let v: serde_json::Value = resp.json().await?;
    if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
        return Err(anyhow!("questdb error: {}", err));
    }
    let dataset = v
        .get("dataset")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::with_capacity(dataset.len());
    let mut max_ts_ns = cursor_ns;
    for row in dataset {
        let Some(arr) = row.as_array() else {
            continue;
        };
        // Columns: symbol, bid_price, ask_price, bid_size, ask_size, timestamp
        let raw_symbol = arr
            .first()
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        if raw_symbol.is_empty() {
            continue;
        }
        // Gate rows from the shared central feed use "BTC_USDT" form; the
        // rest of this codebase expects "BTCUSDT" (matches what our own
        // exchanges/gate.rs parse_frame emits). Normalize here.
        let symbol = if matches!(exchange, ExchangeName::Gate) {
            raw_symbol.replace('_', "")
        } else {
            raw_symbol.to_string()
        };
        let bid = arr.get(1).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let ask = arr.get(2).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let bid_size = arr.get(3).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let ask_size = arr.get(4).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let ts_iso = arr.get(5).and_then(|x| x.as_str()).unwrap_or_default();
        let ts = match DateTime::parse_from_rfc3339(ts_iso) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => continue,
        };
        let ts_ns = ts.timestamp_nanos_opt().unwrap_or(0);
        if ts_ns > max_ts_ns {
            max_ts_ns = ts_ns;
        }
        out.push(BookTick {
            exchange,
            symbol,
            bid_price: bid,
            ask_price: ask,
            bid_size,
            ask_size,
            last_price: None,
            mark_price: None,
            index_price: None,
            timestamp: ts,
        });
    }
    Ok((out, max_ts_ns))
}

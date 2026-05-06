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
use std::collections::VecDeque;
use std::time::{Duration, Instant};
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

// ---------------------------------------------------------------------------
// Historical replay (backtest mode)
// ---------------------------------------------------------------------------

const FLIPSTER_TABLE: &str = "flipster_bookticker";

/// Fetch a single ordered chunk `cursor < ts <= end_ns` from one bookticker
/// table. Returns the parsed ticks plus the new cursor (= last row's ts).
async fn fetch_chunk(
    client: &reqwest::Client,
    cfg: &ReaderConfig,
    exchange: ExchangeName,
    table: &str,
    cursor_ns: i64,
    end_ns: i64,
) -> Result<(Vec<BookTick>, i64)> {
    let cursor_iso = Utc
        .timestamp_nanos(cursor_ns)
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string();
    let end_iso = Utc
        .timestamp_nanos(end_ns)
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string();
    let sql = match exchange {
        ExchangeName::Flipster => format!(
            "SELECT symbol, bid_price, ask_price, 0.0 bid_size, 0.0 ask_size, timestamp \
             FROM {table} WHERE timestamp > '{cursor_iso}' AND timestamp <= '{end_iso}' \
             ORDER BY timestamp LIMIT {lim}",
            lim = cfg.batch_limit
        ),
        _ => format!(
            "SELECT symbol, bid_price, ask_price, bid_size, ask_size, timestamp \
             FROM {table} WHERE timestamp > '{cursor_iso}' AND timestamp <= '{end_iso}' \
             ORDER BY timestamp LIMIT {lim}",
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
        let Some(arr) = row.as_array() else { continue };
        let raw_symbol = arr.first().and_then(|x| x.as_str()).unwrap_or_default();
        if raw_symbol.is_empty() {
            continue;
        }
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

/// Drive the paper bot with historical bookticker rows streamed out of the
/// shared QuestDB, merged in timestamp order across flipster + hedge tables.
///
/// The merge uses a watermark scheme: at any point, only rows with `ts <=
/// min(flipster_cursor, hedge_cursor)` are safe to emit, because we know no
/// earlier rows can still arrive on either side. When one side's buffer is
/// drained, we refill it before continuing. When both sides are exhausted,
/// the replay returns.
///
/// Drops into the same broadcast channel as live mode, so all strategy code
/// runs unchanged.
pub async fn replay_merged(
    cfg: ReaderConfig,
    start_ts: DateTime<Utc>,
    end_ts: DateTime<Utc>,
    hedge_exchange: ExchangeName,
    hedge_table: &str,
    follower_exchange: ExchangeName,
    follower_table: &str,
    tx: broadcast::Sender<BookTick>,
) -> Result<u64> {
    info!(
        start = %start_ts,
        end = %end_ts,
        hedge = hedge_exchange.as_str(),
        hedge_table,
        follower = follower_exchange.as_str(),
        follower_table,
        "backtest replay: starting"
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?;
    let start_ns = start_ts.timestamp_nanos_opt().unwrap_or(0);
    let end_ns = end_ts.timestamp_nanos_opt().unwrap_or(i64::MAX);
    let mut fl_cursor = start_ns;
    let mut bn_cursor = start_ns;
    let mut fl_buf: VecDeque<BookTick> = VecDeque::new();
    let mut bn_buf: VecDeque<BookTick> = VecDeque::new();
    let mut fl_done = false;
    let mut bn_done = false;

    let mut emitted: u64 = 0;
    let mut last_log = Instant::now();

    loop {
        // Refill empty buffers — unless the side is already drained past end_ns.
        if fl_buf.is_empty() && !fl_done {
            let (rows, max_ts) = fetch_chunk(
                &client, &cfg, follower_exchange, follower_table, fl_cursor, end_ns,
            )
            .await?;
            if rows.is_empty() {
                fl_done = true;
            } else {
                fl_cursor = max_ts;
                fl_buf.extend(rows);
            }
        }
        if bn_buf.is_empty() && !bn_done {
            let (rows, max_ts) = fetch_chunk(
                &client, &cfg, hedge_exchange, hedge_table, bn_cursor, end_ns,
            )
            .await?;
            if rows.is_empty() {
                bn_done = true;
            } else {
                bn_cursor = max_ts;
                bn_buf.extend(rows);
            }
        }
        // Done when both sides drained.
        if fl_buf.is_empty() && bn_buf.is_empty() && fl_done && bn_done {
            break;
        }
        // Watermark = min of still-active cursors. A side that is done lifts
        // its cursor to +inf so the other side can drain freely.
        let watermark = match (fl_done, bn_done) {
            (false, false) => fl_cursor.min(bn_cursor),
            (true, false) => bn_cursor,
            (false, true) => fl_cursor,
            (true, true) => i64::MAX,
        };

        // Drain from both buffers in chronological order up to the watermark.
        let mut batch_emitted = 0u64;
        loop {
            let (take_fl, ts_ns) = match (fl_buf.front(), bn_buf.front()) {
                (Some(f), Some(b)) => {
                    if f.timestamp <= b.timestamp {
                        (true, f.timestamp.timestamp_nanos_opt().unwrap_or(0))
                    } else {
                        (false, b.timestamp.timestamp_nanos_opt().unwrap_or(0))
                    }
                }
                (Some(f), None) => (true, f.timestamp.timestamp_nanos_opt().unwrap_or(0)),
                (None, Some(b)) => (false, b.timestamp.timestamp_nanos_opt().unwrap_or(0)),
                (None, None) => break,
            };
            if ts_ns > watermark {
                break;
            }
            let tick = if take_fl {
                fl_buf.pop_front().unwrap()
            } else {
                bn_buf.pop_front().unwrap()
            };
            let _ = tx.send(tick);
            emitted += 1;
            batch_emitted += 1;
            // Yield aggressively: with 16 variant subscribers each processing
            // ticks sequentially, a fast producer overruns their lag buffers.
            // 256 emits/yield keeps the broadcast channel from backing up
            // without materially slowing the replay.
            if batch_emitted % 256 == 0 {
                tokio::task::yield_now().await;
            }
        }

        if last_log.elapsed().as_secs() >= 2 {
            info!(
                emitted,
                fl_done,
                bn_done,
                watermark_iso = %Utc.timestamp_nanos(watermark).format("%H:%M:%S%.3f"),
                "backtest replay progress"
            );
            last_log = Instant::now();
        }
    }

    info!(emitted, "backtest replay: done");
    Ok(emitted)
}

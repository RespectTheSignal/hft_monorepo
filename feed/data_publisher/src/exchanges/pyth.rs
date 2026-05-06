//! Pyth Network — fair-value oracle aggregating MM quotes from many sources.
//!
//! WS:    wss://hermes.pyth.network/ws
//! REST:  https://hermes.pyth.network/v2/price_feeds?asset_type=crypto
//! Sub:   {"type":"subscribe","ids":["<64-hex-feed-id>", ...]}
//! Tick:  price_update.price_feed.price = {price, conf, expo, publish_time}
//!        real_price = price * 10^expo;  conf is the +/- 1σ uncertainty band.
//!
//! Schema mapping (so existing dashboards Just Work):
//!   bid_price = (price - conf) * 10^expo   (lower bound of fair value)
//!   ask_price = (price + conf) * 10^expo   (upper bound)
//!   mid       = price * 10^expo            (Pyth's published fair value)
//!   spread    = 2*conf*10^expo             (Pyth's confidence interval)

use crate::error::{Error, Result};
use crate::exchanges::common::{backoff_secs, http_get_json, now_ns};
use crate::ilp::{BookTickerRow, BookTickerWriter};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

pub const WS_URL: &str = "wss://hermes.pyth.network/ws";
pub const REST_FEEDS: &str =
    "https://hermes.pyth.network/v2/price_feeds?asset_type=crypto";

/// (display_symbol, feed_id_hex) for every USD-quoted crypto feed.
pub async fn fetch_usd_crypto_feeds(client: &reqwest::Client) -> Result<Vec<(String, String)>> {
    let v = http_get_json(client, REST_FEEDS).await?;
    let arr = v
        .as_array()
        .ok_or_else(|| Error::Other("pyth feeds: not an array".into()))?;
    let mut out = Vec::new();
    for f in arr {
        let id = f.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let attrs = f.get("attributes").cloned().unwrap_or_default();
        let quote = attrs.get("quote_currency").and_then(|x| x.as_str()).unwrap_or("");
        let display = attrs.get("display_symbol").and_then(|x| x.as_str()).unwrap_or("");
        if quote == "USD" && !id.is_empty() && !display.is_empty() {
            out.push((display.to_string(), id.to_string()));
        }
    }
    out.sort();
    Ok(out)
}

pub async fn run_ws_task(
    id: usize,
    feeds: Vec<(String, String)>,
    writer: BookTickerWriter,
) {
    let mut attempt: u32 = 0;
    loop {
        info!("[pyth-{id}] connecting ({} feeds)", feeds.len());
        match run_one_session(id, &feeds, &writer).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = backoff_secs(attempt);
                attempt = attempt.saturating_add(1);
                warn!("[pyth-{id}] disconnected: {e} — reconnect in {secs}s");
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }
}

async fn run_one_session(
    id: usize,
    feeds: &[(String, String)],
    writer: &BookTickerWriter,
) -> Result<()> {
    let (mut ws, _resp) = connect_async(WS_URL).await?;

    // feed_id → display_symbol lookup for parsed price_updates
    let id_to_sym: HashMap<String, String> = feeds
        .iter()
        .map(|(s, i)| (i.clone(), s.clone()))
        .collect();

    // Hermes accepts a single subscribe with all ids, but to stay polite we
    // chunk. 100/sub frame is safe.
    for chunk in feeds.chunks(100) {
        let ids: Vec<&str> = chunk.iter().map(|(_, i)| i.as_str()).collect();
        let payload = serde_json::json!({"type": "subscribe", "ids": ids});
        ws.send(Message::Text(payload.to_string())).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    info!("[pyth-{id}] subscribed");

    loop {
        let msg = match ws.next().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => return Err(e.into()),
            None => return Err(Error::Other("ws stream ended".into())),
        };
        match msg {
            Message::Text(text) => {
                if let Some(row) = parse_tick(&text, &id_to_sym) {
                    writer.submit(row);
                } else {
                    debug!("[pyth-{id}] non-tick: {}", &text[..text.len().min(180)]);
                }
            }
            Message::Binary(_) => {}
            Message::Ping(p) => ws.send(Message::Pong(p)).await?,
            Message::Pong(_) => {}
            Message::Close(f) => return Err(Error::Other(format!("server close: {:?}", f))),
            Message::Frame(_) => {}
        }
    }
}

fn parse_tick(text: &str, id_to_sym: &HashMap<String, String>) -> Option<BookTickerRow> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("type").and_then(|x| x.as_str()) != Some("price_update") {
        return None;
    }
    let pf = v.get("price_feed")?;
    let id = pf.get("id").and_then(|x| x.as_str())?;
    let symbol = id_to_sym.get(id).cloned()?;
    let p = pf.get("price")?;
    let raw_price: f64 = p.get("price").and_then(|x| x.as_str()).and_then(|s| s.parse().ok())?;
    let raw_conf:  f64 = p.get("conf") .and_then(|x| x.as_str()).and_then(|s| s.parse().ok())?;
    let expo:      i32 = p.get("expo") .and_then(|x| x.as_i64())? as i32;
    let scale = 10f64.powi(expo);
    let price = raw_price * scale;
    let conf  = raw_conf  * scale;
    if !price.is_finite() || price <= 0.0 || !conf.is_finite() || conf < 0.0 {
        return None;
    }
    let server_ts_ns = p
        .get("publish_time")
        .and_then(|x| x.as_i64())
        .map(|sec| sec * 1_000_000_000)
        .unwrap_or_else(now_ns);
    Some(BookTickerRow {
        symbol,
        bid: price - conf,
        ask: price + conf,
        bid_size: Some(conf),
        ask_size: Some(conf),
        server_ts_ns,
        recv_ts_ns: None,
    })
}

pub fn spawn_all(feeds: Vec<(String, String)>, topics_per_conn: usize, writer: BookTickerWriter) {
    let chunks: Vec<Vec<(String, String)>> =
        feeds.chunks(topics_per_conn).map(|c| c.to_vec()).collect();
    info!(
        "Pyth: {} feeds → {} WS connections",
        chunks.iter().map(|c| c.len()).sum::<usize>(),
        chunks.len()
    );
    for (idx, fs) in chunks.into_iter().enumerate() {
        let w = writer.clone();
        tokio::spawn(async move {
            run_ws_task(idx, fs, w).await;
            error!("[pyth-{idx}] task exited unexpectedly");
        });
    }
}

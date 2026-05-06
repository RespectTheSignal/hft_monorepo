//! Kraken Futures (perpetual) book ticker via the `ticker` feed.
//!
//! WS:    wss://futures.kraken.com/ws/v1
//! REST:  https://futures.kraken.com/derivatives/api/v3/instruments
//! Sub:   {"event":"subscribe","feed":"ticker","product_ids":[...]}
//! Tick:  bid / ask / bid_size / ask_size / time (ms)
//! Ping:  none required (server pings)

use crate::error::{Error, Result};
use crate::exchanges::common::{backoff_secs, http_get_json, now_ns};
use crate::ilp::{BookTickerRow, BookTickerWriter};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

pub const WS_URL: &str = "wss://futures.kraken.com/ws/v1";
pub const REST_INSTRUMENTS: &str =
    "https://futures.kraken.com/derivatives/api/v3/instruments";

pub async fn fetch_perp_symbols(client: &reqwest::Client) -> Result<Vec<String>> {
    let v = http_get_json(client, REST_INSTRUMENTS).await?;
    let inst = v
        .get("instruments")
        .and_then(|x| x.as_array())
        .ok_or_else(|| Error::Other("kraken instruments: no array".into()))?;
    let mut out = Vec::with_capacity(inst.len());
    for i in inst {
        let sym = i.get("symbol").and_then(|x| x.as_str()).unwrap_or("");
        let kind = i.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let tradeable = i.get("tradeable").and_then(|x| x.as_bool()).unwrap_or(false);
        // Only perpetuals (PF_*, PI_*); skip dated futures (FI_*) and indices.
        if !tradeable || sym.is_empty() {
            continue;
        }
        if kind == "flexible_futures" || kind == "futures_inverse" {
            if sym.starts_with("PF_") || sym.starts_with("PI_") {
                out.push(sym.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

pub async fn run_ws_task(id: usize, symbols: Vec<String>, writer: BookTickerWriter) {
    let mut attempt: u32 = 0;
    loop {
        info!("[kraken-{id}] connecting ({} syms)", symbols.len());
        match run_one_session(id, &symbols, &writer).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = backoff_secs(attempt);
                attempt = attempt.saturating_add(1);
                warn!("[kraken-{id}] disconnected: {e} — reconnect in {secs}s");
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }
}

async fn run_one_session(
    id: usize,
    symbols: &[String],
    writer: &BookTickerWriter,
) -> Result<()> {
    let (mut ws, _resp) = connect_async(WS_URL).await?;

    // Kraken accepts arrays of product_ids per subscribe; chunk to keep
    // each frame small.
    for chunk in symbols.chunks(100) {
        let payload = serde_json::json!({
            "event": "subscribe",
            "feed": "ticker",
            "product_ids": chunk,
        });
        ws.send(Message::Text(payload.to_string())).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    info!("[kraken-{id}] subscribed");

    loop {
        let msg = match ws.next().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => return Err(e.into()),
            None => return Err(Error::Other("ws stream ended".into())),
        };
        match msg {
            Message::Text(text) => {
                if let Some(row) = parse_tick(&text) {
                    writer.submit(row);
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

fn parse_tick(text: &str) -> Option<BookTickerRow> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("feed").and_then(|x| x.as_str()) != Some("ticker") {
        return None;
    }
    if v.get("event").is_some() {
        // subscribed/unsubscribed events also carry feed:"ticker"
        return None;
    }
    let symbol = v.get("product_id").and_then(|x| x.as_str())?.to_string();
    let bid = v.get("bid").and_then(|x| x.as_f64())?;
    let ask = v.get("ask").and_then(|x| x.as_f64())?;
    if bid <= 0.0 || ask <= 0.0 {
        return None;
    }
    let bid_size = v.get("bid_size").and_then(|x| x.as_f64());
    let ask_size = v.get("ask_size").and_then(|x| x.as_f64());
    let server_ts_ns = v
        .get("time")
        .and_then(|x| x.as_i64())
        .map(|ms| ms * 1_000_000)
        .unwrap_or_else(now_ns);
    Some(BookTickerRow {
        symbol,
        bid,
        ask,
        bid_size,
        ask_size,
        server_ts_ns,
        recv_ts_ns: None,
    })
}

pub fn spawn_all(symbols: Vec<String>, topics_per_conn: usize, writer: BookTickerWriter) {
    let chunks: Vec<Vec<String>> = symbols.chunks(topics_per_conn).map(|c| c.to_vec()).collect();
    info!(
        "Kraken: {} syms → {} WS connections",
        chunks.iter().map(|c| c.len()).sum::<usize>(),
        chunks.len()
    );
    for (idx, syms) in chunks.into_iter().enumerate() {
        let w = writer.clone();
        tokio::spawn(async move {
            run_ws_task(idx, syms, w).await;
            error!("[kraken-{idx}] task exited unexpectedly");
        });
    }
}

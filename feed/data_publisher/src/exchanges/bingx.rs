//! BingX Swap (perpetual) book ticker feed.
//!
//! WS:    wss://open-api-swap.bingx.com/swap-market
//! REST:  https://open-api.bingx.com/openApi/swap/v2/quote/contracts
//! Sub:   {"id":"<uuid>","reqType":"sub","dataType":"<sym>@bookTicker"}
//! Tick:  data.b / data.a / data.T (ms)
//! Frames: gzip-compressed binary for both data AND control. Server sends
//!         "Ping" (after gunzip) every 5s; client replies plain text "Pong".

use crate::error::{Error, Result};
use crate::exchanges::common::{backoff_secs, http_get_json, now_ns, parse_f64};
use crate::ilp::{BookTickerRow, BookTickerWriter};
use flate2::read::GzDecoder;
use futures_util::{SinkExt, StreamExt};
use std::io::Read;
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

pub const WS_URL: &str = "wss://open-api-swap.bingx.com/swap-market";
pub const REST_CONTRACTS: &str = "https://open-api.bingx.com/openApi/swap/v2/quote/contracts";

pub async fn fetch_perp_symbols(client: &reqwest::Client) -> Result<Vec<String>> {
    let v = http_get_json(client, REST_CONTRACTS).await?;
    let data = v
        .get("data")
        .and_then(|x| x.as_array())
        .ok_or_else(|| Error::Other("bingx contracts: no data".into()))?;
    let mut out = Vec::with_capacity(data.len());
    for c in data {
        let sym = c.get("symbol").and_then(|x| x.as_str()).unwrap_or("");
        let currency = c.get("currency").and_then(|x| x.as_str()).unwrap_or("");
        let status = c.get("status").and_then(|x| x.as_i64()).unwrap_or(0);
        let api_open = c
            .get("apiStateOpen")
            .and_then(|x| x.as_str())
            .map(|s| s == "true")
            .unwrap_or(false);
        if currency == "USDT" && status == 1 && api_open && !sym.is_empty() {
            out.push(sym.to_string());
        }
    }
    out.sort();
    Ok(out)
}

pub async fn run_ws_task(id: usize, symbols: Vec<String>, writer: BookTickerWriter) {
    let mut attempt: u32 = 0;
    loop {
        info!("[bingx-{id}] connecting ({} syms)", symbols.len());
        match run_one_session(id, &symbols, &writer).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = backoff_secs(attempt);
                attempt = attempt.saturating_add(1);
                warn!("[bingx-{id}] disconnected: {e} — reconnect in {secs}s");
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

    for (i, sym) in symbols.iter().enumerate() {
        let payload = serde_json::json!({
            "id": format!("{id}-{i}"),
            "reqType": "sub",
            "dataType": format!("{sym}@bookTicker"),
        });
        ws.send(Message::Text(payload.to_string())).await?;
        // BingX rate-limits subs; one per ~10ms is safe.
        if i % 10 == 9 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    info!("[bingx-{id}] subscribed");

    loop {
        let msg = match ws.next().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => return Err(e.into()),
            None => return Err(Error::Other("ws stream ended".into())),
        };
        match msg {
            Message::Binary(bytes) => {
                let text = match gunzip(&bytes) {
                    Some(t) => t,
                    None => continue,
                };
                if text == "Ping" {
                    ws.send(Message::Text("Pong".into())).await?;
                    continue;
                }
                if let Some(row) = parse_tick(&text) {
                    writer.submit(row);
                } else {
                    debug!("[bingx-{id}] non-tick: {}", &text[..text.len().min(180)]);
                }
            }
            Message::Text(text) => {
                // Some BingX deployments send plain text Ping/Pong.
                if text == "Ping" {
                    ws.send(Message::Text("Pong".into())).await?;
                } else if let Some(row) = parse_tick(&text) {
                    writer.submit(row);
                }
            }
            Message::Ping(p) => ws.send(Message::Pong(p)).await?,
            Message::Pong(_) => {}
            Message::Close(f) => return Err(Error::Other(format!("server close: {:?}", f))),
            Message::Frame(_) => {}
        }
    }
}

fn gunzip(bytes: &[u8]) -> Option<String> {
    let mut decoder = GzDecoder::new(bytes);
    let mut out = String::new();
    decoder.read_to_string(&mut out).ok()?;
    Some(out)
}

fn parse_tick(text: &str) -> Option<BookTickerRow> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let dtype = v.get("dataType").and_then(|x| x.as_str()).unwrap_or("");
    if !dtype.contains("@bookTicker") {
        return None;
    }
    let data = v.get("data")?;
    let symbol = data
        .get("s")
        .and_then(|x| x.as_str())
        .or_else(|| dtype.split('@').next())?
        .to_string();
    let bid = data.get("b").and_then(|x| x.as_str()).and_then(parse_f64)?;
    let ask = data.get("a").and_then(|x| x.as_str()).and_then(parse_f64)?;
    if bid <= 0.0 || ask <= 0.0 {
        return None;
    }
    let bid_size = data.get("B").and_then(|x| x.as_str()).and_then(parse_f64);
    let ask_size = data.get("A").and_then(|x| x.as_str()).and_then(parse_f64);
    let server_ts_ns = data
        .get("T")
        .and_then(|x| x.as_i64())
        .map(|t| t * 1_000_000)
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
        "BingX: {} syms → {} WS connections",
        chunks.iter().map(|c| c.len()).sum::<usize>(),
        chunks.len()
    );
    for (idx, syms) in chunks.into_iter().enumerate() {
        let w = writer.clone();
        tokio::spawn(async move {
            run_ws_task(idx, syms, w).await;
            error!("[bingx-{idx}] task exited unexpectedly");
        });
    }
}

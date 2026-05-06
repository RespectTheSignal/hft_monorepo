//! MEXC Futures (USDT-margined) book ticker feed via depth.full channel.
//!
//! WS:    wss://contract.mexc.com/edge
//! REST:  https://contract.mexc.com/api/v1/contract/detail
//! Sub:   {"method":"sub.depth.full","param":{"symbol":"BTC_USDT","limit":5}}
//! Tick:  data.bids[0]=[px,size,n] / data.asks[0]=[px,size,n] / outer ts (ms)
//! Ping:  {"method":"ping"} every ~10s
//!
//! Note: we used to subscribe to `sub.ticker` which MEXC throttles to
//! ~3s push intervals — useless for cross-exchange lag analysis.
//! `sub.depth.full` pushes every ~100-300ms with real top-of-book.

use crate::error::{Error, Result};
use crate::exchanges::common::{backoff_secs, http_get_json, now_ns};
use crate::ilp::{BookTickerRow, BookTickerWriter};
use futures_util::{SinkExt, StreamExt};
use std::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

pub const WS_URL: &str = "wss://contract.mexc.com/edge";
pub const REST_CONTRACTS: &str = "https://contract.mexc.com/api/v1/contract/detail";

pub async fn fetch_usdt_perp_symbols(client: &reqwest::Client) -> Result<Vec<String>> {
    let v = http_get_json(client, REST_CONTRACTS).await?;
    let data = v
        .get("data")
        .and_then(|x| x.as_array())
        .ok_or_else(|| Error::Other("mexc contracts: no data".into()))?;
    let mut out = Vec::with_capacity(data.len());
    for c in data {
        let sym = c.get("symbol").and_then(|x| x.as_str()).unwrap_or("");
        let quote = c.get("quoteCoin").and_then(|x| x.as_str()).unwrap_or("");
        let state = c.get("state").and_then(|x| x.as_i64()).unwrap_or(-1);
        let api_allowed = c
            .get("apiAllowed")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        if quote == "USDT" && state == 0 && api_allowed && !sym.is_empty() {
            out.push(sym.to_string());
        }
    }
    out.sort();
    Ok(out)
}

pub async fn run_ws_task(id: usize, symbols: Vec<String>, writer: BookTickerWriter) {
    let mut attempt: u32 = 0;
    loop {
        info!("[mexc-{id}] connecting ({} syms)", symbols.len());
        match run_one_session(id, &symbols, &writer).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = backoff_secs(attempt);
                attempt = attempt.saturating_add(1);
                warn!("[mexc-{id}] disconnected: {e} — reconnect in {secs}s");
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
            "method": "sub.depth.full",
            "param": { "symbol": sym, "limit": 5 }
        });
        ws.send(Message::Text(payload.to_string())).await?;
        if i % 20 == 19 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    info!("[mexc-{id}] subscribed");

    let ping_interval = Duration::from_secs(10);
    let mut last_ping = Instant::now();

    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if last_ping.elapsed() >= ping_interval {
                    ws.send(Message::Text("{\"method\":\"ping\"}".into())).await?;
                    last_ping = Instant::now();
                }
            }
            msg = ws.next() => {
                let msg = match msg {
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
    }
}

fn parse_tick(text: &str) -> Option<BookTickerRow> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let channel = v.get("channel").and_then(|x| x.as_str()).unwrap_or("");
    if channel != "push.depth.full" {
        return None;
    }
    let symbol = v.get("symbol").and_then(|x| x.as_str())?.to_string();
    let data = v.get("data")?;
    let bids = data.get("bids")?.as_array()?;
    let asks = data.get("asks")?.as_array()?;
    let bid_l1 = bids.first()?.as_array()?;
    let ask_l1 = asks.first()?.as_array()?;
    let bid = bid_l1.first()?.as_f64()?;
    let ask = ask_l1.first()?.as_f64()?;
    if bid <= 0.0 || ask <= 0.0 {
        return None;
    }
    let bid_size = bid_l1.get(1).and_then(|x| x.as_f64());
    let ask_size = ask_l1.get(1).and_then(|x| x.as_f64());
    let server_ts_ns = v
        .get("ts")
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
        "MEXC: {} syms → {} WS connections",
        chunks.iter().map(|c| c.len()).sum::<usize>(),
        chunks.len()
    );
    for (idx, syms) in chunks.into_iter().enumerate() {
        let w = writer.clone();
        tokio::spawn(async move {
            run_ws_task(idx, syms, w).await;
            error!("[mexc-{idx}] task exited unexpectedly");
        });
    }
}

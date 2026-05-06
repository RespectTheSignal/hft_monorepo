//! Binance Futures aggTrade + partial-depth feeds for the alts that overlap with BingX.
//!
//! aggTrade WS:    wss://fstream.binance.com/stream?streams=<sym>@aggTrade/...
//! depth5@100ms:   wss://fstream.binance.com/stream?streams=<sym>@depth5@100ms/...
//! REST exchange:  https://fapi.binance.com/fapi/v1/exchangeInfo
//!
//! Data flow: subscribe to up to ~200 streams per WS connection (Binance allows
//! ~1024 but we stay conservative), parse trades / depth, write to QuestDB ILP.
//!
//! Symbol naming: stored as Binance native `BTCUSDT` (no underscore).

use crate::error::{Error, Result};
use crate::exchanges::common::{backoff_secs, http_get_json, now_ns};
use crate::ilp::{Depth5Row, Depth5Writer, TradeRow, TradeWriter};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

pub const REST_INFO: &str = "https://fapi.binance.com/fapi/v1/exchangeInfo";
pub const WS_FUTURES: &str = "wss://fstream.binance.com/stream";
/// Spot WS (used for aggTrade because gate1 IP is silently blocked from
/// fstream aggTrade — depth/bookTicker still works on fstream).
pub const WS_SPOT: &str = "wss://stream.binance.com:9443/stream";
pub const REST_BINGX: &str = "https://open-api.bingx.com/openApi/swap/v2/quote/contracts";

pub async fn fetch_usdt_perp_symbols(client: &reqwest::Client) -> Result<Vec<String>> {
    let v = http_get_json(client, REST_INFO).await?;
    let arr = v
        .get("symbols")
        .and_then(|x| x.as_array())
        .ok_or_else(|| Error::Other("binance exchangeInfo: no symbols".into()))?;
    let mut out = Vec::new();
    for s in arr {
        let sym = s.get("symbol").and_then(|x| x.as_str()).unwrap_or("");
        let ct = s.get("contractType").and_then(|x| x.as_str()).unwrap_or("");
        let st = s.get("status").and_then(|x| x.as_str()).unwrap_or("");
        let q  = s.get("quoteAsset").and_then(|x| x.as_str()).unwrap_or("");
        if ct == "PERPETUAL" && st == "TRADING" && q == "USDT" {
            out.push(sym.to_string());
        }
    }
    out.sort();
    Ok(out)
}

pub async fn fetch_bingx_bases(client: &reqwest::Client) -> Result<HashSet<String>> {
    let v = http_get_json(client, REST_BINGX).await?;
    let arr = v
        .get("data")
        .and_then(|x| x.as_array())
        .ok_or_else(|| Error::Other("bingx contracts: no data".into()))?;
    let mut out = HashSet::new();
    for c in arr {
        let sym = c.get("symbol").and_then(|x| x.as_str()).unwrap_or("");
        let cur = c.get("currency").and_then(|x| x.as_str()).unwrap_or("");
        let st  = c.get("status").and_then(|x| x.as_i64()).unwrap_or(0);
        if cur == "USDT" && st == 1 {
            if let Some(b) = sym.strip_suffix("-USDT") {
                out.insert(b.to_string());
            }
        }
    }
    Ok(out)
}

/// Returns the Binance perp symbols (BTCUSDT format) that also exist on BingX,
/// minus the configured majors.
pub async fn fetch_overlap_alts(client: &reqwest::Client, majors: &[&str]) -> Result<Vec<String>> {
    let bin = fetch_usdt_perp_symbols(client).await?;
    let bx_bases = fetch_bingx_bases(client).await?;
    let exclude: HashSet<&str> = majors.iter().copied().collect();
    let mut out: Vec<String> = bin
        .into_iter()
        .filter(|s| {
            let base = s.strip_suffix("USDT").unwrap_or("");
            !exclude.contains(base) && bx_bases.contains(base)
        })
        .collect();
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// aggTrade
// ---------------------------------------------------------------------------

pub async fn run_aggtrade_task(id: usize, symbols: Vec<String>, writer: TradeWriter) {
    let mut attempt: u32 = 0;
    loop {
        info!("[bin-trade-{id}] connecting ({} syms)", symbols.len());
        match aggtrade_session(id, &symbols, &writer).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = backoff_secs(attempt);
                attempt = attempt.saturating_add(1);
                warn!("[bin-trade-{id}] disconnected: {e} — reconnect in {secs}s");
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }
}

async fn aggtrade_session(id: usize, symbols: &[String], writer: &TradeWriter) -> Result<()> {
    let streams: Vec<String> = symbols
        .iter()
        .map(|s| format!("{}@aggTrade", s.to_lowercase()))
        .collect();
    let url = format!("{}?streams={}", WS_SPOT, streams.join("/"));
    let (mut ws, _resp) = connect_async(&url).await?;
    info!("[bin-trade-{id}] subscribed");

    loop {
        let msg = match ws.next().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => return Err(e.into()),
            None => return Err(Error::Other("ws stream ended".into())),
        };
        match msg {
            Message::Text(text) => {
                if let Some(row) = parse_aggtrade(&text) {
                    writer.submit(row);
                } else {
                    debug!("[bin-trade-{id}] non-trade: {}", &text[..text.len().min(180)]);
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

fn parse_aggtrade(text: &str) -> Option<TradeRow> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let data = v.get("data")?;
    let event = data.get("e").and_then(|x| x.as_str()).unwrap_or("");
    if event != "aggTrade" {
        return None;
    }
    let symbol = data.get("s").and_then(|x| x.as_str())?.to_string();
    let price: f64 = data.get("p").and_then(|x| x.as_str()).and_then(|s| s.parse().ok())?;
    let qty:   f64 = data.get("q").and_then(|x| x.as_str()).and_then(|s| s.parse().ok())?;
    if price <= 0.0 || qty <= 0.0 {
        return None;
    }
    let m = data.get("m").and_then(|x| x.as_bool()).unwrap_or(false);
    let trade_id = data.get("a").and_then(|x| x.as_i64()).unwrap_or(0);
    let server_ts_ns = data
        .get("T")
        .and_then(|x| x.as_i64())
        .map(|ms| ms * 1_000_000)
        .unwrap_or_else(now_ns);
    Some(TradeRow {
        symbol,
        price,
        quantity: qty,
        usd_value: price * qty,
        is_buyer_maker: m,
        trade_id,
        server_ts_ns,
    })
}

pub fn spawn_aggtrade_all(symbols: Vec<String>, streams_per_conn: usize, writer: TradeWriter) {
    let chunks: Vec<Vec<String>> = symbols.chunks(streams_per_conn).map(|c| c.to_vec()).collect();
    info!(
        "Binance aggTrade: {} syms → {} WS connections",
        chunks.iter().map(|c| c.len()).sum::<usize>(),
        chunks.len()
    );
    for (idx, syms) in chunks.into_iter().enumerate() {
        let w = writer.clone();
        tokio::spawn(async move {
            run_aggtrade_task(idx, syms, w).await;
            error!("[bin-trade-{idx}] task exited");
        });
    }
}

// ---------------------------------------------------------------------------
// depth5@100ms
// ---------------------------------------------------------------------------

pub async fn run_depth_task(id: usize, symbols: Vec<String>, writer: Depth5Writer) {
    let mut attempt: u32 = 0;
    loop {
        info!("[bin-depth-{id}] connecting ({} syms)", symbols.len());
        match depth_session(id, &symbols, &writer).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = backoff_secs(attempt);
                attempt = attempt.saturating_add(1);
                warn!("[bin-depth-{id}] disconnected: {e} — reconnect in {secs}s");
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }
}

async fn depth_session(id: usize, symbols: &[String], writer: &Depth5Writer) -> Result<()> {
    let streams: Vec<String> = symbols
        .iter()
        .map(|s| format!("{}@depth5@100ms", s.to_lowercase()))
        .collect();
    let url = format!("{}?streams={}", WS_FUTURES, streams.join("/"));
    let (mut ws, _resp) = connect_async(&url).await?;
    info!("[bin-depth-{id}] subscribed");

    loop {
        let msg = match ws.next().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => return Err(e.into()),
            None => return Err(Error::Other("ws stream ended".into())),
        };
        match msg {
            Message::Text(text) => {
                if let Some(row) = parse_depth5(&text) {
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

fn parse_depth5(text: &str) -> Option<Depth5Row> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let data = v.get("data")?;
    let symbol = data.get("s").and_then(|x| x.as_str())?.to_string();
    let bids_arr = data.get("b")?.as_array()?;
    let asks_arr = data.get("a")?.as_array()?;
    let mut bids = [(0.0_f64, 0.0_f64); 5];
    let mut asks = [(0.0_f64, 0.0_f64); 5];
    for (i, lvl) in bids_arr.iter().take(5).enumerate() {
        let arr = lvl.as_array()?;
        bids[i].0 = arr.first()?.as_str()?.parse().ok()?;
        bids[i].1 = arr.get(1)?.as_str()?.parse().ok()?;
    }
    for (i, lvl) in asks_arr.iter().take(5).enumerate() {
        let arr = lvl.as_array()?;
        asks[i].0 = arr.first()?.as_str()?.parse().ok()?;
        asks[i].1 = arr.get(1)?.as_str()?.parse().ok()?;
    }
    if bids[0].0 <= 0.0 || asks[0].0 <= 0.0 {
        return None;
    }
    let server_ts_ns = data
        .get("T")
        .and_then(|x| x.as_i64())
        .or_else(|| data.get("E").and_then(|x| x.as_i64()))
        .map(|ms| ms * 1_000_000)
        .unwrap_or_else(now_ns);
    Some(Depth5Row {
        symbol,
        bids,
        asks,
        server_ts_ns,
    })
}

pub fn spawn_depth_all(symbols: Vec<String>, streams_per_conn: usize, writer: Depth5Writer) {
    let chunks: Vec<Vec<String>> = symbols.chunks(streams_per_conn).map(|c| c.to_vec()).collect();
    info!(
        "Binance depth5: {} syms → {} WS connections",
        chunks.iter().map(|c| c.len()).sum::<usize>(),
        chunks.len()
    );
    for (idx, syms) in chunks.into_iter().enumerate() {
        let w = writer.clone();
        tokio::spawn(async move {
            run_depth_task(idx, syms, w).await;
            error!("[bin-depth-{idx}] task exited");
        });
    }
}

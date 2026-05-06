//! HashKey Global derivatives book ticker (uses depth topic, takes L1).
//!
//! WS:    wss://stream-glb.hashkey.com/quote/ws/v1
//! REST:  https://api-glb.hashkey.com/api/v1/exchangeInfo  (contracts[])
//! Sub:   {"symbol":"BTCUSDT-PERPETUAL","topic":"depth","event":"sub",
//!         "params":{"binary":false,"limit":5}}
//! Tick:  data[0].b[0] / data[0].a[0] / data[0].t (ms)
//! Ping:  none required (server pings)

use crate::error::{Error, Result};
use crate::exchanges::common::{backoff_secs, http_get_json, now_ns, parse_f64};
use crate::ilp::{BookTickerRow, BookTickerWriter};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

pub const WS_URL: &str = "wss://stream-glb.hashkey.com/quote/ws/v1";
pub const REST_INFO: &str = "https://api-glb.hashkey.com/api/v1/exchangeInfo";

pub async fn fetch_perp_symbols(client: &reqwest::Client) -> Result<Vec<String>> {
    let v = http_get_json(client, REST_INFO).await?;
    let contracts = v
        .get("contracts")
        .and_then(|x| x.as_array())
        .ok_or_else(|| Error::Other("hashkey exchangeInfo: no contracts".into()))?;
    let mut out = Vec::new();
    for c in contracts {
        let sym = c.get("symbol").and_then(|x| x.as_str()).unwrap_or("");
        let status = c.get("status").and_then(|x| x.as_str()).unwrap_or("");
        if status == "TRADING" && !sym.is_empty() {
            out.push(sym.to_string());
        }
    }
    out.sort();
    Ok(out)
}

pub async fn run_ws_task(id: usize, symbols: Vec<String>, writer: BookTickerWriter) {
    let mut attempt: u32 = 0;
    loop {
        info!("[hashkey-{id}] connecting ({} syms)", symbols.len());
        match run_one_session(id, &symbols, &writer).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = backoff_secs(attempt);
                attempt = attempt.saturating_add(1);
                warn!("[hashkey-{id}] disconnected: {e} — reconnect in {secs}s");
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

    for sym in symbols {
        let payload = serde_json::json!({
            "symbol": sym,
            "topic": "depth",
            "event": "sub",
            "params": { "binary": false, "limit": 5 }
        });
        ws.send(Message::Text(payload.to_string())).await?;
    }
    info!("[hashkey-{id}] subscribed");

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
                } else if text.contains("\"ping\"") {
                    // server ping — answer with pong (echo timestamp if present)
                    let v: serde_json::Value =
                        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
                    let pong = serde_json::json!({
                        "pong": v.get("ping").cloned().unwrap_or(serde_json::Value::Null)
                    });
                    ws.send(Message::Text(pong.to_string())).await?;
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
    if v.get("topic").and_then(|x| x.as_str()) != Some("depth") {
        return None;
    }
    let symbol = v.get("symbol").and_then(|x| x.as_str())?.to_string();
    let data_arr = v.get("data")?.as_array()?;
    let snap = data_arr.first()?;
    let bids = snap.get("b")?.as_array()?;
    let asks = snap.get("a")?.as_array()?;
    let bid = bids.first()?.as_array()?.first()?.as_str().and_then(parse_f64)?;
    let ask = asks.first()?.as_array()?.first()?.as_str().and_then(parse_f64)?;
    if bid <= 0.0 || ask <= 0.0 {
        return None;
    }
    let bid_size = bids
        .first()
        .and_then(|x| x.as_array())
        .and_then(|a| a.get(1))
        .and_then(|x| x.as_str())
        .and_then(parse_f64);
    let ask_size = asks
        .first()
        .and_then(|x| x.as_array())
        .and_then(|a| a.get(1))
        .and_then(|x| x.as_str())
        .and_then(parse_f64);
    let server_ts_ns = snap
        .get("t")
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

pub fn spawn_all(symbols: Vec<String>, _topics_per_conn: usize, writer: BookTickerWriter) {
    // HashKey has very few contracts — single connection is plenty.
    info!("HashKey: {} syms → 1 WS connection", symbols.len());
    let w = writer.clone();
    tokio::spawn(async move {
        run_ws_task(0, symbols, w).await;
        error!("[hashkey-0] task exited unexpectedly");
    });
    let _ = writer;
}

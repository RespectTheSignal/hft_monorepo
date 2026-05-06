//! Backpack Exchange perpetuals book ticker.
//!
//! WS:    wss://ws.backpack.exchange
//! REST:  https://api.backpack.exchange/api/v1/markets  (filter marketType=PERP, orderBookState=Open)
//! Sub:   {"method":"SUBSCRIBE","params":["bookTicker.<sym>"],"id":<n>}
//! Tick:  data.b / data.a / data.T (microseconds — divide by 1000 to ms)
//! Ping:  none required (server pings)

use crate::error::{Error, Result};
use crate::exchanges::common::{backoff_secs, http_get_json, now_ns, parse_f64};
use crate::ilp::{BookTickerRow, BookTickerWriter};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

pub const WS_URL: &str = "wss://ws.backpack.exchange";
pub const REST_MARKETS: &str = "https://api.backpack.exchange/api/v1/markets";

pub async fn fetch_perp_symbols(client: &reqwest::Client) -> Result<Vec<String>> {
    let v = http_get_json(client, REST_MARKETS).await?;
    let arr = v
        .as_array()
        .ok_or_else(|| Error::Other("backpack markets: not an array".into()))?;
    let mut out = Vec::new();
    for m in arr {
        let mt = m.get("marketType").and_then(|x| x.as_str()).unwrap_or("");
        let state = m
            .get("orderBookState")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let sym = m.get("symbol").and_then(|x| x.as_str()).unwrap_or("");
        if mt == "PERP" && state == "Open" && !sym.is_empty() {
            out.push(sym.to_string());
        }
    }
    out.sort();
    Ok(out)
}

pub async fn run_ws_task(id: usize, symbols: Vec<String>, writer: BookTickerWriter) {
    let mut attempt: u32 = 0;
    loop {
        info!("[backpack-{id}] connecting ({} syms)", symbols.len());
        match run_one_session(id, &symbols, &writer).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = backoff_secs(attempt);
                attempt = attempt.saturating_add(1);
                warn!("[backpack-{id}] disconnected: {e} — reconnect in {secs}s");
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

    for (i, chunk) in symbols.chunks(50).enumerate() {
        let params: Vec<String> = chunk.iter().map(|s| format!("bookTicker.{s}")).collect();
        let payload = serde_json::json!({
            "method": "SUBSCRIBE",
            "params": params,
            "id": i + 1,
        });
        ws.send(Message::Text(payload.to_string())).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    info!("[backpack-{id}] subscribed");

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
    let stream = v.get("stream").and_then(|x| x.as_str())?;
    if !stream.starts_with("bookTicker.") {
        return None;
    }
    let data = v.get("data")?;
    let symbol = data
        .get("s")
        .and_then(|x| x.as_str())
        .or_else(|| stream.strip_prefix("bookTicker."))?;
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
        .map(|t| {
            // microseconds → nanoseconds
            if t > 1_000_000_000_000_000 { t * 1_000 } else { t * 1_000_000 }
        })
        .unwrap_or_else(now_ns);
    Some(BookTickerRow {
        symbol: symbol.to_string(),
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
        "Backpack: {} syms → {} WS connections",
        chunks.iter().map(|c| c.len()).sum::<usize>(),
        chunks.len()
    );
    for (idx, syms) in chunks.into_iter().enumerate() {
        let w = writer.clone();
        tokio::spawn(async move {
            run_ws_task(idx, syms, w).await;
            error!("[backpack-{idx}] task exited unexpectedly");
        });
    }
}

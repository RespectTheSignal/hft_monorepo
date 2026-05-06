//! OKX V5 public bbo-tbt feed (USDT-SWAP perpetuals).
//!
//! WS:    wss://ws.okx.com:8443/ws/v5/public
//! REST:  https://www.okx.com/api/v5/public/instruments?instType=SWAP
//! Sub:   {"op":"subscribe","args":[{"channel":"bbo-tbt","instId":"<inst>"}]}
//! Tick:  data[0].bids[0][0] / asks[0][0] / ts (ms)
//! Ping:  plain text "ping" every 25s; server replies "pong" (ignore)

use crate::error::{Error, Result};
use crate::exchanges::common::{backoff_secs, http_get_json, now_ns, parse_f64};
use crate::ilp::{BookTickerRow, BookTickerWriter};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

pub const WS_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";
pub const REST_INSTRUMENTS: &str =
    "https://www.okx.com/api/v5/public/instruments?instType=SWAP";

pub async fn fetch_usdt_swap_instruments(client: &reqwest::Client) -> Result<Vec<String>> {
    let v = http_get_json(client, REST_INSTRUMENTS).await?;
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| Error::Other("okx instruments: no data array".into()))?;
    let mut out = Vec::with_capacity(data.len());
    for inst in data {
        let inst_id = inst.get("instId").and_then(|x| x.as_str()).unwrap_or("");
        let state = inst.get("state").and_then(|x| x.as_str()).unwrap_or("");
        let settle = inst.get("settleCcy").and_then(|x| x.as_str()).unwrap_or("");
        if state == "live" && settle == "USDT" && inst_id.ends_with("-USDT-SWAP") {
            out.push(inst_id.to_string());
        }
    }
    out.sort();
    Ok(out)
}

pub async fn run_ws_task(
    id: usize,
    instruments: Vec<String>,
    writer: BookTickerWriter,
) {
    let mut attempt: u32 = 0;
    loop {
        info!("[okx-{id}] connecting ({} insts)", instruments.len());
        match run_one_session(id, &instruments, &writer).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = backoff_secs(attempt);
                attempt = attempt.saturating_add(1);
                warn!("[okx-{id}] disconnected: {e} — reconnect in {secs}s");
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }
}

async fn run_one_session(
    id: usize,
    instruments: &[String],
    writer: &BookTickerWriter,
) -> Result<()> {
    let (mut ws, _resp) = connect_async(WS_URL).await?;

    // Subscribe in chunks (OKX accepts large args arrays but we keep it modest).
    for chunk in instruments.chunks(50) {
        let args: Vec<serde_json::Value> = chunk
            .iter()
            .map(|inst| serde_json::json!({"channel":"bbo-tbt","instId":inst}))
            .collect();
        let payload = serde_json::json!({"op":"subscribe","args":args});
        ws.send(Message::Text(payload.to_string())).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    info!("[okx-{id}] subscribed");

    let mut last_ping = std::time::Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if last_ping.elapsed() >= Duration::from_secs(25) {
                    ws.send(Message::Text("ping".into())).await?;
                    last_ping = std::time::Instant::now();
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
                        if text == "pong" { continue; }
                        if let Some(rows) = parse_tick(&text) {
                            for r in rows { writer.submit(r); }
                        } else {
                            // event/error frames
                            debug!("[okx-{id}] non-tick: {}", &text[..text.len().min(180)]);
                        }
                    }
                    Message::Binary(_) => {}
                    Message::Ping(p) => { ws.send(Message::Pong(p)).await?; }
                    Message::Pong(_) => {}
                    Message::Close(f) => {
                        return Err(Error::Other(format!("server close: {:?}", f)));
                    }
                    Message::Frame(_) => {}
                }
            }
        }
    }
}

/// Returns Vec<BookTickerRow> if this is a bbo-tbt data frame; None otherwise.
fn parse_tick(text: &str) -> Option<Vec<BookTickerRow>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let arg = v.get("arg")?;
    let channel = arg.get("channel").and_then(|c| c.as_str())?;
    if channel != "bbo-tbt" {
        return None;
    }
    let data = v.get("data")?.as_array()?;
    let recv_ns = now_ns();
    let mut out = Vec::with_capacity(data.len());
    for row in data {
        let inst_id = row
            .get("instId")
            .and_then(|x| x.as_str())
            .or_else(|| arg.get("instId").and_then(|x| x.as_str()))?;
        let bids = row.get("bids")?.as_array()?;
        let asks = row.get("asks")?.as_array()?;
        let bid = bids.first()?.as_array()?.first()?.as_str().and_then(parse_f64)?;
        let ask = asks.first()?.as_array()?.first()?.as_str().and_then(parse_f64)?;
        let bid_size = bids
            .first()
            .and_then(|b| b.as_array())
            .and_then(|a| a.get(1))
            .and_then(|x| x.as_str())
            .and_then(parse_f64);
        let ask_size = asks
            .first()
            .and_then(|b| b.as_array())
            .and_then(|a| a.get(1))
            .and_then(|x| x.as_str())
            .and_then(parse_f64);
        let server_ts_ns = row
            .get("ts")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .map(|ms| ms * 1_000_000)
            .unwrap_or(recv_ns);
        if bid <= 0.0 || ask <= 0.0 {
            continue;
        }
        out.push(BookTickerRow {
            symbol: inst_id.to_string(),
            bid,
            ask,
            bid_size,
            ask_size,
            server_ts_ns,
        recv_ts_ns: None,
    });
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn spawn_all(instruments: Vec<String>, topics_per_conn: usize, writer: BookTickerWriter) {
    let chunks: Vec<Vec<String>> = instruments
        .chunks(topics_per_conn)
        .map(|c| c.to_vec())
        .collect();
    let total = chunks.len();
    info!("OKX: {} insts → {total} WS connections", chunks.iter().map(|c| c.len()).sum::<usize>());
    for (idx, insts) in chunks.into_iter().enumerate() {
        let w = writer.clone();
        tokio::spawn(async move {
            run_ws_task(idx, insts, w).await;
            error!("[okx-{idx}] task exited unexpectedly");
        });
    }
}

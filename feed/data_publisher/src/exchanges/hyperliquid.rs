//! Hyperliquid public BBO feed → QuestDB `hyperliquid_bookticker`.
//!
//! WS:    wss://api.hyperliquid.xyz/ws
//! REST:  POST https://api.hyperliquid.xyz/info  body {"type":"meta"}
//! Sub:   {"method":"subscribe","subscription":{"type":"bbo","coin":"<COIN>"}}
//! Frame: {"channel":"bbo","data":{"coin":"BTC","time":<ms>,"bbo":[{"px":..,"sz":..,"n":..},
//!                                                                {"px":..,"sz":..,"n":..}]}}
//!         bbo[0] = best bid, bbo[1] = best ask (HL convention).
//! Ping:  HL sends server pings transparently; tungstenite auto-pongs.

use crate::error::{Error, Result};
use crate::exchanges::common::{backoff_secs, http_post_json_body, now_ns, parse_f64};
use crate::ilp::{BookTickerRow, BookTickerWriter};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

pub const WS_URL: &str = "wss://api.hyperliquid.xyz/ws";
pub const WS_URL_UI: &str = "wss://api-ui.hyperliquid.xyz/ws";
pub const REST_INFO: &str = "https://api.hyperliquid.xyz/info";

pub async fn fetch_active_coins(client: &reqwest::Client) -> Result<Vec<String>> {
    let body = serde_json::json!({"type":"meta"});
    let v = http_post_json_body(client, REST_INFO, &body).await?;
    let universe = v
        .get("universe")
        .and_then(|u| u.as_array())
        .ok_or_else(|| Error::Other("hl meta: no universe array".into()))?;
    let mut out = Vec::with_capacity(universe.len());
    for c in universe {
        let name = c.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let delisted = c.get("isDelisted").and_then(|x| x.as_bool()).unwrap_or(false);
        if !name.is_empty() && !delisted {
            out.push(name.to_string());
        }
    }
    out.sort();
    Ok(out)
}

pub async fn run_ws_task(id: usize, ws_url: &'static str, coins: Vec<String>, writer: BookTickerWriter) {
    let mut attempt: u32 = 0;
    loop {
        info!("[hl-{id}] connecting {} ({} coins)", ws_url, coins.len());
        match run_one_session(id, ws_url, &coins, &writer).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = backoff_secs(attempt);
                attempt = attempt.saturating_add(1);
                warn!("[hl-{id}] disconnected: {e} — reconnect in {secs}s");
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }
}

async fn run_one_session(
    id: usize,
    ws_url: &str,
    coins: &[String],
    writer: &BookTickerWriter,
) -> Result<()> {
    let (mut ws, _resp) = connect_async(ws_url).await?;

    // HL accepts one subscription per message. Throttle slightly to avoid
    // server-side flow control.
    for (i, coin) in coins.iter().enumerate() {
        let payload = serde_json::json!({
            "method": "subscribe",
            "subscription": {"type":"bbo","coin":coin}
        });
        ws.send(Message::Text(payload.to_string())).await?;
        if i % 20 == 19 {
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
    info!("[hl-{id}] subscribed {} coins", coins.len());

    loop {
        let msg = match ws.next().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => return Err(e.into()),
            None => return Err(Error::Other("ws stream ended".into())),
        };
        match msg {
            Message::Text(text) => {
                if let Some(row) = parse_bbo(&text) {
                    writer.submit(row);
                } else {
                    debug!("[hl-{id}] non-bbo: {}", trim(&text, 160));
                }
            }
            Message::Binary(_) => continue,
            Message::Ping(p) => {
                let _ = ws.send(Message::Pong(p)).await;
            }
            Message::Pong(_) => {}
            Message::Close(f) => {
                return Err(Error::Other(format!("server close: {:?}", f)));
            }
            Message::Frame(_) => {}
        }
    }
}

fn parse_bbo(text: &str) -> Option<BookTickerRow> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("channel")?.as_str()? != "bbo" {
        return None;
    }
    let data = v.get("data")?;
    let coin = data.get("coin")?.as_str()?.to_string();
    let bbo = data.get("bbo")?.as_array()?;
    if bbo.len() < 2 {
        return None;
    }
    // HL sometimes sends nulls when book has only one side; both required.
    let bid_obj = bbo.first()?.as_object()?;
    let ask_obj = bbo.get(1)?.as_object()?;
    let bid = bid_obj.get("px")?.as_str().and_then(parse_f64)?;
    let ask = ask_obj.get("px")?.as_str().and_then(parse_f64)?;
    if bid <= 0.0 || ask <= 0.0 {
        return None;
    }
    let bid_size = bid_obj.get("sz").and_then(|x| x.as_str()).and_then(parse_f64);
    let ask_size = ask_obj.get("sz").and_then(|x| x.as_str()).and_then(parse_f64);
    let server_ts_ns = data
        .get("time")
        .and_then(|x| x.as_i64())
        .map(|ms| ms * 1_000_000)
        .unwrap_or_else(now_ns);
    Some(BookTickerRow {
        symbol: coin,
        bid,
        ask,
        bid_size,
        ask_size,
        server_ts_ns,
        recv_ts_ns: Some(now_ns()),
    })
}

fn trim(s: &str, n: usize) -> String {
    if s.len() > n { format!("{}…", &s[..n]) } else { s.to_string() }
}

pub fn spawn_all(
    ws_url: &'static str,
    coins: Vec<String>,
    topics_per_conn: usize,
    writer: BookTickerWriter,
) {
    let chunks: Vec<Vec<String>> = coins.chunks(topics_per_conn).map(|c| c.to_vec()).collect();
    let total = chunks.len();
    info!(
        "HL[{ws_url}]: {} coins → {total} WS connections",
        chunks.iter().map(|c| c.len()).sum::<usize>()
    );
    for (idx, c) in chunks.into_iter().enumerate() {
        let w = writer.clone();
        tokio::spawn(async move {
            run_ws_task(idx, ws_url, c, w).await;
            error!("[hl-{idx}] task exited unexpectedly");
        });
    }
}

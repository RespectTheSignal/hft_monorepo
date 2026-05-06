//! KuCoin Futures USDT-margined book ticker (tickerV2).
//!
//! Bullet:  POST https://api-futures.kucoin.com/api/v1/bullet-public
//!          → {endpoint, pingInterval}; WS URL = endpoint?token=&connectId=
//! REST:    https://api-futures.kucoin.com/api/v1/contracts/active
//! Sub:     {"id":<n>,"type":"subscribe","topic":"/contractMarket/tickerV2:<sym>",
//!           "privateChannel":false,"response":true}
//! Tick:    data.bestBidPrice / bestAskPrice / ts (ns — 19 digits)
//! Ping:    {"id":<n>,"type":"ping"} every pingInterval ms

use crate::error::{Error, Result};
use crate::exchanges::common::{backoff_secs, http_get_json, http_post_json, now_ns, parse_f64};
use crate::ilp::{BookTickerRow, BookTickerWriter};
use futures_util::{SinkExt, StreamExt};
use std::time::{Duration, Instant};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

pub const REST_CONTRACTS: &str = "https://api-futures.kucoin.com/api/v1/contracts/active";
pub const REST_BULLET: &str = "https://api-futures.kucoin.com/api/v1/bullet-public";

pub async fn fetch_usdt_perp_symbols(client: &reqwest::Client) -> Result<Vec<String>> {
    let v = http_get_json(client, REST_CONTRACTS).await?;
    let data = v
        .get("data")
        .and_then(|x| x.as_array())
        .ok_or_else(|| Error::Other("kucoin contracts: no data".into()))?;
    let mut out = Vec::with_capacity(data.len());
    for c in data {
        let sym = c.get("symbol").and_then(|x| x.as_str()).unwrap_or("");
        let quote = c.get("quoteCurrency").and_then(|x| x.as_str()).unwrap_or("");
        let settle = c.get("settleCurrency").and_then(|x| x.as_str()).unwrap_or("");
        let status = c.get("status").and_then(|x| x.as_str()).unwrap_or("");
        if quote == "USDT" && settle == "USDT" && status == "Open" && !sym.is_empty() {
            out.push(sym.to_string());
        }
    }
    out.sort();
    Ok(out)
}

#[derive(Clone)]
struct Bullet {
    endpoint: String,
    ping_interval_ms: u64,
}

async fn fetch_bullet(client: &reqwest::Client) -> Result<(Bullet, String)> {
    let v = http_post_json(client, REST_BULLET).await?;
    let data = v
        .get("data")
        .ok_or_else(|| Error::Other("kucoin bullet: no data".into()))?;
    let token = data
        .get("token")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::Other("kucoin bullet: no token".into()))?
        .to_string();
    let server = data
        .get("instanceServers")
        .and_then(|x| x.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| Error::Other("kucoin bullet: no instanceServers".into()))?;
    let endpoint = server
        .get("endpoint")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::Other("kucoin bullet: no endpoint".into()))?
        .to_string();
    let ping_interval_ms = server
        .get("pingInterval")
        .and_then(|x| x.as_u64())
        .unwrap_or(18000);
    Ok((Bullet { endpoint, ping_interval_ms }, token))
}

pub async fn run_ws_task(id: usize, symbols: Vec<String>, writer: BookTickerWriter) {
    let mut attempt: u32 = 0;
    let client = reqwest::Client::builder()
        .user_agent("data_publisher/kucoin")
        .build()
        .expect("reqwest");
    loop {
        info!("[kucoin-{id}] connecting ({} syms)", symbols.len());
        match run_one_session(id, &symbols, &writer, &client).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = backoff_secs(attempt);
                attempt = attempt.saturating_add(1);
                warn!("[kucoin-{id}] disconnected: {e} — reconnect in {secs}s");
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }
}

async fn run_one_session(
    id: usize,
    symbols: &[String],
    writer: &BookTickerWriter,
    client: &reqwest::Client,
) -> Result<()> {
    let (bullet, token) = fetch_bullet(client).await?;
    let connect_id = now_ns();
    let url = format!("{}?token={}&connectId={}", bullet.endpoint, token, connect_id);

    let (mut ws, _resp) = connect_async(&url).await?;

    // KuCoin sends a {"type":"welcome"} frame first; we don't have to wait but
    // we do need to subscribe after connect. A short delay avoids racing the
    // welcome handler.
    tokio::time::sleep(Duration::from_millis(200)).await;

    for chunk in symbols.chunks(100) {
        let topic = format!("/contractMarket/tickerV2:{}", chunk.join(","));
        let payload = serde_json::json!({
            "id": now_ns(),
            "type": "subscribe",
            "topic": topic,
            "privateChannel": false,
            "response": false,
        });
        ws.send(Message::Text(payload.to_string())).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    info!("[kucoin-{id}] subscribed (ping every {}ms)", bullet.ping_interval_ms);

    let ping_interval = Duration::from_millis(bullet.ping_interval_ms);
    let mut last_ping = Instant::now();

    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if last_ping.elapsed() >= ping_interval {
                    let payload = serde_json::json!({"id": now_ns(), "type": "ping"});
                    ws.send(Message::Text(payload.to_string())).await?;
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
    if v.get("type").and_then(|x| x.as_str()) != Some("message") {
        return None;
    }
    let topic = v.get("topic").and_then(|x| x.as_str())?;
    if !topic.starts_with("/contractMarket/tickerV2:") {
        return None;
    }
    let data = v.get("data")?;
    let symbol = data
        .get("symbol")
        .and_then(|x| x.as_str())
        .or_else(|| topic.rsplit(':').next())?
        .to_string();
    let bid_v = data.get("bestBidPrice")?;
    let ask_v = data.get("bestAskPrice")?;
    let bid = match bid_v {
        serde_json::Value::String(s) => parse_f64(s)?,
        serde_json::Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    let ask = match ask_v {
        serde_json::Value::String(s) => parse_f64(s)?,
        serde_json::Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    if bid <= 0.0 || ask <= 0.0 {
        return None;
    }
    let bid_size = data.get("bestBidSize").and_then(json_to_f64);
    let ask_size = data.get("bestAskSize").and_then(json_to_f64);
    let server_ts_ns = data
        .get("ts")
        .and_then(|x| x.as_i64())
        .map(|t| {
            // ts may be ms (13 digits), µs (16), or ns (19) — normalize to ns.
            if t > 1_000_000_000_000_000_000 { t }
            else if t > 1_000_000_000_000_000 { t * 1_000 }
            else if t > 1_000_000_000_000 { t * 1_000_000 }
            else { t * 1_000_000_000 }
        })
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

fn json_to_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::String(s) => parse_f64(s),
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

pub fn spawn_all(symbols: Vec<String>, topics_per_conn: usize, writer: BookTickerWriter) {
    let chunks: Vec<Vec<String>> = symbols.chunks(topics_per_conn).map(|c| c.to_vec()).collect();
    info!(
        "KuCoin: {} syms → {} WS connections",
        chunks.iter().map(|c| c.len()).sum::<usize>(),
        chunks.len()
    );
    for (idx, syms) in chunks.into_iter().enumerate() {
        let w = writer.clone();
        tokio::spawn(async move {
            run_ws_task(idx, syms, w).await;
            error!("[kucoin-{idx}] task exited unexpectedly");
        });
    }
}

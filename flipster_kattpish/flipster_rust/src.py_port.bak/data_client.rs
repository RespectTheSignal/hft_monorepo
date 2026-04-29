// Flipster dual-feed data client:
//   api feed: wss://trading-api.flipster.io/api/v1/stream  — HMAC-signed, ticker.{sym}
//   web feed: wss://api.flipster.io/api/v2/stream/r230522  — cookie-auth, market/orderbooks-v2
//
// Both feeds push into a shared DataCache. Mirrors Python flipster_latency_pick.py api_feed/web_feed.

use crate::data_cache::DataCache;
use crate::types::BookTickerC;
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};
use tracing::{info, warn};

const API_WS_URL: &str = "wss://trading-api.flipster.io/api/v1/stream";
const WEB_WS_URL: &str = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription";
const CDP_VERSION_URL_FMT: &str = "http://localhost:{port}/json/version";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/141.0.0.0";

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct FlipsterDataClient {
    cache: Arc<DataCache>,
    api_key: String,
    api_secret: String,
    cdp_port: u16,
    shutdown: Arc<Notify>,
}

impl FlipsterDataClient {
    pub fn new(cache: Arc<DataCache>, api_key: String, api_secret: String, cdp_port: u16) -> Self {
        Self {
            cache,
            api_key,
            api_secret,
            cdp_port,
            shutdown: Arc::new(Notify::new()),
        }
    }

    pub fn stop(&self) {
        self.shutdown.notify_waiters();
    }

    /// Spawns api + web feeds as independent reconnecting tasks.
    pub fn start(&self, symbols: Vec<String>) {
        let api_symbols = symbols.clone();
        let api = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = api.api_feed_loop(&api_symbols).await {
                    warn!("[api] feed error: {} — retrying in 3s", e);
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                    _ = api.shutdown.notified() => break,
                }
            }
        });

        let web_symbols = symbols;
        let web = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = web.web_feed_loop(&web_symbols).await {
                    warn!("[web] feed error: {} — retrying in 3s", e);
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                    _ = web.shutdown.notified() => break,
                }
            }
        });
    }

    fn sign_api(&self, expires: i64) -> Result<String> {
        let msg = format!("GET/api/v1/stream{}", expires);
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes())
            .map_err(|e| anyhow!("hmac init: {}", e))?;
        mac.update(msg.as_bytes());
        Ok(hex::encode(mac.finalize().into_bytes()))
    }

    async fn api_feed_loop(&self, symbols: &[String]) -> Result<()> {
        let expires = Utc::now().timestamp() + 3600;
        let sig = self.sign_api(expires)?;

        let mut req = API_WS_URL.into_client_request()?;
        let h = req.headers_mut();
        h.insert("api-key", self.api_key.parse()?);
        h.insert("api-expires", expires.to_string().parse()?);
        h.insert("api-signature", sig.parse()?);
        h.insert("User-Agent", USER_AGENT.parse()?);

        let (ws, _) = connect_async(req).await.context("connect api ws")?;
        info!("[api] connected, subscribing {} symbols", symbols.len());
        let (mut write, mut read) = ws.split();

        // Subscribe in chunks of 50 (Flipster limit).
        for chunk in symbols.chunks(50) {
            let topics: Vec<String> = chunk.iter().map(|s| format!("ticker.{}", s)).collect();
            let msg = json!({ "op": "subscribe", "args": topics });
            write
                .send(Message::Text(msg.to_string().into()))
                .await
                .context("send api subscribe")?;
        }

        loop {
            tokio::select! {
                _ = self.shutdown.notified() => return Ok(()),
                msg = read.next() => {
                    let msg = match msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => return Err(anyhow!("api ws recv: {}", e)),
                        None => return Err(anyhow!("api ws closed")),
                    };
                    match msg {
                        Message::Text(text) => self.handle_api_text(&text),
                        Message::Ping(p) => {
                            let _ = write.send(Message::Pong(p)).await;
                        }
                        Message::Close(_) => return Err(anyhow!("api ws closed by server")),
                        _ => {}
                    }
                }
            }
        }
    }

    fn handle_api_text(&self, text: &str) {
        let d: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };
        let topic = d.get("topic").and_then(|v| v.as_str()).unwrap_or("");
        if !topic.starts_with("ticker.") {
            return;
        }
        let sym = &topic[7..];
        let recv_ms = Utc::now().timestamp_millis();
        let server_ms = d.get("ts").and_then(|v| v.as_i64()).unwrap_or(recv_ms);

        let data = match d.get("data").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => return,
        };
        for row in data {
            let rows = match row.get("rows").and_then(|v| v.as_array()) {
                Some(a) => a,
                None => continue,
            };
            for r in rows {
                let bid = parse_num(r.get("bidPrice"));
                let ask = parse_num(r.get("askPrice"));
                if bid <= 0.0 || ask <= 0.0 {
                    continue;
                }
                let tk = BookTickerC {
                    bid_price: bid,
                    ask_price: ask,
                    bid_size: 0.0,
                    ask_size: 0.0,
                    server_time: server_ms,
                };
                self.cache.update_api_bookticker(sym, tk, recv_ms);
            }
        }
    }

    async fn web_feed_loop(&self, symbols: &[String]) -> Result<()> {
        let cookies = fetch_cookies(self.cdp_port).await?;
        let cookie_hdr = cookies
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("; ");

        let mut req = WEB_WS_URL.into_client_request()?;
        let h = req.headers_mut();
        h.insert("Origin", "https://flipster.io".parse()?);
        h.insert("Cookie", cookie_hdr.parse()?);
        h.insert("User-Agent", USER_AGENT.parse()?);

        let (ws, _) = connect_async(req).await.context("connect web ws")?;
        info!("[web] connected, subscribing {} symbols", symbols.len());
        let (mut write, mut read) = ws.split();

        // Subscribe to BOTH orderbooks-v2 (top-of-book bid/ask) and tickers (midPrice @5Hz).
        // The ticker feed provides a freshness heartbeat for illiquid symbols where
        // orderbooks-v2 is sparse, and enables the orderbook-stale divergence filter.
        let sub = json!({
            "s": {
                "market/orderbooks-v2": { "rows": symbols },
                "market/tickers":       { "rows": symbols },
            }
        });
        write
            .send(Message::Text(sub.to_string().into()))
            .await
            .context("send web subscribe")?;

        loop {
            tokio::select! {
                _ = self.shutdown.notified() => return Ok(()),
                msg = read.next() => {
                    let msg = match msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => return Err(anyhow!("web ws recv: {}", e)),
                        None => return Err(anyhow!("web ws closed")),
                    };
                    match msg {
                        Message::Text(text) => self.handle_web_text(&text),
                        Message::Ping(p) => {
                            let _ = write.send(Message::Pong(p)).await;
                        }
                        Message::Close(_) => return Err(anyhow!("web ws closed by server")),
                        _ => {}
                    }
                }
            }
        }
    }

    fn handle_web_text(&self, text: &str) {
        let d: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };
        let topic = match d.get("t") {
            Some(t) => t,
            None => return,
        };
        let recv_ms = Utc::now().timestamp_millis();
        let server_ms = d.get("p").and_then(|v| v.as_i64()).map(|ns| ns / 1_000_000).unwrap_or(recv_ms);

        // --- orderbooks-v2: top-of-book bid/ask ---
        if let Some(ob_topic) = topic.get("market/orderbooks-v2") {
            for section in ["s", "i", "u"] {
                let ob = match ob_topic.get(section).and_then(|v| v.as_object()) {
                    Some(o) => o,
                    None => continue,
                };
                for (sym, book) in ob {
                    let bids = match book.get("bids").and_then(|v| v.as_array()) {
                        Some(a) => a,
                        None => continue,
                    };
                    let asks = match book.get("asks").and_then(|v| v.as_array()) {
                        Some(a) => a,
                        None => continue,
                    };
                    let bid = top_of_book(bids);
                    let ask = top_of_book(asks);
                    if bid <= 0.0 || ask <= 0.0 {
                        continue;
                    }
                    let tk = BookTickerC {
                        bid_price: bid,
                        ask_price: ask,
                        bid_size: 0.0,
                        ask_size: 0.0,
                        server_time: server_ms,
                    };
                    self.cache.update_web_bookticker(sym, tk, recv_ms);
                }
            }
        }

        // --- tickers: midPrice @5Hz (freshness heartbeat + divergence filter) ---
        if let Some(tk_topic) = topic.get("market/tickers") {
            for section in ["s", "i", "u"] {
                let tk = match tk_topic.get(section).and_then(|v| v.as_object()) {
                    Some(o) => o,
                    None => continue,
                };
                for (sym, row) in tk {
                    let mid = parse_num(row.get("midPrice"));
                    if mid > 0.0 {
                        self.cache.update_web_ticker_mid(sym, mid, server_ms);
                    }
                }
            }
        }
    }
}

fn parse_num(v: Option<&Value>) -> f64 {
    let v = match v {
        Some(v) => v,
        None => return 0.0,
    };
    if let Some(n) = v.as_f64() {
        return n;
    }
    if let Some(s) = v.as_str() {
        return s.parse().unwrap_or(0.0);
    }
    0.0
}

fn top_of_book(levels: &[Value]) -> f64 {
    levels
        .first()
        .and_then(|l| l.as_array())
        .and_then(|a| a.first())
        .map(|p| parse_num(Some(p)))
        .unwrap_or(0.0)
}

/// Extract flipster cookies from a running Chrome via CDP on localhost:{port}.
async fn fetch_cookies(port: u16) -> Result<Vec<(String, String)>> {
    let url = CDP_VERSION_URL_FMT.replace("{port}", &port.to_string());
    let ver: Value = reqwest::get(&url)
        .await
        .with_context(|| format!("GET {}", url))?
        .json()
        .await
        .context("parse CDP version")?;
    let ws_url = ver
        .get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("CDP: no webSocketDebuggerUrl"))?;

    let (mut ws, _) = connect_async(ws_url).await.context("connect CDP ws")?;
    let req = json!({ "id": 1, "method": "Storage.getCookies" });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .context("send CDP getCookies")?;
    let reply = loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => break t,
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(anyhow!("CDP recv: {}", e)),
            None => return Err(anyhow!("CDP closed")),
        }
    };
    let _ = ws.close(None).await;
    let v: Value = serde_json::from_str(&reply).context("parse CDP reply")?;
    let cookies = v
        .get("result")
        .and_then(|r| r.get("cookies"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("CDP: no cookies"))?;
    let mut out = Vec::new();
    for c in cookies {
        let domain = c.get("domain").and_then(|v| v.as_str()).unwrap_or("");
        if !domain.contains("flipster") {
            continue;
        }
        let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let value = c.get("value").and_then(|v| v.as_str()).unwrap_or("");
        if !name.is_empty() {
            out.push((name.to_string(), value.to_string()));
        }
    }
    Ok(out)
}

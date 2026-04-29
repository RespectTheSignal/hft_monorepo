// Flipster WS data source — implements DataClient trait.
//
// Connects to:
//   api feed: wss://trading-api.flipster.io/api/v1/stream   (HMAC auth, ticker.{sym})
//   web feed: wss://api.flipster.io/api/v2/stream/r230522   (cookie auth, market/orderbooks-v2 + tickers)
//
// Pushes bookticker/trade into DataCache using exchange="gate" for the primary trading
// feed (strategy code still references `gate_bt`/`gate_web_bt` internally — we keep the
// field names and just swap the data source).

use crate::data_cache::DataCache;
use crate::data_client::DataClient;
use crate::types::BookTickerC;
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use log::{info, warn};
use parking_lot::RwLock;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};

const API_WS_URL: &str = "wss://trading-api.flipster.io/api/v1/stream";
const WEB_WS_URL: &str = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription";
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/141.0.0.0 Safari/537.36";
const EXCHANGE_PRIMARY: &str = "gate"; // Keep gate_hft field naming; actual venue is Flipster.

type HmacSha256 = Hmac<Sha256>;

pub struct FlipsterWsClient {
    cache: Arc<DataCache>,
    api_key: String,
    api_secret: String,
    cdp_port: u16,
    running: Arc<AtomicBool>,
    symbols: Arc<RwLock<Vec<String>>>,
    shutdown: Arc<Notify>,
    rt: tokio::runtime::Handle,
}

impl FlipsterWsClient {
    pub fn new(
        cache: Arc<DataCache>,
        api_key: String,
        api_secret: String,
        cdp_port: u16,
        symbols: Vec<String>,
        rt: tokio::runtime::Handle,
    ) -> Arc<Self> {
        Arc::new(Self {
            cache,
            api_key,
            api_secret,
            cdp_port,
            running: Arc::new(AtomicBool::new(false)),
            symbols: Arc::new(RwLock::new(symbols)),
            shutdown: Arc::new(Notify::new()),
            rt,
        })
    }

    fn sign_api(&self, expires: i64) -> Result<String> {
        let msg = format!("GET/api/v1/stream{}", expires);
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes())
            .map_err(|e| anyhow!("hmac init: {}", e))?;
        mac.update(msg.as_bytes());
        Ok(hex::encode(mac.finalize().into_bytes()))
    }
}

impl DataClient for FlipsterWsClient {
    fn start(&self) -> Result<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(()); // already running
        }

        // Binance ZMQ subscriber (base exchange reference data for decide_order_v8).
        // Reuses ZmqSubClient-compatible topic prefix "binance_bookticker_".
        if let Ok(url) = std::env::var("BINANCE_DATA_URL") {
            if !url.trim().is_empty() {
                spawn_binance_zmq(self.cache.clone(), url, self.running.clone());
            }
        } else if let Ok(url) = std::env::var("BINANCE_ZMQ_ADDR") {
            if !url.trim().is_empty() {
                spawn_binance_zmq(self.cache.clone(), url, self.running.clone());
            }
        }

        // Spawn api + web feeds as independent reconnecting tasks.
        let api_cache = self.cache.clone();
        let api_key = self.api_key.clone();
        let api_secret = self.api_secret.clone();
        let api_symbols = self.symbols.clone();
        let api_running = self.running.clone();
        let api_shutdown = self.shutdown.clone();
        self.rt.spawn(async move {
            let signer = |expires: i64| -> Result<String> {
                let msg = format!("GET/api/v1/stream{}", expires);
                let mut mac = HmacSha256::new_from_slice(api_secret.as_bytes())
                    .map_err(|e| anyhow!("hmac init: {}", e))?;
                mac.update(msg.as_bytes());
                Ok(hex::encode(mac.finalize().into_bytes()))
            };
            while api_running.load(Ordering::SeqCst) {
                let syms = api_symbols.read().clone();
                if let Err(e) = api_feed_once(&api_cache, &api_key, &signer, &syms, &api_shutdown).await {
                    warn!("[flipster-api] err: {} — reconnect 3s", e);
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                    _ = api_shutdown.notified() => break,
                }
            }
            info!("[flipster-api] exit");
        });

        let web_cache = self.cache.clone();
        let web_cdp_port = self.cdp_port;
        let web_symbols = self.symbols.clone();
        let web_running = self.running.clone();
        let web_shutdown = self.shutdown.clone();
        self.rt.spawn(async move {
            while web_running.load(Ordering::SeqCst) {
                let syms = web_symbols.read().clone();
                if let Err(e) = web_feed_once(&web_cache, web_cdp_port, &syms, &web_shutdown).await {
                    warn!("[flipster-web] err: {} — reconnect 3s", e);
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                    _ = web_shutdown.notified() => break,
                }
            }
            info!("[flipster-web] exit");
        });

        Ok(())
    }

    fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.shutdown.notify_waiters();
    }

    fn update_symbols(&self, symbols: Vec<String>) {
        *self.symbols.write() = symbols;
        // Reconnect to re-subscribe with new symbol set.
        self.shutdown.notify_waiters();
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

async fn api_feed_once<F>(
    cache: &Arc<DataCache>,
    api_key: &str,
    signer: &F,
    symbols: &[String],
    shutdown: &Arc<Notify>,
) -> Result<()>
where
    F: Fn(i64) -> Result<String>,
{
    let expires = Utc::now().timestamp() + 3600;
    let sig = signer(expires)?;
    let mut req = API_WS_URL.into_client_request()?;
    let h = req.headers_mut();
    h.insert("api-key", api_key.parse()?);
    h.insert("api-expires", expires.to_string().parse()?);
    h.insert("api-signature", sig.parse()?);
    h.insert("User-Agent", USER_AGENT.parse()?);

    let (ws, _) = connect_async(req).await.context("connect api ws")?;
    info!("[flipster-api] connected, subscribing {} syms", symbols.len());
    let (mut write, mut read) = ws.split();

    for chunk in symbols.chunks(50) {
        let topics: Vec<String> = chunk.iter().map(|s| format!("ticker.{}", s)).collect();
        let msg = json!({ "op": "subscribe", "args": topics });
        write
            .send(Message::Text(msg.to_string().into()))
            .await
            .context("send subscribe")?;
    }

    loop {
        tokio::select! {
            _ = shutdown.notified() => return Ok(()),
            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(anyhow!("recv: {}", e)),
                    None => return Err(anyhow!("closed")),
                };
                match msg {
                    Message::Text(text) => handle_api_text(cache, &text),
                    Message::Ping(p) => { let _ = write.send(Message::Pong(p)).await; }
                    Message::Close(_) => return Err(anyhow!("closed by server")),
                    _ => {}
                }
            }
        }
    }
}

fn handle_api_text(cache: &Arc<DataCache>, text: &str) {
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
    // Flipster api feed `ts` is in nanoseconds (same as Python bot: int(d["ts"])/1e6).
    let server_ms = d
        .get("ts")
        .and_then(|v| v.as_i64())
        .map(|ns| ns / 1_000_000)
        .unwrap_or(recv_ms);
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
            let bt = make_bookticker(EXCHANGE_PRIMARY, sym, bid, ask, 0.0, 0.0, server_ms);
            cache.update_bookticker(EXCHANGE_PRIMARY, sym, bt, recv_ms);
        }
    }
}

async fn web_feed_once(
    cache: &Arc<DataCache>,
    cdp_port: u16,
    symbols: &[String],
    shutdown: &Arc<Notify>,
) -> Result<()> {
    let cookies = crate::flipster_cookies::fetch_flipster_cookies(cdp_port).await?;
    info!("[flipster-web] fetched {} cookies", cookies.len());
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
    info!("[flipster-web] connected, subscribing {} syms", symbols.len());
    let (mut write, mut read) = ws.split();

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
            _ = shutdown.notified() => return Ok(()),
            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(anyhow!("recv: {}", e)),
                    None => return Err(anyhow!("closed")),
                };
                match msg {
                    Message::Text(text) => handle_web_text(cache, &text),
                    Message::Ping(p) => { let _ = write.send(Message::Pong(p)).await; }
                    Message::Close(_) => return Err(anyhow!("closed by server")),
                    _ => {}
                }
            }
        }
    }
}

fn handle_web_text(cache: &Arc<DataCache>, text: &str) {
    let d: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };
    let topic = match d.get("t") {
        Some(t) => t,
        None => return,
    };
    let recv_ms = Utc::now().timestamp_millis();
    let server_ms = d
        .get("p")
        .and_then(|v| v.as_i64())
        .map(|ns| ns / 1_000_000)
        .unwrap_or(recv_ms);

    // orderbooks-v2 → web bookticker
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
                let bid_size = top_of_book_size(bids);
                let ask_size = top_of_book_size(asks);
                if bid <= 0.0 || ask <= 0.0 {
                    continue;
                }
                let bt = make_bookticker(
                    EXCHANGE_PRIMARY, sym, bid, ask, bid_size, ask_size, server_ms,
                );
                cache.update_webbookticker(sym, bt, recv_ms);
            }
        }
    }
    // tickers → could store midPrice if DataCache supported it; gate_hft cache doesn't have
    // a direct web_ticker_mid field. We could augment the cache later if needed.
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

/// Size at top of book — Flipster format `[["price","size"],...]`.
fn top_of_book_size(levels: &[Value]) -> f64 {
    levels
        .first()
        .and_then(|l| l.as_array())
        .and_then(|a| a.get(1))
        .map(|s| parse_num(Some(s)))
        .unwrap_or(0.0)
}

/// Pack a BookTickerC with null-padded exchange/symbol byte arrays.
fn make_bookticker(
    exchange: &str,
    symbol: &str,
    bid_price: f64,
    ask_price: f64,
    bid_size: f64,
    ask_size: f64,
    server_time: i64,
) -> BookTickerC {
    let mut ex = [0u8; 16];
    let eb = exchange.as_bytes();
    ex[..eb.len().min(16)].copy_from_slice(&eb[..eb.len().min(16)]);
    let mut sy = [0u8; 32];
    let sb = symbol.as_bytes();
    sy[..sb.len().min(32)].copy_from_slice(&sb[..sb.len().min(32)]);
    BookTickerC {
        exchange: ex,
        symbol: sy,
        bid_price,
        ask_price,
        bid_size,
        ask_size,
        event_time: server_time,
        server_time,
        publisher_sent_ms: 0,
        subscriber_received_ms: server_time,
        subscriber_dump_ms: 0,
    }
}

/// Subscribe to a ZMQ PUB (Binance bookticker) and feed cache as the base exchange.
/// Mirrors zmq_sub_client.rs base-topic handling, but only the binance prefix.
fn spawn_binance_zmq(
    cache: Arc<crate::data_cache::DataCache>,
    url: String,
    running: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let ctx = match zmq::Context::new() {
            c => c,
        };
        let sub = match ctx.socket(zmq::SUB) {
            Ok(s) => s,
            Err(e) => {
                warn!("[binance-zmq] socket err: {}", e);
                return;
            }
        };
        if let Err(e) = sub.set_rcvhwm(100_000) {
            warn!("[binance-zmq] rcvhwm err: {}", e);
        }
        if let Err(e) = sub.connect(&url) {
            warn!("[binance-zmq] connect {} err: {}", url, e);
            return;
        }
        if let Err(e) = sub.set_subscribe(b"binance_bookticker_") {
            warn!("[binance-zmq] sub err: {}", e);
            return;
        }
        info!("[binance-zmq] subscribed {} (binance_bookticker_)", url);

        use std::time::{SystemTime, UNIX_EPOCH};
        while running.load(Ordering::SeqCst) {
            match sub.recv_multipart(zmq::DONTWAIT) {
                Ok(frames) => {
                    if frames.len() < 2 {
                        continue;
                    }
                    let payload = &frames[1];
                    let received_at_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64;
                    if let Some(bt) = crate::types::BookTickerC::from_bytes(payload) {
                        let gate_sym = bt.symbol_str();
                        // Binance ZMQ publishes "BTC_USDT" gate format; rewrite to
                        // Flipster "BTCUSDT.PERP" so strategy base_bt lookups match
                        // the Flipster-format cache keys used for gate_bt/gate_web_bt.
                        let flipster_sym = gate_to_flipster_symbol(&gate_sym);
                        cache.update_bookticker("binance", &flipster_sym, bt, received_at_ms);
                    }
                }
                Err(zmq::Error::EAGAIN) => {
                    let mut items = [sub.as_poll_item(zmq::POLLIN)];
                    let _ = zmq::poll(&mut items, 1);
                }
                Err(e) => {
                    if running.load(Ordering::SeqCst) {
                        warn!("[binance-zmq] recv err: {}", e);
                    }
                    break;
                }
            }
        }
        info!("[binance-zmq] exit");
    });
}

/// "BTC_USDT" → "BTCUSDT.PERP" — normalizes gate-style binance symbol to Flipster perp.
fn gate_to_flipster_symbol(gate_sym: &str) -> String {
    let base = gate_sym.trim_end_matches("_USDT");
    format!("{}USDT.PERP", base)
}

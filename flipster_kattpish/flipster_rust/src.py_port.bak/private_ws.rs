// Flipster private WS — `private/positions`, `private/orders`, `private/margins`.
// Mirrors Python positions_ws.py::WSPositionTracker.

use crate::position::{FlipsterOrder, FlipsterPosition};
use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};
use tracing::{debug, info, warn};

const WS_URL: &str = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription";
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/141.0.0.0 Safari/537.36";

#[derive(Clone, Debug, Default)]
pub struct MarginState {
    pub available: f64,
    pub balance: f64,
    pub nonbonus_collateral: f64,
}

#[derive(Clone)]
pub struct PrivateWsState {
    /// "SYMBOL/SLOT" -> Position
    pub positions: Arc<RwLock<HashMap<String, FlipsterPosition>>>,
    /// orderId -> Order
    pub orders: Arc<RwLock<HashMap<String, FlipsterOrder>>>,
    pub margin: Arc<ArcSwap<MarginState>>,
    pub connected: Arc<AtomicBool>,
}

impl Default for PrivateWsState {
    fn default() -> Self {
        Self::new()
    }
}

impl PrivateWsState {
    pub fn new() -> Self {
        Self {
            positions: Arc::new(RwLock::new(HashMap::new())),
            orders: Arc::new(RwLock::new(HashMap::new())),
            margin: Arc::new(ArcSwap::from_pointee(MarginState::default())),
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Convenience: (side, qty, entry) for "{sym}/0".
    pub fn position_side_qty_entry(&self, sym: &str) -> Option<(&'static str, f64, f64)> {
        let key = format!("{}/0", sym);
        let guard = self.positions.read();
        guard.get(&key).and_then(|p| p.side_qty_entry())
    }

    pub fn has_position(&self, sym: &str) -> bool {
        let key = format!("{}/0", sym);
        self.positions
            .read()
            .get(&key)
            .map(|p| p.is_open())
            .unwrap_or(false)
    }
}

pub struct PrivateWsClient {
    state: PrivateWsState,
    cookies: Arc<ArcSwap<Vec<(String, String)>>>,
    shutdown: Arc<Notify>,
}

impl PrivateWsClient {
    pub fn new(state: PrivateWsState, cookies: Arc<ArcSwap<Vec<(String, String)>>>) -> Self {
        Self {
            state,
            cookies,
            shutdown: Arc::new(Notify::new()),
        }
    }

    pub fn stop(&self) {
        self.shutdown.notify_waiters();
    }

    pub fn spawn(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.run_once().await {
                    warn!("[priv-ws] err: {} — reconnect 3s", e);
                    self.state.connected.store(false, Ordering::Relaxed);
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                    _ = self.shutdown.notified() => break,
                }
            }
        });
    }

    async fn run_once(&self) -> Result<()> {
        let cookies = self.cookies.load_full();
        if cookies.is_empty() {
            return Err(anyhow!("no cookies available"));
        }
        let cookie_hdr = cookies
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("; ");

        let mut req = WS_URL.into_client_request()?;
        let h = req.headers_mut();
        h.insert("Origin", "https://flipster.io".parse()?);
        h.insert("Cookie", cookie_hdr.parse()?);
        h.insert("User-Agent", USER_AGENT.parse()?);

        let (ws, _) = connect_async(req).await.context("connect private ws")?;
        info!("[priv-ws] connected, subscribing");
        let (mut write, mut read) = ws.split();

        let sub = json!({
            "s": {
                "private/positions": { "rows": ["*"] },
                "private/orders":    { "rows": ["*"] },
                "private/margins":   { "rows": ["*"] },
            }
        });
        write
            .send(Message::Text(sub.to_string().into()))
            .await
            .context("send subscribe")?;
        self.state.connected.store(true, Ordering::Relaxed);

        loop {
            tokio::select! {
                _ = self.shutdown.notified() => return Ok(()),
                msg = read.next() => {
                    let msg = match msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => return Err(anyhow!("recv: {}", e)),
                        None => return Err(anyhow!("closed")),
                    };
                    match msg {
                        Message::Text(text) => self.handle_text(&text),
                        Message::Ping(p) => { let _ = write.send(Message::Pong(p)).await; }
                        Message::Close(_) => return Err(anyhow!("closed by server")),
                        _ => {}
                    }
                }
            }
        }
    }

    fn handle_text(&self, text: &str) {
        // "ping" → respond pong (handled via actual Ping frames). Flipster also sends textual "ping"
        // but Python bot treats it; we just ignore non-JSON text.
        let d: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return,
        };
        let topic = match d.get("t").and_then(|v| v.as_object()) {
            Some(t) => t,
            None => return,
        };

        if let Some(pos_topic) = topic.get("private/positions") {
            self.apply_positions(pos_topic);
        }
        if let Some(ord_topic) = topic.get("private/orders") {
            self.apply_orders(ord_topic);
        }
        if let Some(mar_topic) = topic.get("private/margins") {
            self.apply_margins(mar_topic);
        }
    }

    fn apply_positions(&self, topic: &Value) {
        let mut positions = self.state.positions.write();
        // snapshot "s" + insert "i" + update "u" — all upsert.
        for key in ["s", "i", "u"] {
            if let Some(map) = topic.get(key).and_then(|v| v.as_object()) {
                for (k, fields) in map {
                    let fields_obj = match fields.as_object() {
                        Some(o) => o,
                        None => continue,
                    };
                    let entry = positions.entry(k.clone()).or_default();
                    entry.update(fields_obj);
                    // Mid-session: if margin dropped to 0, position closed → remove.
                    if key == "u" && entry.init_margin_reserved() == 0.0 {
                        positions.remove(k);
                    }
                }
            }
        }
        // delete "d" — array of ids OR object keyed by id (with metadata).
        if let Some(d) = topic.get("d") {
            if let Some(arr) = d.as_array() {
                for k in arr {
                    if let Some(k) = k.as_str() {
                        positions.remove(k);
                    }
                }
            } else if let Some(obj) = d.as_object() {
                for k in obj.keys() {
                    positions.remove(k);
                }
            }
        }
    }

    fn apply_orders(&self, topic: &Value) {
        debug!("[priv-ws] orders topic: {}", topic);
        let mut orders = self.state.orders.write();
        // snapshot "s" + insert "i" + update "u" — all upsert.
        for key in ["s", "i", "u"] {
            if let Some(map) = topic.get(key).and_then(|v| v.as_object()) {
                for (oid, fields) in map {
                    let fields_obj = match fields.as_object() {
                        Some(o) => o,
                        None => continue,
                    };
                    let entry = orders.entry(oid.clone()).or_default();
                    entry.update(fields_obj);
                }
            }
        }
        if let Some(d) = topic.get("d") {
            if let Some(arr) = d.as_array() {
                for k in arr {
                    if let Some(k) = k.as_str() {
                        orders.remove(k);
                    }
                }
            } else if let Some(obj) = d.as_object() {
                for k in obj.keys() {
                    orders.remove(k);
                }
            }
        }
    }

    fn apply_margins(&self, topic: &Value) {
        // Python: only updates from "u" key, USDT currency.
        let u = match topic.get("u").and_then(|v| v.as_object()) {
            Some(o) => o,
            None => return,
        };
        let usdt = match u.get("USDT").and_then(|v| v.as_object()) {
            Some(o) => o,
            None => return,
        };
        let cur = self.state.margin.load();
        let mut next = (**cur).clone();
        if let Some(v) = usdt.get("crossMarginAvailable") {
            if let Some(n) = parse_str_num(v) {
                next.available = n;
            }
        }
        if let Some(v) = usdt.get("marginBalance") {
            if let Some(n) = parse_str_num(v) {
                next.balance = n;
            }
        }
        if let Some(v) = usdt.get("nonBonusCollateral") {
            if let Some(n) = parse_str_num(v) {
                next.nonbonus_collateral = n;
            }
        }
        self.state.margin.store(Arc::new(next));
    }
}

fn parse_str_num(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        return s.parse().ok();
    }
    None
}

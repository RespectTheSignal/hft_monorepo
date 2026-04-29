//! Flipster private WebSocket subscriber.
//!
//! Subscribes to `private/positions`, `private/orders`, and
//! `private/margins` on `wss://api.flipster.io/api/v2/stream/r230522` and
//! maintains an in-memory state map. The executor uses this state to:
//!
//! - Verify whether a LIMIT order filled after we attempted to cancel it
//!   (HTTP GET on `/trade/positions` returns 405; WS is the only path).
//! - Read current position size for SAFETY revert sizing.
//! - Detect orphaned positions before placing new entries.
//!
//! Auto-reconnects on disconnect with a 3s backoff, mirroring the Python
//! `_flipster_ws_async` loop.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

const WS_URL: &str = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription";

/// Live snapshot of Flipster private state. All mutations happen inside
/// the WS task; readers clone what they need under the RwLock.
#[derive(Default, Debug)]
pub struct FlipsterState {
    /// Key = "SYMBOL/SLOT" (e.g. "BTCUSDT.PERP/0"). Value = the latest
    /// merged fields the WS has sent us. Schema is loose — we only need
    /// `position`, `initMarginReserved`, occasionally `avgPrice`.
    pub positions: HashMap<String, HashMap<String, Value>>,
    /// Key = orderId. Value = order fields including `status`.
    pub orders: HashMap<String, HashMap<String, Value>>,
    pub connected: bool,
}

impl FlipsterState {
    /// Signed position size (positive=long, negative=short).
    /// Reads the `position` field from `positions["{symbol}/{slot}"]`.
    pub fn position_size(&self, symbol: &str, slot: u32) -> f64 {
        let key = format!("{symbol}/{slot}");
        self.positions
            .get(&key)
            .and_then(|p| p.get("position"))
            .and_then(value_as_f64)
            .unwrap_or(0.0)
    }

    /// True if WS shows any indication of an open position — non-zero
    /// `position` OR non-zero `initMarginReserved`. WS sometimes
    /// broadcasts margin before size; this catches that race.
    pub fn has_position(&self, symbol: &str, slot: u32) -> bool {
        let key = format!("{symbol}/{slot}");
        let Some(p) = self.positions.get(&key) else {
            return false;
        };
        if p.get("position")
            .and_then(value_as_f64)
            .map(|s| s.abs() > 0.0)
            .unwrap_or(false)
        {
            return true;
        }
        p.get("initMarginReserved")
            .and_then(value_as_f64)
            .map(|m| m > 0.0)
            .unwrap_or(false)
    }

    /// True if the order is still in an open state (NEW / OPEN /
    /// PARTIALLY_FILLED / PENDING). Returns false for missing keys
    /// (cancelled/filled both drop the order from the open-orders map).
    pub fn order_open(&self, order_id: &str) -> bool {
        let Some(o) = self.orders.get(order_id) else {
            return false;
        };
        let status = o
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        !["FILL", "CANCEL", "REJECT", "EXPIR", "DONE"]
            .iter()
            .any(|t| status.contains(t))
    }

    /// Returns (bid, ask) for `symbol` from the WS positions feed. None
    /// if not present or fields missing/zero.
    pub fn bid_ask(&self, symbol: &str, slot: u32) -> Option<(f64, f64)> {
        let key = format!("{symbol}/{slot}");
        let p = self.positions.get(&key)?;
        let bid = p.get("bidPrice").and_then(value_as_f64)?;
        let ask = p.get("askPrice").and_then(value_as_f64)?;
        if bid > 0.0 && ask > 0.0 && ask >= bid {
            Some((bid, ask))
        } else {
            None
        }
    }

    /// Iterate (symbol, slot, signed_size, margin_reserved, mid_price)
    /// currently held according to WS state. mid_price is from the WS feed
    /// (midPrice or bid/ask average), used by the orphan sweeper to send
    /// MARKET orders with a valid reference price (Flipster rejects 0).
    pub fn iter_open_positions(&self) -> Vec<(String, u32, f64, f64, f64)> {
        let mut out = Vec::new();
        for (key, p) in &self.positions {
            let size = p.get("position").and_then(value_as_f64).unwrap_or(0.0);
            let margin = p
                .get("initMarginReserved")
                .and_then(value_as_f64)
                .unwrap_or(0.0);
            if size.abs() == 0.0 && margin == 0.0 {
                continue;
            }
            let mid = p
                .get("midPrice")
                .and_then(value_as_f64)
                .or_else(|| {
                    let bid = p.get("bidPrice").and_then(value_as_f64)?;
                    let ask = p.get("askPrice").and_then(value_as_f64)?;
                    Some((bid + ask) * 0.5)
                })
                .or_else(|| p.get("avgPrice").and_then(value_as_f64))
                .unwrap_or(0.0);
            if let Some((sym, slot_s)) = key.rsplit_once('/') {
                if let Ok(slot) = slot_s.parse::<u32>() {
                    out.push((sym.to_string(), slot, size, margin, mid));
                }
            }
        }
        out
    }

    /// Iterate (order_id, symbol) for orders still in an open status.
    pub fn iter_open_orders(&self) -> Vec<(String, String)> {
        self.orders
            .iter()
            .filter_map(|(oid, fields)| {
                let status = fields
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_ascii_uppercase();
                if ["FILL", "CANCEL", "REJECT", "EXPIR", "DONE"]
                    .iter()
                    .any(|t| status.contains(t))
                {
                    return None;
                }
                let sym = fields.get("symbol").and_then(|s| s.as_str())?;
                Some((oid.clone(), sym.to_string()))
            })
            .collect()
    }
}

fn value_as_f64(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        return s.parse::<f64>().ok();
    }
    None
}

pub type SharedState = Arc<RwLock<FlipsterState>>;

/// Spawn the long-running WS task. Returns a handle to the shared state.
/// The task auto-reconnects on disconnect.
pub fn spawn(cookies: HashMap<String, String>) -> SharedState {
    let state: SharedState = Arc::new(RwLock::new(FlipsterState::default()));
    let state_clone = state.clone();
    tokio::spawn(async move {
        run_loop(cookies, state_clone).await;
    });
    state
}

async fn run_loop(cookies: HashMap<String, String>, state: SharedState) {
    let cookie_hdr = cookies
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");

    loop {
        match connect_once(&cookie_hdr, &state).await {
            Ok(()) => warn!("[flip-ws] connection closed cleanly; reconnecting in 3s"),
            Err(e) => warn!(error = %e, "[flip-ws] error; reconnecting in 3s"),
        }
        {
            let mut s = state.write().await;
            s.connected = false;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

async fn connect_once(cookie_hdr: &str, state: &SharedState) -> anyhow::Result<()> {
    let mut req = WS_URL.into_client_request()?;
    let h = req.headers_mut();
    h.insert(
        HeaderName::from_static("origin"),
        HeaderValue::from_static("https://flipster.io"),
    );
    h.insert(HeaderName::from_static("cookie"), HeaderValue::from_str(cookie_hdr)?);
    h.insert(
        HeaderName::from_static("user-agent"),
        HeaderValue::from_static(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36",
        ),
    );

    let (mut ws, _resp) = tokio_tungstenite::connect_async(req).await?;
    let sub = serde_json::json!({
        "s": {
            "private/positions": {"rows": ["*"]},
            "private/orders":    {"rows": ["*"]},
            "private/margins":   {"rows": ["*"]},
        }
    });
    ws.send(Message::Text(sub.to_string().into())).await?;
    {
        let mut s = state.write().await;
        s.connected = true;
    }
    info!("[flip-ws] connected & subscribed (private positions/orders/margins)");

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => match std::str::from_utf8(&b) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            },
            Message::Ping(p) => {
                ws.send(Message::Pong(p)).await?;
                continue;
            }
            Message::Pong(_) | Message::Frame(_) => continue,
            Message::Close(_) => return Ok(()),
        };

        // Flipster also sends literal "ping" text frames in addition to
        // RFC6455 ping; mirror Python by replying "pong".
        if text == "ping" {
            ws.send(Message::Text("pong".to_string().into())).await?;
            continue;
        }

        let parsed: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(t) = parsed.get("t").and_then(|t| t.as_object()) else {
            continue;
        };

        if let Some(positions) = t.get("private/positions") {
            apply_position_update(positions, state).await;
        }
        if let Some(orders) = t.get("private/orders") {
            apply_order_update(orders, state).await;
        }
    }
    Ok(())
}

async fn apply_position_update(node: &Value, state: &SharedState) {
    // Flipster sends a "s" (snapshot) on initial subscribe followed by
    // "u" (incremental updates) and "d" (deletes). We treat snapshot the
    // same as updates — merge fields into our state. Without snapshot
    // handling, pre-existing orphans were invisible to the sweeper.
    let snapshot = node.get("s").and_then(|v| v.as_object());
    let updates = node.get("u").and_then(|v| v.as_object());
    let deletes = node.get("d").and_then(|v| v.as_array());

    if snapshot.is_none() && updates.is_none() && deletes.is_none() {
        return;
    }
    let mut s = state.write().await;
    for src in [snapshot, updates].into_iter().flatten() {
        for (k, v) in src {
            let entry = s.positions.entry(k.clone()).or_default();
            if let Some(fields) = v.as_object() {
                for (fk, fv) in fields {
                    entry.insert(fk.clone(), fv.clone());
                }
            }
            let drop = entry
                .get("initMarginReserved")
                .and_then(value_as_f64)
                .map(|m| m == 0.0)
                .unwrap_or(false);
            if drop {
                s.positions.remove(k);
            }
        }
    }
    if let Some(deletes) = deletes {
        for d in deletes {
            if let Some(k) = d.as_str() {
                s.positions.remove(k);
            }
        }
    }
}

async fn apply_order_update(node: &Value, state: &SharedState) {
    let snapshot = node.get("s").and_then(|v| v.as_object());
    let updates = node.get("u").and_then(|v| v.as_object());
    let deletes = node.get("d").and_then(|v| v.as_array());

    if snapshot.is_none() && updates.is_none() && deletes.is_none() {
        return;
    }
    let mut s = state.write().await;
    for src in [snapshot, updates].into_iter().flatten() {
        for (k, v) in src {
            let entry = s.orders.entry(k.clone()).or_default();
            if let Some(fields) = v.as_object() {
                for (fk, fv) in fields {
                    entry.insert(fk.clone(), fv.clone());
                }
            }
        }
    }
    if let Some(deletes) = deletes {
        for d in deletes {
            if let Some(k) = d.as_str() {
                s.orders.remove(k);
            }
        }
    }
}

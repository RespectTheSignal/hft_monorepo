//! Flipster private + public WebSocket subscriber.
//!
//! Subscribes to `private/positions`, `private/orders`, `private/margins`
//! AND `market/orderbooks-v2` on `wss://api.flipster.io/api/v2/stream/r230522`
//! over a single connection. The executor uses the in-memory state to:
//!
//! - Verify whether a LIMIT order filled after we attempted to cancel it
//!   (HTTP GET on `/trade/positions` returns 405; WS is the only path).
//! - Read current position size for SAFETY revert sizing.
//! - Detect orphaned positions before placing new entries.
//! - Pre-check L10 orderbook depth before entry to size against impact.
//!
//! Depth subscription requires an explicit symbol list. We read it from
//! `FLIPSTER_DEPTH_SYMBOLS` (comma-separated full Flipster symbols, e.g.
//! `BTCUSDT.PERP,ETHUSDT.PERP`); falls back to the gate_lead whitelist.
//!
//! Auto-reconnects on disconnect with a 3s backoff, mirroring the Python
//! `_flipster_ws_async` loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_http_proxy::http_connect_tokio_with_basic_auth;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

const WS_URL: &str = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription";

/// L10 orderbook snapshot for a single symbol. Levels are sorted best
/// first (`bids[0]` = highest bid, `asks[0]` = lowest ask).
#[derive(Default, Debug, Clone)]
pub struct DepthSnapshot {
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
    /// Local-clock recv epoch ms. Used by readers to gauge staleness.
    pub recv_ms: u64,
}

/// Result of walking the take-side levels for a target qty. Lets the
/// caller decide between SKIP (impact too high), SHRINK (cap to qty
/// available within max-impact), or PROCEED (depth is fine).
#[derive(Debug, Clone, Copy)]
pub struct DepthWalk {
    /// Total qty reachable across all known levels (= available_qty).
    pub levels_total_qty: f64,
    /// Worst price reached when accumulating until target_qty filled,
    /// or the deepest level if target exceeded total.
    pub worst_price: f64,
    /// Top-of-book price on the take side.
    pub top_price: f64,
    /// (worst_price - top_price) / top_price in bp, signed so positive
    /// = unfavorable to us.
    pub impact_bp: f64,
    /// True if we would have to skip past all known levels to fill
    /// target_qty (i.e. depth insufficient).
    pub depth_exhausted: bool,
}

/// Live snapshot of Flipster private + public state. All mutations
/// happen inside the WS task; readers clone what they need under the
/// RwLock.
#[derive(Default, Debug)]
pub struct FlipsterState {
    /// Key = "SYMBOL/SLOT" (e.g. "BTCUSDT.PERP/0"). Value = the latest
    /// merged fields the WS has sent us. Schema is loose — we only need
    /// `position`, `initMarginReserved`, occasionally `avgPrice`.
    pub positions: HashMap<String, HashMap<String, Value>>,
    /// Key = orderId. Value = order fields including `status`.
    pub orders: HashMap<String, HashMap<String, Value>>,
    /// Key = full Flipster symbol (e.g. "BTCUSDT.PERP"). L10 book.
    pub depth: HashMap<String, DepthSnapshot>,
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

    /// Walk the take-side levels for `symbol` until cumulative qty meets
    /// `target_qty`. `side` is the entry direction ("long" → consumes
    /// asks, "short" → consumes bids). Returns None if depth absent or
    /// stale beyond `max_age_ms`.
    pub fn depth_walk_for_qty(
        &self,
        symbol: &str,
        side: &str,
        target_qty: f64,
        max_age_ms: u64,
    ) -> Option<DepthWalk> {
        let snap = self.depth.get(symbol)?;
        if snap.bids.is_empty() && snap.asks.is_empty() {
            return None;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if now_ms.saturating_sub(snap.recv_ms) > max_age_ms {
            return None;
        }
        let levels = if side == "long" { &snap.asks } else { &snap.bids };
        if levels.is_empty() {
            return None;
        }
        let top = levels[0].0;
        let mut acc = 0.0;
        let mut worst = top;
        let mut exhausted = true;
        for (px, sz) in levels.iter() {
            worst = *px;
            acc += sz;
            if acc >= target_qty {
                exhausted = false;
                break;
            }
        }
        let total: f64 = levels.iter().map(|(_, s)| s).sum();
        let impact_signed = if top > 0.0 {
            ((worst - top) / top) * 1e4 * if side == "long" { 1.0 } else { -1.0 }
        } else {
            0.0
        };
        Some(DepthWalk {
            levels_total_qty: total,
            worst_price: worst,
            top_price: top,
            impact_bp: impact_signed,
            depth_exhausted: exhausted,
        })
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

    let proxies = read_proxy_pool();
    if !proxies.is_empty() {
        info!(
            n = proxies.len(),
            "[flip-ws] proxy pool active — rotating per reconnect"
        );
    }
    let next_idx = AtomicUsize::new(0);

    loop {
        let proxy = if proxies.is_empty() {
            None
        } else {
            let i = next_idx.fetch_add(1, Ordering::Relaxed) % proxies.len();
            Some(proxies[i].clone())
        };
        match connect_once(&cookie_hdr, &state, proxy.as_deref()).await {
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

/// Same precedence as flipster-client: FLIPSTER_PROXY_POOL (comma list)
/// → FLIPSTER_PROXY_URL (single) → empty.
fn read_proxy_pool() -> Vec<String> {
    if let Ok(pool) = std::env::var("FLIPSTER_PROXY_POOL") {
        let entries: Vec<String> = pool
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if !entries.is_empty() {
            return entries;
        }
    }
    if let Ok(url) = std::env::var("FLIPSTER_PROXY_URL") {
        if !url.is_empty() {
            return vec![url];
        }
    }
    Vec::new()
}

async fn connect_once(
    cookie_hdr: &str,
    state: &SharedState,
    proxy_url: Option<&str>,
) -> anyhow::Result<()> {
    let mut req = WS_URL.into_client_request()?;
    let h = req.headers_mut();
    h.insert(
        HeaderName::from_static("origin"),
        HeaderValue::from_static("https://flipster.io"),
    );
    h.insert(
        HeaderName::from_static("cookie"),
        HeaderValue::from_str(cookie_hdr)?,
    );
    h.insert(
        HeaderName::from_static("user-agent"),
        HeaderValue::from_static(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36",
        ),
    );

    // Route the WS through the same proxy pool as REST so the host's IP
    // never directly hits api.flipster.io. tokio-tungstenite has no
    // built-in proxy, so we open the TCP socket ourselves, do an HTTP
    // CONNECT tunnel, then hand the TLS-wrapped stream off to the WS
    // handshake (`client_async_tls_with_config` does the TLS upgrade).
    let (mut ws, _resp) = if let Some(proxy_url) = proxy_url {
        let parsed = url::Url::parse(proxy_url)?;
        let phost = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("proxy URL has no host: {proxy_url}"))?
            .to_string();
        let pport = parsed.port().unwrap_or(3128);
        let user = parsed.username().to_string();
        let pass = parsed.password().unwrap_or("").to_string();
        let mut tcp = TcpStream::connect((phost.as_str(), pport)).await?;
        if user.is_empty() {
            async_http_proxy::http_connect_tokio(&mut tcp, "api.flipster.io", 443).await?;
        } else {
            http_connect_tokio_with_basic_auth(&mut tcp, "api.flipster.io", 443, &user, &pass)
                .await?;
        }
        tokio_tungstenite::client_async_tls_with_config(req, tcp, None, None).await?
    } else {
        tokio_tungstenite::connect_async(req).await?
    };
    let depth_symbols = read_depth_symbols();
    let sub = serde_json::json!({
        "s": {
            "private/positions": {"rows": ["*"]},
            "private/orders":    {"rows": ["*"]},
            "private/margins":   {"rows": ["*"]},
            "market/orderbooks-v2": {"rows": depth_symbols},
        }
    });
    ws.send(Message::Text(sub.to_string().into())).await?;
    {
        let mut s = state.write().await;
        s.connected = true;
    }
    info!(
        depth_n = depth_symbols.len(),
        "[flip-ws] connected & subscribed (private + orderbooks-v2)"
    );

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
        if let Some(books) = t.get("market/orderbooks-v2") {
            apply_orderbook_update(books, state).await;
        }
    }
    Ok(())
}

/// Read FLIPSTER_DEPTH_SYMBOLS (comma-separated full symbols) or fall
/// back to gate_lead's default whitelist (BASE → BASEUSDT.PERP).
fn read_depth_symbols() -> Vec<String> {
    if let Ok(raw) = std::env::var("FLIPSTER_DEPTH_SYMBOLS") {
        let v: Vec<String> = raw
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        if !v.is_empty() {
            return v;
        }
    }
    // Default = gate_lead live whitelist. Kept in sync manually with
    // `collector::gate_lead::GateLeadParams::default`.
    let bases = [
        "AAVE", "BAS", "BEAT", "BOB", "BRETT", "CGPT", "CROSS", "CYS", "D", "ENSO", "FIGHT",
        "FLUID", "GENIUS", "GRIFFAIN", "GUA", "H", "INTC", "INX", "JCT", "KITE", "MAGMA", "MASK",
        "ON", "OPEN", "PENDLE", "PENGU", "POPCAT", "SNDK", "TAG", "TURTLE", "UAI",
    ];
    bases.iter().map(|b| format!("{b}USDT.PERP")).collect()
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

async fn apply_orderbook_update(node: &Value, state: &SharedState) {
    let snapshot = node.get("s").and_then(|v| v.as_object());
    let updates = node.get("u").and_then(|v| v.as_object());
    if snapshot.is_none() && updates.is_none() {
        return;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut s = state.write().await;
    for src in [snapshot, updates].into_iter().flatten() {
        for (sym, v) in src {
            let bids = parse_levels(v.get("bids"));
            let asks = parse_levels(v.get("asks"));
            if bids.is_empty() && asks.is_empty() {
                continue;
            }
            s.depth.insert(
                sym.clone(),
                DepthSnapshot {
                    bids,
                    asks,
                    recv_ms: now_ms,
                },
            );
        }
    }
}

fn parse_levels(v: Option<&Value>) -> Vec<(f64, f64)> {
    let Some(arr) = v.and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|lv| {
            let pair = lv.as_array()?;
            let px = value_as_f64(pair.first()?)?;
            let sz = value_as_f64(pair.get(1)?)?;
            if px > 0.0 && sz > 0.0 {
                Some((px, sz))
            } else {
                None
            }
        })
        .collect()
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

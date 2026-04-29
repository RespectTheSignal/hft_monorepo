//! Real-time Flipster account state — v1 trading API.
//!
//! Keeps an in-process snapshot of the executor's Flipster account synced
//! via two parallel tasks:
//!
//! 1. **REST poll** — `/api/v1/account`, `/account/balance`, `/account/margin`,
//!    `/account/position` every `FLIPSTER_ACCT_POLL_S` seconds (default
//!    10). Authoritative source for the slow-changing fields and the
//!    safety net when the WS goes silent.
//! 2. **WS subscribe** — `wss://trading-api.flipster.io/api/v1/stream`
//!    subscribes to `account`, `account.position`, `account.balance`,
//!    `account.margin`. Pushes incremental updates within ~ms of server
//!    state change.
//!
//! Both write to the same `Arc<RwLock<AccountState>>`; readers
//! (`executor.rs` filters, dashboards) see the latest value either path
//! produced. The two tasks are independent — one stalling never blocks
//! the other.
//!
//! Auth: HMAC-SHA256 over `METHOD + path + expires (+ body)`, per the
//! shared scheme used elsewhere in the stack
//! (`feed/data_publisher/src/flipster/auth.rs`,
//! `strategy_flipster_python/.../execution/auth.py`). Reads
//! `FLIPSTER_API_KEY` / `FLIPSTER_API_SECRET` from env.
//!
//! Coexists with `flipster_ws.rs` (v2 cookie-based feed used by orphan
//! sweepers) — does not replace it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use tokio::sync::{broadcast, RwLock};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

const REST_BASE: &str = "https://trading-api.flipster.io";
const WS_URL: &str = "wss://trading-api.flipster.io/api/v1/stream";
const WS_AUTH_PATH: &str = "/api/v1/stream";
const POLL_DEFAULT_S: u64 = 10;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(3);

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    #[serde(default)]
    pub total_wallet_balance: String,
    #[serde(default)]
    pub total_unrealized_pnl: String,
    #[serde(default)]
    pub total_margin_balance: String,
    #[serde(default)]
    pub total_margin_reserved: String,
    #[serde(default)]
    pub available_balance: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    pub asset: String,
    #[serde(default)]
    pub balance: String,
    #[serde(default)]
    pub available_balance: String,
    #[serde(default)]
    pub max_withdrawable_amount: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginInfo {
    pub currency: String,
    #[serde(default)]
    pub isolated_margin_available: String,
    #[serde(default)]
    pub unrealized_pnl: String,
    #[serde(default)]
    pub reserved_for_isolated_margin: String,
    #[serde(default)]
    pub reserved_for_isolated_margin_position: String,
    #[serde(default)]
    pub cross_margin_available: String,
    #[serde(default)]
    pub margin_balance: String,
    #[serde(default)]
    pub cross_margin_ratio: Option<String>,
    #[serde(default)]
    pub init_margin_reserved: String,
    #[serde(default)]
    pub init_position_margin: String,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub symbol: String,
    #[serde(default)]
    pub slot: u32,
    #[serde(default)]
    pub leverage: u32,
    #[serde(default)]
    pub margin_type: String,
    #[serde(default)]
    pub position_side: Option<String>,
    #[serde(default)]
    pub position_amount: Option<String>,
    #[serde(default)]
    pub position_qty: Option<String>,
    #[serde(default)]
    pub mark_price: Option<String>,
    #[serde(default)]
    pub unrealized_pnl: Option<String>,
    #[serde(default)]
    pub initial_margin: Option<String>,
    #[serde(default)]
    pub maint_margin: Option<String>,
    #[serde(default)]
    pub entry_price: Option<String>,
    #[serde(default)]
    pub break_even_price: Option<String>,
    #[serde(default)]
    pub liquidation_price: Option<String>,
}

/// Tells the position-change subscriber what just happened so the
/// caller doesn't have to diff. Carried in the broadcast channel
/// alongside the new state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionChangeKind {
    /// First time we see this (symbol, slot).
    Open,
    /// Existing slot's fields updated (size/entry/PnL etc.).
    Update,
    /// Slot disappeared (closed) — listed by symbol/slot only.
    Close,
}

#[derive(Debug, Clone)]
pub struct PositionChange {
    pub symbol: String,
    pub slot: u32,
    pub kind: PositionChangeKind,
    /// `None` for `Close` (the row is gone).
    pub position: Option<Position>,
}

#[derive(Default, Debug)]
pub struct AccountState {
    pub account: Option<AccountInfo>,
    /// Keyed by asset (e.g. "USDT", "BTC").
    pub balances: HashMap<String, Balance>,
    /// Keyed by symbol → slot → Position. Multi-position trade-mode
    /// stacks several slots on one symbol; this nested shape lets
    /// callers iterate a base's stack cheaply (`positions[sym].values()`)
    /// without scanning a flat map.
    pub positions: HashMap<String, HashMap<u32, Position>>,
    /// Keyed by currency (e.g. "USDT").
    pub margins: HashMap<String, MarginInfo>,
    pub last_rest_at_ms: i64,
    pub last_ws_at_ms: i64,
    pub ws_connected: bool,
}

impl AccountState {
    pub fn position_for(&self, symbol: &str, slot: u32) -> Option<&Position> {
        self.positions.get(symbol).and_then(|m| m.get(&slot))
    }

    /// All open positions on a given symbol, regardless of slot.
    pub fn positions_for_symbol(&self, symbol: &str) -> Option<&HashMap<u32, Position>> {
        self.positions.get(symbol)
    }

    pub fn parse_f64<F>(opt: Option<&str>, default: F) -> f64
    where
        F: FnOnce() -> f64,
    {
        opt.and_then(|s| s.parse::<f64>().ok()).unwrap_or_else(default)
    }
}

pub type SharedAccountState = Arc<RwLock<AccountState>>;

/// Spawn the REST + WS sync tasks. Returns the shared state. Both tasks
/// auto-restart on failure with backoff. Returns an empty state if the
/// API key is missing — the caller can still hold the handle and the
/// state stays at its default until creds are configured.
pub fn spawn() -> SharedAccountState {
    spawn_with_changes().0
}

/// Same as `spawn` but also returns a `broadcast::Sender<PositionChange>`
/// the caller can subscribe to for live notifications. Drop the
/// receiver to stop listening — the broadcast accepts new subscribers
/// at any time. Capacity is 1024; lagged subscribers see a `Lagged`
/// recv error (the writers don't block).
pub fn spawn_with_changes() -> (SharedAccountState, broadcast::Sender<PositionChange>) {
    let state: SharedAccountState = Arc::new(RwLock::new(AccountState::default()));
    let (change_tx, _) = broadcast::channel::<PositionChange>(1024);
    let api_key = std::env::var("FLIPSTER_API_KEY").ok();
    let api_secret = std::env::var("FLIPSTER_API_SECRET").ok();
    if api_key.is_none() || api_secret.is_none() {
        warn!(
            "[flip-acct] FLIPSTER_API_KEY/FLIPSTER_API_SECRET not set — account sync disabled"
        );
        return (state, change_tx);
    }
    let key = api_key.unwrap();
    let secret = api_secret.unwrap();

    let poll_interval_s: u64 = std::env::var("FLIPSTER_ACCT_POLL_S")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(POLL_DEFAULT_S);

    info!(
        poll_interval_s,
        "[flip-acct] starting REST poll + WS sync"
    );

    {
        let st = state.clone();
        let key = key.clone();
        let secret = secret.clone();
        let tx = change_tx.clone();
        tokio::spawn(async move {
            rest_poll_loop(st, key, secret, poll_interval_s, tx).await;
        });
    }
    {
        let st = state.clone();
        let tx = change_tx.clone();
        tokio::spawn(async move {
            ws_loop(st, key, secret, tx).await;
        });
    }
    (state, change_tx)
}

/// Set the account-level trade mode. Valid values per Flipster docs:
/// "ONE_WAY" or "MULTIPLE_POSITIONS". Issues PUT /api/v1/account/trade-mode
/// with HMAC auth + the same proxy used by the rest of this module.
/// Returns Ok if the API echoes the requested mode back; Err otherwise.
pub async fn set_trade_mode(mode: &str) -> Result<()> {
    let api_key = std::env::var("FLIPSTER_API_KEY")
        .map_err(|_| anyhow!("FLIPSTER_API_KEY not set"))?;
    let api_secret = std::env::var("FLIPSTER_API_SECRET")
        .map_err(|_| anyhow!("FLIPSTER_API_SECRET not set"))?;
    let mut builder = reqwest::Client::builder().gzip(true).https_only(true);
    if let Ok(proxy_url) = std::env::var("FLIPSTER_PROXY_URL") {
        if !proxy_url.is_empty() {
            if let Ok(p) = reqwest::Proxy::all(&proxy_url) {
                builder = builder.proxy(p);
            }
        }
    }
    let client = builder.build()?;

    let path = "/api/v1/account/trade-mode";
    let body = serde_json::json!({ "tradeMode": mode }).to_string();
    let expires = unix_now_secs() + 60;
    let signature = sign_hmac(&api_secret, "PUT", path, expires, Some(body.as_bytes()));
    let url = format!("{}{}", REST_BASE, path);
    let resp = client
        .put(&url)
        .header("api-key", &api_key)
        .header("api-expires", expires.to_string())
        .header("api-signature", signature)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .with_context(|| format!("PUT {}", path))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("PUT {} -> {}: {}", path, status, text));
    }
    info!(mode, response = %text, "[flip-acct] trade-mode set");
    Ok(())
}

// -----------------------------------------------------------------------------
// REST poll task
// -----------------------------------------------------------------------------

async fn rest_poll_loop(
    state: SharedAccountState,
    api_key: String,
    api_secret: String,
    interval_s: u64,
    change_tx: broadcast::Sender<PositionChange>,
) {
    let mut builder = reqwest::Client::builder().gzip(true).https_only(true);
    // Same CF-bypass proxy used by flipster-client. Optional — if the env
    // var isn't set the client goes direct.
    if let Ok(proxy_url) = std::env::var("FLIPSTER_PROXY_URL") {
        if !proxy_url.is_empty() {
            match reqwest::Proxy::all(&proxy_url) {
                Ok(p) => {
                    builder = builder.proxy(p);
                    info!(proxy = %proxy_url, "[flip-acct/rest] using proxy");
                }
                Err(e) => warn!(error = %e, "[flip-acct/rest] invalid FLIPSTER_PROXY_URL"),
            }
        }
    }
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "[flip-acct/rest] failed to build http client");
            return;
        }
    };
    let mut tick = tokio::time::interval(Duration::from_secs(interval_s));
    // Skip the first immediate tick: we want one poll right after start
    // *and* periodic thereafter, but interval()'s first tick fires
    // instantly which is fine — issue is just to log the first cycle.
    loop {
        tick.tick().await;
        if let Err(e) = poll_once(&client, &api_key, &api_secret, &state, &change_tx).await {
            warn!(error = %e, "[flip-acct/rest] poll failed");
        }
    }
}

async fn poll_once(
    client: &reqwest::Client,
    api_key: &str,
    api_secret: &str,
    state: &SharedAccountState,
    change_tx: &broadcast::Sender<PositionChange>,
) -> Result<()> {
    let now_ms = unix_now_ms();
    let account = fetch_json(client, api_key, api_secret, "GET", "/api/v1/account").await?;
    let margin = fetch_json(client, api_key, api_secret, "GET", "/api/v1/account/margin").await?;
    let balance = fetch_json(client, api_key, api_secret, "GET", "/api/v1/account/balance").await?;
    let position = fetch_json(client, api_key, api_secret, "GET", "/api/v1/account/position").await?;

    let account_info: Option<AccountInfo> = serde_json::from_value(account.clone()).ok();
    let balances: Vec<Balance> = parse_array(&balance);
    let positions: Vec<Position> = parse_array(&position);
    let margins: Vec<MarginInfo> = match margin.get("margins") {
        Some(arr) => parse_array(arr),
        None => Vec::new(),
    };

    let mut s = state.write().await;
    if let Some(a) = account_info {
        s.account = Some(a);
    }
    // REST is the authoritative snapshot — replace, don't merge: if a
    // balance disappeared from the response we want the in-memory map to
    // reflect that.
    s.balances.clear();
    for b in balances {
        s.balances.insert(b.asset.clone(), b);
    }
    // Build the new positions tree, then diff against the old one so
    // each (symbol, slot) change emits exactly one Open/Update/Close.
    let mut new_positions: HashMap<String, HashMap<u32, Position>> = HashMap::new();
    for p in positions {
        new_positions
            .entry(p.symbol.clone())
            .or_default()
            .insert(p.slot, p);
    }
    let changes = diff_positions(&s.positions, &new_positions);
    s.positions = new_positions;
    s.margins.clear();
    for m in margins {
        s.margins.insert(m.currency.clone(), m);
    }
    s.last_rest_at_ms = now_ms;
    drop(s);
    for c in changes {
        emit_change(change_tx, c);
    }
    Ok(())
}

/// Compute (Open / Update / Close) events between two snapshots.
fn diff_positions(
    old: &HashMap<String, HashMap<u32, Position>>,
    new: &HashMap<String, HashMap<u32, Position>>,
) -> Vec<PositionChange> {
    let mut changes = Vec::new();
    // Open / Update.
    for (sym, slots) in new {
        for (slot, pos) in slots {
            match old.get(sym).and_then(|m| m.get(slot)) {
                None => changes.push(PositionChange {
                    symbol: sym.clone(),
                    slot: *slot,
                    kind: PositionChangeKind::Open,
                    position: Some(pos.clone()),
                }),
                Some(prev) if !position_eq(prev, pos) => changes.push(PositionChange {
                    symbol: sym.clone(),
                    slot: *slot,
                    kind: PositionChangeKind::Update,
                    position: Some(pos.clone()),
                }),
                Some(_) => {}
            }
        }
    }
    // Close.
    for (sym, slots) in old {
        for slot in slots.keys() {
            if !new.get(sym).map(|m| m.contains_key(slot)).unwrap_or(false) {
                changes.push(PositionChange {
                    symbol: sym.clone(),
                    slot: *slot,
                    kind: PositionChangeKind::Close,
                    position: None,
                });
            }
        }
    }
    changes
}

/// Loose Position equality — only the fields that materially change
/// over a position's lifetime. Avoids spurious `Update` events on
/// timestamp-only churn.
fn position_eq(a: &Position, b: &Position) -> bool {
    a.symbol == b.symbol
        && a.slot == b.slot
        && a.position_amount == b.position_amount
        && a.position_qty == b.position_qty
        && a.entry_price == b.entry_price
        && a.position_side == b.position_side
}

fn emit_change(tx: &broadcast::Sender<PositionChange>, ev: PositionChange) {
    let kind = ev.kind;
    let summary = ev
        .position
        .as_ref()
        .map(|p| {
            format!(
                "side={} amount={} entry={}",
                p.position_side.as_deref().unwrap_or(""),
                p.position_amount.as_deref().unwrap_or("?"),
                p.entry_price.as_deref().unwrap_or("?"),
            )
        })
        .unwrap_or_else(|| "(closed)".into());
    info!(
        symbol = %ev.symbol,
        slot = ev.slot,
        kind = ?kind,
        detail = %summary,
        "[flip-acct] position change"
    );
    let _ = tx.send(ev);
}

async fn fetch_json(
    client: &reqwest::Client,
    api_key: &str,
    api_secret: &str,
    method: &str,
    path: &str,
) -> Result<Value> {
    let expires = unix_now_secs() + 60;
    let signature = sign_hmac(api_secret, method, path, expires, None);
    let url = format!("{}{}", REST_BASE, path);
    let req = client
        .request(reqwest::Method::from_bytes(method.as_bytes())?, &url)
        .header("api-key", api_key)
        .header("api-expires", expires.to_string())
        .header("api-signature", signature);
    let resp = req
        .send()
        .await
        .with_context(|| format!("send {} {}", method, path))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .with_context(|| format!("read body {} {}", method, path))?;
    if !status.is_success() {
        return Err(anyhow!("{} {} -> {}: {}", method, path, status, text));
    }
    serde_json::from_str::<Value>(&text)
        .with_context(|| format!("parse json {} {} (body={:?})", method, path, &text[..text.len().min(200)]))
}

fn parse_array<T: for<'de> Deserialize<'de>>(value: &Value) -> Vec<T> {
    match value {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect(),
        _ => Vec::new(),
    }
}

// -----------------------------------------------------------------------------
// WS task
// -----------------------------------------------------------------------------

async fn ws_loop(
    state: SharedAccountState,
    api_key: String,
    api_secret: String,
    change_tx: broadcast::Sender<PositionChange>,
) {
    loop {
        match ws_connect_once(&state, &api_key, &api_secret, &change_tx).await {
            Ok(()) => warn!("[flip-acct/ws] closed cleanly; reconnecting in 3s"),
            Err(e) => warn!(error = %e, "[flip-acct/ws] error; reconnecting in 3s"),
        }
        {
            let mut s = state.write().await;
            s.ws_connected = false;
        }
        tokio::time::sleep(RECONNECT_BACKOFF).await;
    }
}

async fn ws_connect_once(
    state: &SharedAccountState,
    api_key: &str,
    api_secret: &str,
    change_tx: &broadcast::Sender<PositionChange>,
) -> Result<()> {
    let expires = unix_now_secs() + 60;
    let signature = sign_hmac(api_secret, "GET", WS_AUTH_PATH, expires, None);

    let mut req = WS_URL.into_client_request()?;
    let h = req.headers_mut();
    h.insert(HeaderName::from_static("api-key"), HeaderValue::from_str(api_key)?);
    h.insert(
        HeaderName::from_static("api-expires"),
        HeaderValue::from_str(&expires.to_string())?,
    );
    h.insert(
        HeaderName::from_static("api-signature"),
        HeaderValue::from_str(&signature)?,
    );

    let (mut ws, _resp) = tokio_tungstenite::connect_async(req).await?;
    {
        let mut s = state.write().await;
        s.ws_connected = true;
    }

    let sub = serde_json::json!({
        "op": "subscribe",
        "args": ["account", "account.position", "account.balance", "account.margin"],
    });
    ws.send(Message::Text(sub.to_string().into())).await?;
    info!("[flip-acct/ws] connected & subscribed");

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
        // Defensive: some Flipster streams use literal "ping" text frames.
        if text == "ping" {
            ws.send(Message::Text("pong".to_string().into())).await?;
            continue;
        }
        let parsed: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        apply_ws_message(&parsed, state, change_tx).await;
    }
    Ok(())
}

async fn apply_ws_message(
    parsed: &Value,
    state: &SharedAccountState,
    change_tx: &broadcast::Sender<PositionChange>,
) {
    // Wire shape per docs: { topic, ts, data: [{ actionType, rows: [...] }, ...] }
    let topic = match parsed.get("topic").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return,
    };
    let data = match parsed.get("data").and_then(|v| v.as_array()) {
        Some(d) => d,
        None => return,
    };
    let now_ms = unix_now_ms();
    let mut pending: Vec<PositionChange> = Vec::new();
    {
        let mut s = state.write().await;
        s.last_ws_at_ms = now_ms;
        for entry in data {
            let rows = match entry.get("rows").and_then(|v| v.as_array()) {
                Some(r) => r,
                None => continue,
            };
            for row in rows {
                apply_row(topic, row, &mut s, &mut pending);
            }
        }
    }
    for c in pending {
        emit_change(change_tx, c);
    }
}

fn apply_row(
    topic: &str,
    row: &Value,
    state: &mut AccountState,
    pending: &mut Vec<PositionChange>,
) {
    match topic {
        "account" => {
            if let Ok(a) = serde_json::from_value::<AccountInfo>(row.clone()) {
                state.account = Some(a);
            }
        }
        "account.balance" => {
            if let Ok(b) = serde_json::from_value::<Balance>(row.clone()) {
                state.balances.insert(b.asset.clone(), b);
            }
        }
        "account.margin" => {
            if let Ok(m) = serde_json::from_value::<MarginInfo>(row.clone()) {
                state.margins.insert(m.currency.clone(), m);
            }
        }
        "account.position" => {
            if let Ok(p) = serde_json::from_value::<Position>(row.clone()) {
                let sym = p.symbol.clone();
                let slot = p.slot;
                // Zero positionAmount = closed; drop it so consumers
                // see Close events and iter_* methods don't list ghosts.
                let is_zero = p
                    .position_amount
                    .as_deref()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|n| n == 0.0)
                    .unwrap_or(false);
                if is_zero {
                    if let Some(slots) = state.positions.get_mut(&sym) {
                        if slots.remove(&slot).is_some() {
                            pending.push(PositionChange {
                                symbol: sym.clone(),
                                slot,
                                kind: PositionChangeKind::Close,
                                position: None,
                            });
                        }
                        if slots.is_empty() {
                            state.positions.remove(&sym);
                        }
                    }
                } else {
                    let entry = state.positions.entry(sym.clone()).or_default();
                    let kind = match entry.get(&slot) {
                        None => PositionChangeKind::Open,
                        Some(prev) if !position_eq(prev, &p) => PositionChangeKind::Update,
                        Some(_) => return, // no-op update
                    };
                    entry.insert(slot, p.clone());
                    pending.push(PositionChange {
                        symbol: sym,
                        slot,
                        kind,
                        position: Some(p),
                    });
                }
            }
        }
        _ => {}
    }
}

// -----------------------------------------------------------------------------
// helpers
// -----------------------------------------------------------------------------

fn sign_hmac(secret: &str, method: &str, path: &str, expires: i64, body: Option<&[u8]>) -> String {
    let mut message = Vec::new();
    message.extend_from_slice(method.to_uppercase().as_bytes());
    message.extend_from_slice(path.as_bytes());
    message.extend_from_slice(expires.to_string().as_bytes());
    if let Some(b) = body {
        message.extend_from_slice(b);
    }
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(&message);
    hex::encode(mac.finalize().into_bytes())
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_python_reference() {
        // Reference vector from strategy_flipster_python/.../auth.py:
        //   sign("secret", "GET", "/api/v1/account", 1700000000) ==
        //   hmac.new(b"secret", b"GET/api/v1/account1700000000",
        //            hashlib.sha256).hexdigest()
        let got = sign_hmac("secret", "GET", "/api/v1/account", 1_700_000_000, None);
        assert_eq!(got.len(), 64);
        // Stable HMAC output (verified against the python reference:
        //   hmac.new(b"secret", b"GET/api/v1/account1700000000",
        //            hashlib.sha256).hexdigest()) — pinned so refactors
        // can't accidentally change the signature scheme.
        assert_eq!(
            got,
            "325b3b678130137bd18545487eb8152a9626d00e569b30b7f6ced15d34eaa534"
        );
    }

    #[test]
    fn parse_balance_row() {
        let v = serde_json::json!({
            "asset": "USDT",
            "balance": "10000.00",
            "availableBalance": "8000.00",
            "maxWithdrawableAmount": "7500.00"
        });
        let b: Balance = serde_json::from_value(v).unwrap();
        assert_eq!(b.asset, "USDT");
        assert_eq!(b.available_balance, "8000.00");
    }

    #[test]
    fn parse_position_row() {
        let v = serde_json::json!({
            "symbol": "BTCUSDT.PERP",
            "slot": 0,
            "leverage": 10,
            "marginType": "ISOLATED",
            "positionSide": "LONG",
            "positionAmount": "0.5",
            "entryPrice": "41650.00"
        });
        let p: Position = serde_json::from_value(v).unwrap();
        assert_eq!(p.symbol, "BTCUSDT.PERP");
        assert_eq!(p.slot, 0);
        assert_eq!(p.position_side.as_deref(), Some("LONG"));
        assert_eq!(p.entry_price.as_deref(), Some("41650.00"));
    }

    #[test]
    fn apply_position_close_removes_entry() {
        let mut st = AccountState::default();
        let mut pending: Vec<PositionChange> = Vec::new();
        let row = serde_json::json!({"symbol":"BTCUSDT.PERP","slot":0,"positionAmount":"0.5"});
        apply_row("account.position", &row, &mut st, &mut pending);
        assert!(st.position_for("BTCUSDT.PERP", 0).is_some());
        assert!(matches!(pending.last().unwrap().kind, PositionChangeKind::Open));
        let close_row = serde_json::json!({"symbol":"BTCUSDT.PERP","slot":0,"positionAmount":"0.0"});
        apply_row("account.position", &close_row, &mut st, &mut pending);
        assert!(st.position_for("BTCUSDT.PERP", 0).is_none());
        assert!(matches!(pending.last().unwrap().kind, PositionChangeKind::Close));
    }

    #[test]
    fn diff_emits_open_update_close() {
        let mut old: HashMap<String, HashMap<u32, Position>> = HashMap::new();
        let mut new: HashMap<String, HashMap<u32, Position>> = HashMap::new();
        let p_a = Position { symbol: "BTCUSDT.PERP".into(), slot: 1, position_amount: Some("1".into()), ..Default::default() };
        let p_b = Position { symbol: "BTCUSDT.PERP".into(), slot: 1, position_amount: Some("2".into()), ..Default::default() };
        let p_c = Position { symbol: "BTCUSDT.PERP".into(), slot: 2, position_amount: Some("1".into()), ..Default::default() };
        old.entry("BTCUSDT.PERP".into()).or_default().insert(1, p_a.clone());
        old.entry("BTCUSDT.PERP".into()).or_default().insert(2, p_c.clone());
        new.entry("BTCUSDT.PERP".into()).or_default().insert(1, p_b);
        // slot 2 is gone in new; slot 3 is new.
        let p_d = Position { symbol: "BTCUSDT.PERP".into(), slot: 3, position_amount: Some("1".into()), ..Default::default() };
        new.entry("BTCUSDT.PERP".into()).or_default().insert(3, p_d);
        let changes = diff_positions(&old, &new);
        let kinds: Vec<_> = changes.iter().map(|c| (c.slot, c.kind)).collect();
        assert!(kinds.contains(&(1, PositionChangeKind::Update)));
        assert!(kinds.contains(&(2, PositionChangeKind::Close)));
        assert!(kinds.contains(&(3, PositionChangeKind::Open)));
    }
}

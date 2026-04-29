// Flipster private WS → populates GateOrderManager's internal state.
//
// Connects to wss://api.flipster.io/api/v2/stream/r230522?mode=subscription with
// CDP cookies, subscribes to private/positions, private/orders, private/margins,
// and translates Flipster messages into gate_hft's GatePosition / GateOpenOrder
// shape so that all downstream strategy code (handle_chance, decide_order, etc.)
// works unchanged.
//
// Position size scaling:
//   Flipster reports qty in native coin units (string-typed floats).
//   We store `size = round(qty * 1e6) as i64` and set quanto_multiplier = 1e-6
//   in the synthetic GateContract — preserves 6 decimals of precision.

use crate::flipster_cookies::fetch_flipster_cookies;
use crate::gate_order_manager::{FuturesAccountData, GateOpenOrder, GatePosition};
use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use futures_util::{SinkExt, StreamExt};
use log::{info, warn};
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

pub const POSITION_SIZE_SCALE: f64 = 1_000_000.0;
/// Must match POSITION_SIZE_SCALE reciprocal — inserted into synthetic GateContract.
pub const POSITION_QUANTO_MULTIPLIER: f64 = 1.0 / POSITION_SIZE_SCALE;

const WS_URL: &str = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription";
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/141.0.0.0 Safari/537.36";

/// Handles shared with GateOrderManager.
#[derive(Clone)]
pub struct FlipsterPrivateWsState {
    pub positions: Arc<ArcSwap<HashMap<String, GatePosition>>>,
    pub open_orders: Arc<ArcSwap<HashMap<String, GateOpenOrder>>>,
    pub futures_account_data: Arc<ArcSwap<Option<FuturesAccountData>>>,
    pub is_healthy: Arc<RwLock<bool>>,
    pub login_name: String,
    pub cdp_port: u16,
    pub shutdown: Arc<AtomicBool>,
}

pub fn spawn(state: FlipsterPrivateWsState, rt: tokio::runtime::Handle) {
    rt.spawn(async move {
        let notify_shutdown = Arc::new(Notify::new());
        let mut reconnect_delay = 1u64;
        loop {
            if state.shutdown.load(Ordering::SeqCst) {
                return;
            }
            match run_session(&state, &notify_shutdown).await {
                Ok(()) => {
                    info!("[{}] flipster-priv-ws session ended cleanly", state.login_name);
                    reconnect_delay = 1;
                }
                Err(e) => {
                    warn!(
                        "[{}] flipster-priv-ws err: {} — reconnect {}s",
                        state.login_name, e, reconnect_delay
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(reconnect_delay)).await;
            reconnect_delay = (reconnect_delay * 2).min(10);
        }
    });
}

async fn run_session(
    state: &FlipsterPrivateWsState,
    shutdown: &Arc<Notify>,
) -> Result<()> {
    // Try the global cookie store first (populated by runner); fall back to a
    // direct CDP fetch if not initialized yet.
    let cookie_hdr = if let Some(store) = crate::flipster_cookie_store::global() {
        store.cookie_header()
    } else {
        let fresh = fetch_flipster_cookies(state.cdp_port).await?;
        fresh
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("; ")
    };

    let mut req = WS_URL.into_client_request()?;
    let h = req.headers_mut();
    h.insert("Origin", "https://flipster.io".parse()?);
    h.insert("Cookie", cookie_hdr.parse()?);
    h.insert("User-Agent", USER_AGENT.parse()?);

    let (ws, _) = connect_async(req).await.context("connect flipster private ws")?;
    info!("[{}] flipster-priv-ws connected", state.login_name);

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
    *state.is_healthy.write() = true;

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
                    Message::Text(text) => handle_text(state, &text),
                    Message::Ping(p) => { let _ = write.send(Message::Pong(p)).await; }
                    Message::Close(_) => return Err(anyhow!("closed by server")),
                    _ => {}
                }
            }
        }
    }
}

fn handle_text(state: &FlipsterPrivateWsState, text: &str) {
    let d: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return,
    };
    let topic = match d.get("t").and_then(|v| v.as_object()) {
        Some(t) => t,
        None => return,
    };
    if let Some(pt) = topic.get("private/positions") {
        apply_positions(state, pt);
    }
    if let Some(ot) = topic.get("private/orders") {
        apply_orders(state, ot);
    }
    if let Some(mt) = topic.get("private/margins") {
        apply_margins(state, mt);
    }
}

fn apply_margins(state: &FlipsterPrivateWsState, topic: &Value) {
    let u = match topic.get("u").and_then(|v| v.as_object()) {
        Some(o) => o,
        None => return,
    };
    let usdt = match u.get("USDT").and_then(|v| v.as_object()) {
        Some(o) => o,
        None => return,
    };
    let mut total = None::<f64>;
    let mut unrealized = None::<f64>;
    if let Some(v) = str_num(usdt.get("marginBalance")) {
        total = Some(v);
    } else if let Some(v) = str_num(usdt.get("nonBonusCollateral")) {
        total = Some(v);
    }
    if let Some(v) = str_num(usdt.get("unrealizedPnl")) {
        unrealized = Some(v);
    }
    if total.is_some() || unrealized.is_some() {
        // Merge into existing (retain known fields).
        let cur = (**state.futures_account_data.load()).clone();
        let (cur_t, cur_u) = match cur {
            Some(d) => (
                d.total.parse().unwrap_or(0.0),
                d.unrealised_pnl.parse().unwrap_or(0.0),
            ),
            None => (0.0, 0.0),
        };
        let new_data = FuturesAccountData {
            total: format!("{}", total.unwrap_or(cur_t)),
            unrealised_pnl: format!("{}", unrealized.unwrap_or(cur_u)),
        };
        state.futures_account_data.store(Arc::new(Some(new_data)));
    }
}

fn apply_positions(state: &FlipsterPrivateWsState, topic: &Value) {
    let mut changed = false;
    let mut next_map = (**state.positions.load()).clone();

    // "s" / "i" / "u" — upsert
    for section in ["s", "i", "u"] {
        let map = match topic.get(section).and_then(|v| v.as_object()) {
            Some(o) => o,
            None => continue,
        };
        for (sym_slot, fields) in map {
            let Some(fields_obj) = fields.as_object() else {
                continue;
            };
            let contract = sym_slot.split('/').next().unwrap_or(sym_slot).to_string();
            let entry = next_map
                .entry(contract.clone())
                .or_insert_with(|| GatePosition {
                    contract: contract.clone(),
                    size: 0,
                    risk_limit: 0.0,
                    entry_price: 0.0,
                    mark_price: 0.0,
                    realised_pnl: 0.0,
                    last_close_pnl: 0.0,
                    history_pnl: 0.0,
                    update_time: chrono::Utc::now().timestamp(),
                });
            if let Some(v) = str_num(fields_obj.get("position")) {
                entry.size = (v * POSITION_SIZE_SCALE).round() as i64;
            }
            if let Some(v) = str_num(fields_obj.get("avgEntryPrice")) {
                entry.entry_price = v;
            }
            if let Some(v) = str_num(fields_obj.get("midPrice")) {
                entry.mark_price = v;
            } else if let Some(v) = str_num(fields_obj.get("markPrice")) {
                entry.mark_price = v;
            }
            if let Some(v) = str_num(fields_obj.get("netPnl")) {
                entry.realised_pnl = v;
            }
            if let Some(v) = str_num(fields_obj.get("initMarginReserved")) {
                // Drop zero-margin positions (Flipster keeps stale entries around after close).
                if v == 0.0 && entry.size == 0 {
                    next_map.remove(&contract);
                    changed = true;
                    continue;
                }
            }
            entry.update_time = chrono::Utc::now().timestamp();
            changed = true;
        }
    }
    // "d" — delete
    if let Some(d) = topic.get("d") {
        if let Some(obj) = d.as_object() {
            for k in obj.keys() {
                let contract = k.split('/').next().unwrap_or(k);
                next_map.remove(contract);
                changed = true;
            }
        } else if let Some(arr) = d.as_array() {
            for v in arr {
                if let Some(k) = v.as_str() {
                    let contract = k.split('/').next().unwrap_or(k);
                    next_map.remove(contract);
                    changed = true;
                }
            }
        }
    }

    if changed {
        state.positions.store(Arc::new(next_map));
    }
}

fn apply_orders(state: &FlipsterPrivateWsState, topic: &Value) {
    let mut changed = false;
    let mut next_map = (**state.open_orders.load()).clone();

    for section in ["s", "i", "u"] {
        let map = match topic.get(section).and_then(|v| v.as_object()) {
            Some(o) => o,
            None => continue,
        };
        for (oid, fields) in map {
            let Some(fields_obj) = fields.as_object() else {
                continue;
            };
            let order = next_map.entry(oid.clone()).or_insert_with(|| GateOpenOrder {
                id: oid.clone(),
                contract: String::new(),
                create_time_ms: 0,
                price: 0.0,
                size: 0,
                status: "open".to_string(),
                left: 0,
                is_reduce_only: false,
                is_close: false,
                tif: String::new(),
                text: String::new(),
                update_time: chrono::Utc::now().timestamp(),
                amend_text: String::new(),
            });
            if let Some(s) = fields_obj.get("symbol").and_then(|v| v.as_str()) {
                order.contract = s.to_string();
            }
            if let Some(s) = fields_obj.get("side").and_then(|v| v.as_str()) {
                // Long → positive, Short → negative
                let sign = if s.eq_ignore_ascii_case("Long") { 1.0 } else { -1.0 };
                if let Some(v) = str_num(fields_obj.get("qty")) {
                    order.size = (v * POSITION_SIZE_SCALE * sign).round() as i64;
                }
                if let Some(v) = str_num(fields_obj.get("leavesQty")) {
                    order.left = (v * POSITION_SIZE_SCALE * sign).round() as i64;
                }
            }
            if let Some(v) = str_num(fields_obj.get("price")) {
                order.price = v;
            }
            if let Some(v) = fields_obj.get("isReduceOnly").and_then(|v| v.as_bool()) {
                order.is_reduce_only = v;
            }
            if let Some(t) = fields_obj.get("orderType").and_then(|v| v.as_str()) {
                order.tif = if t.contains("LIMIT") { "gtc".into() } else { "ioc".into() };
            }
            if let Some(ms) = str_num(fields_obj.get("orderTime")) {
                order.create_time_ms = (ms / 1e6) as i64; // orderTime is ns
            }
            order.update_time = chrono::Utc::now().timestamp();
            changed = true;
        }
    }
    if let Some(d) = topic.get("d") {
        if let Some(obj) = d.as_object() {
            for k in obj.keys() {
                next_map.remove(k);
                changed = true;
            }
        } else if let Some(arr) = d.as_array() {
            for v in arr {
                if let Some(k) = v.as_str() {
                    next_map.remove(k);
                    changed = true;
                }
            }
        }
    }
    if changed {
        state.open_orders.store(Arc::new(next_map));
    }
}

fn str_num(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        return s.parse().ok();
    }
    None
}

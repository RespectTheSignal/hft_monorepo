//! Master account state cache.
//!
//! Single source of truth for "what does master ACTUALLY hold right now".
//! Refreshed every 5 s via clearinghouseState + openOrders REST. The
//! strategy reads this cache before placing new entries to enforce
//! "1 position per coin at master level" — independent of paper bot's
//! own position-counting.
//!
//! Key API:
//!   - `position_size(coin)` → master szi (0.0 if flat)
//!   - `position_age_secs(coin)` → seconds since entry, or None if flat
//!   - `position_pnl_bp(coin)` → unrealized PnL in bp, or None if flat
//!   - `open_order_oids_for(coin)` → live maker exits
//!   - `account_value()` → total margin
//!
//! Refresh logic runs in `spawn(master_addr, testnet)`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct PositionSnapshot {
    pub coin: String,
    pub szi: f64,
    pub entry_px: f64,
    pub mark_px: f64,
    pub unrealized_pnl: f64,
    pub margin_used: f64,
    /// First time this position was observed. Used for live timeout.
    pub first_seen: DateTime<Utc>,
}

impl PositionSnapshot {
    pub fn pnl_bp(&self) -> f64 {
        if self.entry_px <= 0.0 {
            return 0.0;
        }
        let dir = if self.szi > 0.0 { 1.0 } else { -1.0 };
        (self.mark_px - self.entry_px) / self.entry_px * 10_000.0 * dir
    }
    pub fn age_secs(&self, now: DateTime<Utc>) -> i64 {
        (now - self.first_seen).num_seconds()
    }
}

#[derive(Debug, Clone)]
pub struct OpenOrder {
    pub coin: String,
    pub oid: u64,
    pub is_buy: bool,
    pub limit_px: f64,
    pub sz: f64,
    pub reduce_only: bool,
}

#[derive(Default, Debug)]
struct State {
    positions: HashMap<String, PositionSnapshot>,
    orders: Vec<OpenOrder>,
    account_value: f64,
    last_refresh: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct LiveState {
    inner: Arc<RwLock<State>>,
}

impl LiveState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(State::default())),
        }
    }

    pub fn position_size(&self, coin: &str) -> f64 {
        self.inner
            .read()
            .positions
            .get(coin)
            .map(|p| p.szi)
            .unwrap_or(0.0)
    }
    pub fn position(&self, coin: &str) -> Option<PositionSnapshot> {
        self.inner.read().positions.get(coin).cloned()
    }
    pub fn open_order_oids_for(&self, coin: &str) -> Vec<u64> {
        self.inner
            .read()
            .orders
            .iter()
            .filter(|o| o.coin == coin)
            .map(|o| o.oid)
            .collect()
    }
    pub fn account_value(&self) -> f64 {
        self.inner.read().account_value
    }
    pub fn last_refresh(&self) -> Option<DateTime<Utc>> {
        self.inner.read().last_refresh
    }
    pub fn n_positions(&self) -> usize {
        self.inner.read().positions.len()
    }
    pub fn n_open_orders(&self) -> usize {
        self.inner.read().orders.len()
    }

    /// Has master ANY exposure on this coin (position OR resting order)?
    /// New entries must skip if true.
    pub fn coin_busy(&self, coin: &str) -> bool {
        let s = self.inner.read();
        if s.positions.get(coin).map(|p| p.szi.abs() > 0.0).unwrap_or(false) {
            return true;
        }
        s.orders.iter().any(|o| o.coin == coin)
    }
}

/// Spawn the periodic refresh task. Returns a handle to the cache.
pub fn spawn(master_addr: String, testnet: bool, interval_s: u64) -> LiveState {
    let state = LiveState::new();
    let writer = state.clone();
    let url = if testnet {
        "https://api.hyperliquid-testnet.xyz/info"
    } else {
        "https://api.hyperliquid.xyz/info"
    };
    info!(master = %master_addr, "live_state: refresh every {}s", interval_s);
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("live_state: client init: {e}");
                return;
            }
        };
        let mut interval = tokio::time::interval(Duration::from_secs(interval_s));
        loop {
            interval.tick().await;
            if let Err(e) = refresh_once(&client, url, &master_addr, &writer).await {
                warn!("live_state refresh: {e}");
            }
        }
    });
    state
}

async fn refresh_once(
    client: &reqwest::Client,
    info_url: &str,
    master: &str,
    writer: &LiveState,
) -> Result<()> {
    // 1) clearinghouseState
    let body = serde_json::json!({"type":"clearinghouseState","user":master});
    let v: serde_json::Value = client.post(info_url).json(&body).send().await?
        .json().await.context("ch json")?;
    let ms = v.get("marginSummary").cloned().unwrap_or(serde_json::Value::Null);
    let acct_value: f64 = ms
        .get("accountValue").and_then(|x| x.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let now = Utc::now();
    let mut new_positions: HashMap<String, PositionSnapshot> = HashMap::new();
    let asset_positions = v.get("assetPositions").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    // Preserve first_seen from previous snapshot if same coin/szi/entry
    let prev = writer.inner.read().positions.clone();
    for ap in asset_positions {
        let p = match ap.get("position") { Some(p) => p, None => continue };
        let coin = match p.get("coin").and_then(|x| x.as_str()) { Some(c) => c.to_string(), None => continue };
        let szi: f64 = p.get("szi").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        if szi == 0.0 { continue; }
        let entry_px: f64 = p.get("entryPx").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let unr: f64 = p.get("unrealizedPnl").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let margin: f64 = p.get("marginUsed").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        // mark_px ≈ entry_px + unrealized_pnl_per_unit (rough)
        let mark_px = if szi != 0.0 { entry_px + unr / szi } else { entry_px };
        // first_seen carries over if (coin, szi sign) didn't flip
        let first_seen = prev.get(&coin)
            .filter(|prev_pos| prev_pos.szi.signum() == szi.signum())
            .map(|p| p.first_seen)
            .unwrap_or(now);
        new_positions.insert(coin.clone(), PositionSnapshot {
            coin, szi, entry_px, mark_px,
            unrealized_pnl: unr, margin_used: margin, first_seen,
        });
    }

    // 2) openOrders
    let body = serde_json::json!({"type":"openOrders","user":master});
    let v: serde_json::Value = client.post(info_url).json(&body).send().await?
        .json().await.context("oo json")?;
    let mut new_orders: Vec<OpenOrder> = Vec::new();
    for o in v.as_array().cloned().unwrap_or_default() {
        let coin = o.get("coin").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let oid = o.get("oid").and_then(|x| x.as_u64()).unwrap_or(0);
        if coin.is_empty() || oid == 0 { continue; }
        let is_buy = o.get("side").and_then(|x| x.as_str()).map(|s| s == "B").unwrap_or(false);
        let limit_px: f64 = o.get("limitPx").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let sz: f64 = o.get("sz").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let reduce_only = o.get("reduceOnly").and_then(|x| x.as_bool()).unwrap_or(false);
        new_orders.push(OpenOrder { coin, oid, is_buy, limit_px, sz, reduce_only });
    }

    let mut s = writer.inner.write();
    s.positions = new_positions;
    s.orders = new_orders;
    s.account_value = acct_value;
    s.last_refresh = Some(now);
    debug!(
        n_pos = s.positions.len(),
        n_oo = s.orders.len(),
        acct = s.account_value,
        "live_state refreshed"
    );
    Ok(())
}

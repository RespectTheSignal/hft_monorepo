// Flipster order manager — HTTP submit/cancel + private WS state.
// Mirrors Python _submit_one_way_order, _open_position cancel path.
//
// Flipster is Cloudflare-protected; the Python bot uses curl_cffi with Chrome TLS
// impersonation + a rotating HTTP proxy pool. Pure reqwest may be rejected on some
// endpoints — if that happens, fall back to running requests through Python via a
// sidecar, or add a crate like `rquest` with Chrome fingerprint.

use crate::cookies::fetch_flipster_cookies;
use crate::private_ws::{PrivateWsClient, PrivateWsState};
use crate::symbols::SymbolRegistry;
use crate::types::Side;
use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, COOKIE};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use uuid::Uuid;

const API_BASE: &str = "https://api.flipster.io";
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    Market,
    Limit,
}

impl OrderType {
    fn as_str(&self) -> &'static str {
        match self {
            OrderType::Market => "ORDER_TYPE_MARKET",
            OrderType::Limit => "ORDER_TYPE_LIMIT",
        }
    }
}

/// Request passed to submit_order.
#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub symbol: String,
    pub side: Side,
    pub price: f64,
    /// USD notional (Flipster `amount` field).
    pub amount_usd: f64,
    pub order_type: OrderType,
    pub post_only: bool,
    pub reduce_only: bool,
    pub leverage: i32,
}

#[derive(Debug, Clone)]
pub struct OrderResponse {
    pub order_id: Option<String>,
    pub avg_fill: f64,
    pub filled_size: f64,
    pub raw: Value,
}

pub struct FlipsterOrderManager {
    http: reqwest::Client,
    cookies: Arc<ArcSwap<Vec<(String, String)>>>,
    cdp_port: u16,
    symbols: Arc<SymbolRegistry>,
    state: PrivateWsState,
    /// Learned per-symbol leverage caps (set when we hit TooHighLeverage).
    /// Mirrors Python _SYM_LEV_CAP.
    leverage_caps: Arc<DashMap<String, i32>>,
}

impl FlipsterOrderManager {
    /// Fetch initial cookies + spawn private WS.
    pub async fn new(symbols: Arc<SymbolRegistry>, cdp_port: u16) -> Result<Arc<Self>> {
        let cookies = fetch_flipster_cookies(cdp_port).await?;
        info!("fetched {} flipster cookies", cookies.len());
        let cookies = Arc::new(ArcSwap::from_pointee(cookies));
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(10))
            .build()?;
        let state = PrivateWsState::new();
        let mgr = Arc::new(Self {
            http,
            cookies: cookies.clone(),
            cdp_port,
            symbols,
            state: state.clone(),
            leverage_caps: Arc::new(DashMap::new()),
        });

        let ws_client = Arc::new(PrivateWsClient::new(state, cookies.clone()));
        ws_client.spawn();

        // Background cookie refresher every 15min (flipster sessions persist but
        // cookie rotation protects against idle invalidation).
        let mgr_bg = mgr.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(900)).await;
                if let Err(e) = mgr_bg.refresh_cookies().await {
                    warn!("cookie refresh failed: {}", e);
                }
            }
        });

        Ok(mgr)
    }

    pub fn state(&self) -> &PrivateWsState {
        &self.state
    }

    pub fn symbols(&self) -> &SymbolRegistry {
        &self.symbols
    }

    pub async fn refresh_cookies(&self) -> Result<()> {
        let new = fetch_flipster_cookies(self.cdp_port).await?;
        info!("refreshed {} cookies", new.len());
        self.cookies.store(Arc::new(new));
        Ok(())
    }

    fn cookie_header(&self) -> String {
        self.cookies
            .load()
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn base_headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert(COOKIE, HeaderValue::from_str(&self.cookie_header())?);
        h.insert(
            HeaderName::from_static("origin"),
            HeaderValue::from_static("https://flipster.io"),
        );
        h.insert(
            HeaderName::from_static("referer"),
            HeaderValue::from_static("https://flipster.io/"),
        );
        h.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("application/json"),
        );
        Ok(h)
    }

    pub async fn submit_order(&self, req: &OrderRequest) -> Result<OrderResponse> {
        // Apply learned per-sym leverage cap.
        let mut effective_lev = req.leverage;
        if let Some(cap) = self.leverage_caps.get(&req.symbol) {
            effective_lev = effective_lev.min(*cap);
        }

        let url = format!("{}/api/v2/trade/one-way/order/{}", API_BASE, req.symbol);
        let mut attempts = 0;
        loop {
            attempts += 1;
            let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
            let now_ns_s = now_ns.to_string();
            let body = json!({
                "side": req.side.flipster_side(),
                "requestId": Uuid::new_v4().to_string(),
                "timestamp": now_ns_s,
                "reduceOnly": req.reduce_only,
                "refServerTimestamp": now_ns_s,
                "refClientTimestamp": now_ns_s,
                "leverage": effective_lev,
                "price": format!("{}", req.price),
                "amount": format!("{:.4}", req.amount_usd),
                "marginType": "Cross",
                "orderType": req.order_type.as_str(),
                "postOnly": req.post_only,
            });

            let resp = self
                .http
                .post(&url)
                .headers(self.base_headers()?)
                .json(&body)
                .send()
                .await
                .context("POST order")?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

            if status.is_success() {
                return Ok(extract_fill(parsed));
            }

            // TooHighLeverage → learn cap, retry with halved leverage.
            if status.as_u16() == 400 && text.contains("TooHighLeverage") && attempts < 4 {
                let new_cap = (effective_lev / 2).max(1);
                self.leverage_caps.insert(req.symbol.clone(), new_cap);
                info!(
                    "[lev-cap] {}: {} → {} (learned from TooHighLeverage)",
                    req.symbol, effective_lev, new_cap
                );
                effective_lev = new_cap;
                continue;
            }

            // CannotDecreaseLeverage → bump leverage to max and retry once.
            if status.as_u16() == 400 && text.contains("CannotDecreaseLeverage") && attempts < 2 {
                effective_lev = req.leverage.max(100);
                continue;
            }

            // 429 / 5xx — short backoff + retry.
            if (status.as_u16() == 429 || status.as_u16() >= 500) && attempts < 3 {
                tokio::time::sleep(Duration::from_millis(200 * attempts as u64)).await;
                continue;
            }

            return Err(anyhow!(
                "order error {}: {}",
                status.as_u16(),
                truncate(&text, 400)
            ));
        }
    }

    pub async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<()> {
        let url = format!(
            "{}/api/v2/trade/orders/{}/{}",
            API_BASE, symbol, order_id
        );
        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let body = json!({
            "requestId": Uuid::new_v4().to_string(),
            "timestamp": now_ns.to_string(),
            "attribution": "TRADE_PERPETUAL_DEFAULT",
        });
        let resp = self
            .http
            .delete(&url)
            .headers(self.base_headers()?)
            .json(&body)
            .send()
            .await
            .context("DELETE order")?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            return Ok(());
        }
        let text = resp.text().await.unwrap_or_default();
        // Cancel race — order already filled or gone
        if text.contains("AlreadyFilled") || text.contains("NotFound") {
            return Ok(());
        }
        Err(anyhow!("cancel error {}: {}", status.as_u16(), truncate(&text, 300)))
    }
}

fn extract_fill(v: Value) -> OrderResponse {
    let pos = v.get("position").cloned().unwrap_or(Value::Null);
    let order = v.get("order").cloned().unwrap_or(Value::Null);
    let avg = first_str_num(&[pos.get("avgPrice"), order.get("avgPrice")]);
    let size = first_str_num(&[pos.get("size"), order.get("size")])
        .map(|x| x.abs())
        .unwrap_or(0.0);
    let order_id = order
        .get("orderId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    OrderResponse {
        order_id,
        avg_fill: avg.unwrap_or(0.0),
        filled_size: size,
        raw: v,
    }
}

fn first_str_num(opts: &[Option<&Value>]) -> Option<f64> {
    for o in opts {
        if let Some(v) = o {
            if let Some(n) = v.as_f64() {
                return Some(n);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.parse::<f64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

// Gate.io Order Manager (Rust version)
// Manages positions, contracts, and order state via Gate REST API and WebSocket

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use crossbeam_channel;
use crossbeam_queue::ArrayQueue;
use futures_util::{SinkExt, StreamExt};
use hex;
use hmac::{Hmac, Mac};
use log::{debug, error, info, trace, warn};
use parking_lot::RwLock;
use rand::seq::SliceRandom;
use redis::{Client as RedisClient, Commands};
use reqwest::blocking::Client;
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha512;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{connect_async_with_config, tungstenite::Message};

use crate::config::TradeSettings;
use crate::order_manager_client::OrderRequest;
use crate::order_processor_grpc::order_processor::PlaceOrderResponse;
use crate::strategy_utils::calculate_mid_profit_bp;
use crate::{elog, plog};

const GATE_FUTURES_API_BASE: &str = "https://api.gateio.ws/api/v4/futures/usdt";
const GATE_WS_URL: &str = "wss://fx-ws.gateio.ws/v4/ws/usdt";
const WAIT_FOR_BOOK_TICKER_AFTER_TRADE_MS: i64 = 20; // match Python wait (approx)
const WAIT_FOR_BOOK_TICKER_AFTER_TRADE_ENV: &str = "WAIT_FOR_BOOK_TICKER_AFTER_TRADE_MS";
const EPS_F64: f64 = 1e-12;
/// When open order price is within this fraction of gate bookticker (ask for buy, bid for sell), we normalize: size>0 => price/2, size<0 => price*2.
const OPEN_ORDER_PRICE_CLOSE_TO_BOOKTICKER_RATIO: f64 = 0.02;
// const HEALTH_CHECK_PRICE: f64 = 60000.0; // align with Python DEBUGGING_ORDERS price

static LAST_ORDER_AMEND_LOG_MS: AtomicI64 = AtomicI64::new(0);

/// Lock-free queue + notify for WebSocket writer; producers push, single writer task drains.
const WS_QUEUE_CAP: usize = 65536;

pub(crate) struct WsWriterHandle {
    pub(crate) queue: Arc<ArrayQueue<Message>>,
    pub(crate) notify: Arc<Notify>,
    pub(crate) shutdown: Arc<AtomicBool>,
}

impl WsWriterHandle {
    /// Push message and wake writer; returns false if queue full (caller may clear ws_sender).
    pub(crate) fn push_message(&self, msg: Message) -> bool {
        if self.queue.push(msg).is_err() {
            return false;
        }
        self.notify.notify_one();
        true
    }
}

#[derive(Debug, Clone)]
pub struct LastOrder {
    pub contract: String,
    pub level: String,
    pub side: String,
    pub price: f64,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatePosition {
    pub contract: String,
    pub size: i64,
    pub risk_limit: f64,
    pub entry_price: f64,
    pub mark_price: f64,
    pub realised_pnl: f64,
    pub last_close_pnl: f64,
    pub history_pnl: f64,
    // Update time in secondsc
    pub update_time: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct GateFuturesTicker {
    contract: String,
    change_percentage: String,
}

/// Open order from futures.orders WebSocket (event "update", status "open").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateOpenOrder {
    pub id: String,
    pub contract: String,
    pub create_time_ms: i64,
    pub price: f64,
    pub size: i64,
    pub status: String,
    pub left: i64,
    pub is_reduce_only: bool,
    pub is_close: bool,
    pub tif: String,
    pub text: String,
    pub update_time: i64,
    pub amend_text: String,
}

/// Gate WebSocket API rate limit info from response header.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GateRateLimitState {
    pub requests_remain: Option<i64>,
    pub limit: Option<i64>,
    pub reset_timestamp: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateContract {
    pub name: String,
    #[serde(rename = "type")]
    pub contract_type: String,
    pub quanto_multiplier: String,
    pub leverage_min: String,
    pub leverage_max: String,
    pub mark_price: String,
    pub funding_rate: String,
    pub order_size_min: u64,
    pub order_size_max: u64,
    pub order_price_round: String,
}

/// Process-wide TTL for [`update_gate_contracts_cached`]; avoids duplicate `/contracts` HTTP calls across managers.
const GATE_CONTRACTS_CACHE_TTL: Duration = Duration::from_secs(10);

static GATE_CONTRACTS_CACHE: parking_lot::Mutex<
    Option<(Instant, Arc<HashMap<String, GateContract>>)>,
> = parking_lot::Mutex::new(None);

/// Fetches USDT futures contracts from Gate and stores them in `contracts`, at most one HTTP GET per
/// [`GATE_CONTRACTS_CACHE_TTL`] for the whole process (shared cache).
pub fn update_gate_contracts_cached(
    client: &Client,
    contracts: &ArcSwap<HashMap<String, GateContract>>,
) -> Result<()> {
    let now = Instant::now();
    {
        let guard = GATE_CONTRACTS_CACHE.lock();
        if let Some((t, arc)) = guard.as_ref() {
            if now.duration_since(*t) < GATE_CONTRACTS_CACHE_TTL {
                contracts.store(Arc::clone(arc));
                return Ok(());
            }
        }
    }

    // Flipster mode: build synthetic GateContract map from scripts/symbols_v9.json.
    if crate::flipster_cookie_store::global().is_some() {
        return crate::flipster_contracts::load_into(contracts);
    }

    let url = format!("{}/contracts", GATE_FUTURES_API_BASE);
    let response = client
        .get(&url)
        .send()
        .context("Failed to fetch contracts from Gate API")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response
            .text()
            .unwrap_or_else(|_| "Unknown error".to_string());
        return Err(anyhow::anyhow!("Gate API error ({}): {}", status, text));
    }

    let parsed: Vec<GateContract> = response
        .json()
        .context("Failed to parse contracts response")?;

    let new_map: HashMap<String, GateContract> =
        parsed.into_iter().map(|c| (c.name.clone(), c)).collect();
    let new_arc = Arc::new(new_map);

    let now = Instant::now();
    let mut guard = GATE_CONTRACTS_CACHE.lock();
    if let Some((t, arc)) = guard.as_ref() {
        if now.duration_since(*t) < GATE_CONTRACTS_CACHE_TTL {
            contracts.store(Arc::clone(arc));
            return Ok(());
        }
    }
    *guard = Some((now, Arc::clone(&new_arc)));
    contracts.store(new_arc);
    Ok(())
}

/// Process-wide TTL for [`fetch_top_change_percentage_symbols_cached`] (`/tickers`).
const GATE_FUTURES_TICKERS_CACHE_TTL: Duration = Duration::from_secs(10);

static GATE_TOP_CHANGE_TICKERS_CACHE: parking_lot::Mutex<Option<(Instant, Arc<Vec<String>>)>> =
    parking_lot::Mutex::new(None);

/// Contracts sorted by descending `|change_percentage|` from Gate `/tickers`, shared across callers; at most one HTTP GET per
/// [`GATE_FUTURES_TICKERS_CACHE_TTL`]. `top_n` slices the cached ordering without extra requests.
pub fn fetch_top_change_percentage_symbols_cached(
    client: &Client,
    top_n: usize,
) -> Result<Vec<String>> {
    let now = Instant::now();
    {
        let guard = GATE_TOP_CHANGE_TICKERS_CACHE.lock();
        if let Some((t, arc)) = guard.as_ref() {
            if now.duration_since(*t) < GATE_FUTURES_TICKERS_CACHE_TTL {
                return Ok(arc.iter().take(top_n).cloned().collect());
            }
        }
    }

    // Flipster mode: no /tickers change-percentage API equivalent — return empty
    // (limit_open_blocked_symbols stays empty, nothing is blocked on that basis).
    if crate::flipster_cookie_store::global().is_some() {
        let _ = top_n;
        return Ok(Vec::new());
    }

    let url = format!("{}/tickers", GATE_FUTURES_API_BASE);
    let response = client
        .get(&url)
        .send()
        .context("Failed to fetch futures tickers")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response
            .text()
            .unwrap_or_else(|_| "Unknown error".to_string());
        return Err(anyhow::anyhow!("Gate API error ({}): {}", status, text));
    }

    let tickers: Vec<GateFuturesTicker> =
        response.json().context("Failed to parse futures tickers")?;

    let mut entries: Vec<(String, f64)> = tickers
        .into_iter()
        .filter_map(|ticker| {
            ticker
                .change_percentage
                .parse::<f64>()
                .ok()
                .map(|change| (ticker.contract, change))
        })
        .collect();

    entries.sort_by(|a, b| {
        b.1.abs()
            .partial_cmp(&a.1.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let full_sorted: Vec<String> = entries.into_iter().map(|(c, _)| c).collect();
    let full_arc = Arc::new(full_sorted);

    let now = Instant::now();
    let mut guard = GATE_TOP_CHANGE_TICKERS_CACHE.lock();
    if let Some((t, arc)) = guard.as_ref() {
        if now.duration_since(*t) < GATE_FUTURES_TICKERS_CACHE_TTL {
            return Ok(arc.iter().take(top_n).cloned().collect());
        }
    }
    *guard = Some((now, Arc::clone(&full_arc)));
    Ok(full_arc.iter().take(top_n).cloned().collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuturesAccountData {
    pub total: String,
    pub unrealised_pnl: String,
    // Add other fields as needed
}

/// Snapshot of manager state for one symbol — one batch of try_reads; use in strategy to reduce lock traffic. Fail = skip this opportunity.
#[derive(Clone)]
pub struct StrategySymbolSnapshot {
    pub contract: GateContract,
    pub order_size: Option<f64>,
    pub order_count_size: Option<f64>,
    pub usdt_position_size: f64,
    pub avg_entry_price: Option<f64>,
    pub futures_account_data: Option<(f64, f64)>,
}

pub struct GateOrderManager {
    pub login_name: String,
    api_key: String,
    api_secret: String,
    pub uid: i32,
    client: Client,

    /// Lock-free reads via ArcSwap; write never blocks read.
    pub trade_settings: Arc<ArcSwap<crate::config::TradeSettings>>,

    /// Lock-free reads via ArcSwap (stores Arc<HashMap>); write never blocks read.
    pub positions: Arc<ArcSwap<HashMap<String, GatePosition>>>,
    /// Lock-free reads via ArcSwap (stores Arc<HashMap>); write never blocks read.
    pub contracts: Arc<ArcSwap<HashMap<String, GateContract>>>,

    /// Open orders (status == "open") from futures.orders WebSocket; key = order id.
    pub open_orders: Arc<ArcSwap<HashMap<String, GateOpenOrder>>>,

    /// Cache for get_open_orders_for_contract: when open_orders Arc unchanged, return cached Vec per symbol to avoid iterating all orders every tick.
    open_orders_by_contract_cache: Arc<
        parking_lot::RwLock<(
            Option<Arc<HashMap<String, GateOpenOrder>>>,
            HashMap<String, Vec<GateOpenOrder>>,
        )>,
    >,

    // Metrics
    pub order_count_map: Arc<RwLock<HashMap<String, i64>>>,
    /// Lock-free reads via ArcSwap; write never blocks read.
    pub trade_count_map: Arc<ArcSwap<HashMap<String, i64>>>,
    /// Lock-free reads via ArcSwap; write never blocks read.
    pub profitable_trade_count_map: Arc<ArcSwap<HashMap<String, i64>>>,
    pub profit_bp_ema_map: Arc<RwLock<HashMap<String, f64>>>,
    // Last update time
    pub last_update: Arc<RwLock<SystemTime>>,
    pub last_position_updated_time_ms: Arc<RwLock<i64>>,

    // Symbols this account is trading (for is_account_symbol check)
    pub symbols: Arc<RwLock<Vec<String>>>,
    pub close_symbols: Arc<RwLock<Vec<String>>>,

    // Order size in USDT (for get_order_count_size calculation)
    /// Lock-free reads via ArcSwap; write never blocks read.
    pub order_size: Arc<ArcSwap<f64>>,

    /// Futures account data — lock-free reads via ArcSwap; write never blocks read.
    pub futures_account_data: Arc<ArcSwap<Option<FuturesAccountData>>>,

    pub last_orders: Arc<RwLock<HashMap<String, LastOrder>>>,

    pub last_ws_orders: Arc<RwLock<HashMap<String, LastOrder>>>,

    /// Lock-free read; write is done in background thread (on_new_order only sends to channel).
    pub recent_orders_queue: Arc<ArcSwap<VecDeque<LastOrder>>>,

    /// Sender for deferring recent_orders_queue + last_orders update to a background thread.
    order_record_tx: Arc<crossbeam_channel::Sender<(String, LastOrder)>>,

    // DataCache for getting last_book_ticker for print_trade
    data_cache: Option<Arc<crate::data_cache::DataCache>>,

    // Health check status
    pub is_healthy: Arc<RwLock<bool>>,

    /// Gate WebSocket API rate limit (from response header: x_gate_ratelimit_*).
    pub gate_rate_limit: Arc<ArcSwap<GateRateLimitState>>,

    pub last_too_many_orders_time_ms: Arc<RwLock<i64>>,

    // Symbols blocked from limit_open due to top change_percentage
    limit_open_blocked_symbols: Arc<RwLock<HashSet<String>>>,
    // Symbols blocked from limit_open due to Redis dangerous symbols
    dangerous_limit_open_symbols: Arc<RwLock<HashSet<String>>>,
    // Symbols that should trigger market_close (from Redis)
    market_close_symbols: Arc<RwLock<HashSet<String>>>,

    // Callback for successful trades (event-based reordering)
    trade_callback: Arc<RwLock<Option<Box<dyn Fn(&str, i64, f64, f64, f64) + Send + Sync>>>>,

    // WebSocket writer handle (lock-free queue + notify); None when disconnected.
    ws_sender: Arc<RwLock<Option<Arc<WsWriterHandle>>>>,

    // Last trade time map (symbol -> last trade time in ms)
    pub last_trade_time_map: Arc<RwLock<HashMap<String, i64>>>,

    // WebSocket order scale (same as Python ws_order_scale)
    pub ws_order_scale: f64,

    // Save metrics
    pub save_metrics: bool,

    // Update futures account data
    pub update_futures_account_data: bool,

    // Run health check
    pub run_health_check: bool,

    // Run small orders loop
    pub run_small_orders_loop: bool,

    /// When false, do not log order/trade/position details (print_order, print_trade, print_position).
    pub print_orders: bool,

    /// Resolved login-status URL template (with {login_name} placeholder). Supabase trading_runtime > .env > default.
    login_status_url_template: String,

    /// Per-account OrderManagerClient (trading_runtime overrides). When set, follow-up/repeat_profitable/health use it; else env-only client.
    order_manager_client: Option<Arc<crate::order_manager_client::OrderManagerClient>>,

    /// When true, do not close symbols that are not in self.symbols but have position.
    pub is_monitoring: bool,

    /// Sum of `get_net_positions_usdt_size()` across all accounts (WS multi-account). Updated each
    /// strategy signal tick. When `None`, repeat-profitable net-side check uses this account only.
    pub total_net_positions_usdt_size_all_accounts: Option<Arc<ArcSwap<f64>>>,
}

impl GateOrderManager {
    pub fn new(
        login_name: String,
        api_key: String,
        api_secret: String,
        uid: i32,
        symbols: Vec<String>,
        order_size: f64,
        data_cache: Option<Arc<crate::data_cache::DataCache>>,
        trade_settings: crate::config::TradeSettings,
        save_metrics: bool,
        update_futures_account_data: bool,
        run_health_check: bool,
        print_orders: bool,
        login_status_url_template: String,
        order_manager_client: Option<Arc<crate::order_manager_client::OrderManagerClient>>,
        run_small_orders_loop: bool,
        is_monitoring: bool,
        total_net_positions_usdt_size_all_accounts: Option<Arc<ArcSwap<f64>>>,
    ) -> Self {
        let last_orders = Arc::new(RwLock::new(HashMap::new()));
        let recent_orders_queue = Arc::new(ArcSwap::from_pointee(VecDeque::new()));
        let order_record_tx = Self::spawn_order_record_thread(
            Arc::clone(&recent_orders_queue),
            Arc::clone(&last_orders),
        );

        Self {
            login_name,
            api_key,
            api_secret,
            uid,
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            positions: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            contracts: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            open_orders: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            open_orders_by_contract_cache: Arc::new(RwLock::new((None, HashMap::new()))),
            order_count_map: Arc::new(RwLock::new(HashMap::new())),
            trade_count_map: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            profitable_trade_count_map: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            profit_bp_ema_map: Arc::new(RwLock::new(HashMap::new())),
            last_update: Arc::new(RwLock::new(SystemTime::now())),
            last_position_updated_time_ms: Arc::new(RwLock::new(0)),
            symbols: Arc::new(RwLock::new(symbols)),
            order_size: Arc::new(ArcSwap::from_pointee(order_size)),
            futures_account_data: Arc::new(ArcSwap::from_pointee(None)),
            data_cache,
            is_healthy: Arc::new(RwLock::new(false)),
            gate_rate_limit: Arc::new(ArcSwap::from_pointee(GateRateLimitState::default())),
            last_too_many_orders_time_ms: Arc::new(RwLock::new(0)),
            last_ws_orders: Arc::new(RwLock::new(HashMap::new())),
            limit_open_blocked_symbols: Arc::new(RwLock::new(HashSet::new())),
            dangerous_limit_open_symbols: Arc::new(RwLock::new(HashSet::new())),
            market_close_symbols: Arc::new(RwLock::new(HashSet::new())),
            trade_callback: Arc::new(RwLock::new(None)),
            close_symbols: Arc::new(RwLock::new(Vec::new())),
            trade_settings: Arc::new(ArcSwap::from_pointee(trade_settings)),
            last_orders,
            recent_orders_queue,
            order_record_tx,
            ws_sender: Arc::new(RwLock::new(None)),
            last_trade_time_map: Arc::new(RwLock::new(HashMap::new())),
            ws_order_scale: 1.0,
            save_metrics,
            update_futures_account_data,
            run_health_check,
            run_small_orders_loop,
            print_orders,
            login_status_url_template,
            order_manager_client,
            is_monitoring,
            total_net_positions_usdt_size_all_accounts,
        }
    }

    // /// Sign message for WebSocket authentication
    // fn sign_ws_message(&self, channel: &str, request_params: &[u8], timestamp: i64) -> String {
    //     let key = format!(
    //         "api\n{}\n{}\n{}",
    //         channel,
    //         String::from_utf8_lossy(request_params),
    //         timestamp
    //     );
    //     let mut mac = Hmac::<Sha512>::new_from_slice(self.api_secret.as_bytes())
    //         .expect("HMAC can take key of any size");
    //     mac.update(key.as_bytes());
    //     hex::encode(mac.finalize().into_bytes())
    // }

    // /// Build login request for WebSocket
    // fn build_login_request(&self) -> serde_json::Value {
    //     let timestamp = SystemTime::now()
    //         .duration_since(UNIX_EPOCH)
    //         .unwrap()
    //         .as_secs() as i64;

    //     let req_id = format!(
    //         "{}-{}",
    //         SystemTime::now()
    //             .duration_since(UNIX_EPOCH)
    //             .unwrap()
    //             .as_millis(),
    //         self.login_name
    //     );

    //     let signature = self.sign_ws_message("futures.login", b"", timestamp);

    //     json!({
    //         "time": timestamp,
    //         "channel": "futures.login",
    //         "event": "api",
    //         "payload": {
    //             "api_key": self.api_key,
    //             "signature": signature,
    //             "timestamp": timestamp.to_string(),
    //             "req_id": req_id,
    //         }
    //     })
    // }

    fn extract_base_url_from_order_url(&self) -> String {
        // http://localhost:3005/puppeteer/simple-order -> http://localhost:3005
        let order_url = self
            .order_manager_client
            .as_ref()
            .unwrap()
            .order_url_for_login(&self.login_name);
        let base_url = order_url.split("/puppeteer").nth(0).unwrap_or("");
        base_url.to_string()
    }

    pub fn login_base_url(&self) -> String {
        // http://localhost:3005/puppeteer/account-status/{login_name} -> http://localhost:3005
        let base_url = self.extract_base_url_from_order_url();
        base_url
    }

    pub fn get_net_positions_usdt_size(&self) -> f64 {
        let contracts = self.contracts.load();
        let positions = self.positions.load();
        let mut net_positions_usdt_size = 0.0;
        for (_, position) in positions.iter() {
            if let Some(contract) = contracts.get(&position.contract) {
                net_positions_usdt_size += position.size as f64
                    * contract.mark_price.parse::<f64>().unwrap_or(0.0)
                    * contract.quanto_multiplier.parse::<f64>().unwrap_or(0.0);
            }
        }
        net_positions_usdt_size
    }

    pub fn get_risk_limit(&self, symbol: &str) -> f64 {
        let position = self.positions.load().get(symbol).cloned();
        position.map(|p| p.risk_limit).unwrap_or(0.0)
    }

    /// Returns immediately; actual update runs in background. try_send only — full channel drops (acceptable).
    pub fn on_new_order(&self, symbol: &str, order: LastOrder) {
        let _ = self.order_record_tx.try_send((symbol.to_string(), order));
    }

    pub fn on_new_order_ws(&self, symbol: String, order: LastOrder) {
        self.last_ws_orders.write().insert(symbol, order);
    }

    /// Spawns a thread that drains (symbol, order) and updates recent_orders_queue + last_orders. Returns sender.
    fn spawn_order_record_thread(
        recent_orders_queue: Arc<ArcSwap<VecDeque<LastOrder>>>,
        last_orders: Arc<RwLock<HashMap<String, LastOrder>>>,
    ) -> Arc<crossbeam_channel::Sender<(String, LastOrder)>> {
        let (tx, rx) = crossbeam_channel::bounded(512);
        std::thread::spawn(move || {
            for msg in rx {
                let (symbol, order): (String, LastOrder) = msg;
                let mut new_q = (**recent_orders_queue.load()).clone();
                new_q.push_back(order.clone());
                while new_q.len() > 100 {
                    new_q.pop_front();
                }
                recent_orders_queue.store(Arc::new(new_q));
                last_orders.write().insert(symbol, order);
            }
        });
        Arc::new(tx)
    }

    /// Lock-free read; at most 100 elements.
    pub fn get_oldest_order(&self) -> Option<LastOrder> {
        let q = self.recent_orders_queue.load();
        if q.is_empty() || q.len() < 100 {
            return None;
        }
        q.front().cloned()
    }

    /// Lock-free read.
    pub fn is_too_many_orders(&self, time_gap_ms: i64) -> bool {
        let q = self.recent_orders_queue.load();
        if q.is_empty() || q.len() < 100 {
            return false;
        }
        let current_ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let oldest_order = match q.front() {
            Some(o) => o,
            None => return false,
        };
        let time_gap = current_ts_ms - oldest_order.timestamp;
        time_gap < time_gap_ms
    }

    /// Lock-free read; iterates at most 100 elements.
    pub fn is_symbol_too_many_orders(
        &self,
        symbol: &str,
        symbol_order_count_threshold: i64,
        time_gap_ms: i64,
    ) -> bool {
        let q = self.recent_orders_queue.load();
        if q.is_empty() || q.len() < 100 {
            return false;
        }
        let current_ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let oldest_order = match q.front() {
            Some(o) => o,
            None => return false,
        };
        let time_gap = current_ts_ms - oldest_order.timestamp;
        let symbol_orders_count = q.iter().filter(|o| o.contract == symbol).count() as i64;
        time_gap < time_gap_ms && symbol_orders_count > symbol_order_count_threshold
    }

    /// Lock-free read.
    pub fn get_time_gap_to_oldest_order(&self) -> Option<i64> {
        let q = self.recent_orders_queue.load();
        if q.is_empty() || q.len() < 100 {
            return None;
        }
        let current_ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        q.front().map(|o| current_ts_ms - o.timestamp)
    }

    /// Start WebSocket connection for real-time updates
    pub fn start_websocket(&self) {
        // Flipster mode: if the global cookie store is initialized, use the
        // Flipster private WS feed to populate positions/open_orders instead of Gate's WS.
        if crate::flipster_cookie_store::global().is_some() {
            let cdp_port = crate::flipster_cookie_store::global()
                .map(|s| s.cdp_port())
                .unwrap_or(9230);
            let state = crate::flipster_private_ws::FlipsterPrivateWsState {
                positions: self.positions.clone(),
                open_orders: self.open_orders.clone(),
                futures_account_data: self.futures_account_data.clone(),
                is_healthy: self.is_healthy.clone(),
                login_name: self.login_name.clone(),
                cdp_port,
                shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            };
            // Spawn onto the process-wide flipster runtime (no per-thread runtime needed).
            let handle = crate::flipster_runtime::handle();
            crate::flipster_private_ws::spawn(state, handle);
            return;
        }

        let login_name = self.login_name.clone();
        let api_key = self.api_key.clone();
        let api_secret = self.api_secret.clone();
        let positions = self.positions.clone();
        let contracts = self.contracts.clone();
        let order_count_map = self.order_count_map.clone();
        let trade_count_map = self.trade_count_map.clone();
        let profitable_trade_count_map = self.profitable_trade_count_map.clone();
        let last_position_updated_time_ms = self.last_position_updated_time_ms.clone();
        let data_cache = self.data_cache.clone();
        let uid = self.uid;
        let profit_bp_ema_map = self.profit_bp_ema_map.clone();
        let trade_settings = self.trade_settings.load().as_ref().clone();
        let last_orders = self.last_orders.clone();
        let ws_sender = self.ws_sender.clone();
        let trade_callback = self.trade_callback.clone();
        let order_manager = self.clone();

        let is_healthy = Arc::clone(&self.is_healthy);

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");
            rt.block_on(async {
                Self::websocket_loop(
                    login_name,
                    api_key,
                    api_secret,
                    positions,
                    contracts,
                    order_count_map,
                    trade_count_map,
                    profitable_trade_count_map,
                    profit_bp_ema_map,
                    last_position_updated_time_ms,
                    data_cache,
                    uid,
                    trade_settings,
                    last_orders,
                    is_healthy,
                    ws_sender,
                    trade_callback,
                    order_manager,
                )
                .await;
            });
        });
    }

    async fn websocket_loop(
        login_name: String,
        api_key: String,
        api_secret: String,
        positions: Arc<ArcSwap<HashMap<String, GatePosition>>>,
        contracts: Arc<ArcSwap<HashMap<String, GateContract>>>,
        order_count_map: Arc<RwLock<HashMap<String, i64>>>,
        trade_count_map: Arc<ArcSwap<HashMap<String, i64>>>,
        profitable_trade_count_map: Arc<ArcSwap<HashMap<String, i64>>>,
        profit_bp_ema_map: Arc<RwLock<HashMap<String, f64>>>,
        last_position_updated_time_ms: Arc<RwLock<i64>>,
        data_cache: Option<Arc<crate::data_cache::DataCache>>,
        _uid: i32,
        trade_settings: TradeSettings,
        _last_orders: Arc<RwLock<HashMap<String, LastOrder>>>,
        is_healthy: Arc<RwLock<bool>>,
        ws_sender: Arc<RwLock<Option<Arc<WsWriterHandle>>>>,
        trade_callback: Arc<RwLock<Option<Box<dyn Fn(&str, i64, f64, f64, f64) + Send + Sync>>>>,
        order_manager: GateOrderManager,
    ) {
        let mut reconnect_delay = 1u64; // Start with 5 seconds
        let max_reconnect_delay = 10u64; // Max 60 seconds
        let mut reconnect_count = 0u64;
        loop {
            reconnect_count += 1;
            info!(
                "[GateOrderManager {}] Attempting WebSocket connection (attempt #{})...",
                login_name, reconnect_count
            );

            let mut ws_config = WebSocketConfig::default();
            ws_config.write_buffer_size = 1024 * 1024;
            ws_config.max_write_buffer_size = 1024 * 1024 * 10;
            match connect_async_with_config(GATE_WS_URL, Some(ws_config), true).await {
                Ok((ws_stream, _)) => {
                    // Reset reconnect delay and count on successful connection
                    reconnect_delay = 1;
                    reconnect_count = 0;
                    info!(
                        "[GateOrderManager {}] WebSocket connected successfully",
                        login_name
                    );

                    let queue = Arc::new(ArrayQueue::new(WS_QUEUE_CAP));
                    let notify = Arc::new(Notify::new());
                    let shutdown = Arc::new(AtomicBool::new(false));
                    let handle = Arc::new(WsWriterHandle {
                        queue: queue.clone(),
                        notify: notify.clone(),
                        shutdown: shutdown.clone(),
                    });
                    for _ in 0..10 {
                        if let Some(mut sender_lock) = ws_sender.try_write() {
                            *sender_lock = Some(handle.clone());
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }

                    // Python: on_open 콜백에서 로그인을 보내므로, 연결 후 약간 대기
                    tokio::time::sleep(Duration::from_millis(100)).await;

                    let (mut write, mut read) = ws_stream.split();

                    // Build and send login request (Python _build_login_request와 동일)
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64;

                    let req_id = format!(
                        "{}-{}",
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis(),
                        login_name
                    );

                    // Python get_signature와 동일: api\nchannel\nrequest_params\ntimestamp
                    let request_params = b""; // Python: request_params = b""
                    let key = format!(
                        "api\nfutures.login\n{}\n{}",
                        String::from_utf8_lossy(request_params),
                        timestamp
                    );
                    let mut mac = Hmac::<Sha512>::new_from_slice(api_secret.as_bytes())
                        .expect("HMAC can take key of any size");
                    mac.update(key.as_bytes());
                    let signature = hex::encode(mac.finalize().into_bytes());

                    let login_req = json!({
                        "time": timestamp,
                        "channel": "futures.login",
                        "event": "api",
                        "payload": {
                            "api_key": api_key,
                            "signature": signature,
                            "timestamp": timestamp.to_string(),
                            "req_id": req_id,
                        }
                    });

                    if let Err(e) = write
                        .send(Message::Text(login_req.to_string().into()))
                        .await
                    {
                        warn!(
                            "[GateOrderManager {}] Failed to send login (will reconnect): {}",
                            login_name, e
                        );
                        // Close the connection properly before reconnecting
                        let _ = write.close().await;
                        break;
                    }

                    let handle_for_reader = handle.clone();
                    let (writer_done_tx, mut writer_done_rx) = oneshot::channel::<()>();
                    let writer_login_name = login_name.to_string();
                    let writer_handle = handle.clone();

                    // Single writer task: drain lock-free queue, then wait for notify.
                    tokio::spawn(async move {
                        let mut write = write;
                        loop {
                            if writer_handle.shutdown.load(Ordering::SeqCst) {
                                break;
                            }
                            let mut sent = false;
                            while let Some(msg) = writer_handle.queue.pop() {
                                sent = true;
                                if let Err(e) = write.send(msg).await {
                                    warn!(
                                        "[GateOrderManager {}] Writer: send failed (will reconnect): {}",
                                        writer_login_name, e
                                    );
                                    let _ = writer_done_tx.send(());
                                    return;
                                }
                            }
                            if !sent {
                                writer_handle.notify.notified().await;
                            }
                        }
                    });

                    // Wait for login response before subscribing (important for Gate.io)
                    let mut login_successful = false;
                    let mut subscription_sent = false;

                    // Subscribe to channels (send after login response or timeout)
                    // Auth block per Gate WS spec: SIGN = hmac_sha512(secret, format!("channel={}&event={}&time={}", channel, event, ts))
                    let build_auth = |channel: &str, event: &str, ts: i64| -> serde_json::Value {
                        let message = format!("channel={}&event={}&time={}", channel, event, ts);
                        let mut mac = Hmac::<Sha512>::new_from_slice(api_secret.as_bytes())
                            .expect("HMAC can take key of any size");
                        mac.update(message.as_bytes());
                        let sign = hex::encode(mac.finalize().into_bytes());
                        json!({
                            "method": "api_key",
                            "KEY": api_key,
                            "SIGN": sign,
                        })
                    };

                    let subscribe_positions = json!({
                        "time": timestamp,
                        "channel": "futures.positions",
                        "event": "subscribe",
                        "payload": ["!all"],
                        "auth": build_auth("futures.positions", "subscribe", timestamp),
                    });

                    let subscribe_trades = json!({
                        "time": timestamp,
                        "channel": "futures.usertrades",
                        "event": "subscribe",
                        "payload": ["!all"],
                        "auth": build_auth("futures.usertrades", "subscribe", timestamp),
                    });

                    let subscribe_orders = json!({
                        "time": timestamp,
                        "channel": "futures.orders",
                        "event": "subscribe",
                        "payload": ["!all"],
                        "auth": build_auth("futures.orders", "subscribe", timestamp),
                    });

                    // Process messages with timeout for login response
                    let mut login_timeout_passed = false;

                    // Send periodic ping to keep connection alive (same as Python ping_interval=5)
                    let mut ping_interval = tokio::time::interval(Duration::from_secs(5));
                    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                    loop {
                        tokio::select! {
                            biased;
                            // Writer task signalled error (feed/flush failed)
                            _ = &mut writer_done_rx => {
                                break;
                            }
                            // Send ping every 5 seconds (same as Python ping_interval=5); goes via Writer
                            _ = ping_interval.tick() => {
                                if !handle_for_reader.push_message(Message::Ping(Vec::new().into())) {
                                    break;
                                }
                                if !login_successful && !login_timeout_passed {
                                    login_timeout_passed = true;
                                }
                            }
                            // Process incoming messages (Reader never blocks on send)
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Text(text))) => {
                                        trace!(
                                            "[GateOrderManager {}] WebSocket message: {:?}",
                                            login_name, text
                                        );
                                        // Check if this is a login response
                                        if !login_successful {
                                            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                                                // Gate.io login response can be in two formats:
                                                // 1. Direct format: {"channel": "futures.login", "result": {...}}
                                                // 2. Header format: {"header": {"channel": "futures.login", "status": "200"}, "data": {"result": {...}}}

                                                // Check direct format first
                                                if let Some(channel) = data.get("channel").and_then(|v| v.as_str()) {
                                                    if channel == "futures.login" {
                                                        if data.get("result").is_some() {
                                                            // Login successful (direct format)
                                                            login_successful = true;
                                                            plog!("\x1b[32m[GateOrderManager {}] Login successful\x1b[0m", login_name);
                                                            if !subscription_sent {
                                                                handle_for_reader.push_message(Message::Text(subscribe_positions.to_string().into()));
                                                                handle_for_reader.push_message(Message::Text(subscribe_trades.to_string().into()));
                                                                handle_for_reader.push_message(Message::Text(subscribe_orders.to_string().into()));
                                                                subscription_sent = true;
                                                            }
                                                        } else if let Some(error) = data.get("error") {
                                                            elog!("\x1b[31m[GateOrderManager {}] Login failed: {}\x1b[0m", login_name, error);
                                                            break; // Break to reconnect
                                                        }
                                                    }
                                                }

                                                // Check header format
                                                if let Some(header) = data.get("header") {
                                                    if let Some(channel) = header.get("channel").and_then(|v| v.as_str()) {
                                                        if channel == "futures.login" {
                                                            if let Some(status) = header.get("status").and_then(|v| v.as_str()) {
                                                                if status == "200" {
                                                                    if let Some(data_obj) = data.get("data") {
                                                                        if data_obj.get("result").is_some() {
                                                                            // Login successful (header format)
                                                                            login_successful = true;
                                                                            plog!("\x1b[32m[GateOrderManager {}] Login successful\x1b[0m", login_name);
                                                                            if !subscription_sent {
                                                                                handle_for_reader.push_message(Message::Text(subscribe_positions.to_string().into()));
                                                                                handle_for_reader.push_message(Message::Text(subscribe_trades.to_string().into()));
                                                                                handle_for_reader.push_message(Message::Text(subscribe_orders.to_string().into()));
                                                                                subscription_sent = true;
                                                                            }
                                                                        } else {
                                                                            elog!("\x1b[31m[GateOrderManager {}] Login response missing result: {}\x1b[0m", login_name, text);
                                                                        }
                                                                    } else {
                                                                        elog!("\x1b[31m[GateOrderManager {}] Login response missing data field: {}\x1b[0m", login_name, text);
                                                                    }
                                                                } else {
                                                                    elog!("\x1b[31m[GateOrderManager {}] Login failed: status={}, response: {}\x1b[0m", login_name, status, text);
                                                                    break; // Break to reconnect
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }

                                        // Handle all messages (including login response)
                                        // Don't break on error - continue processing other messages
                                        if let Err(e) = Self::handle_ws_message(
                                            &text,
                                            &positions,
                                            &contracts,
                                            &order_count_map,
                                            &trade_count_map,
                                            &profitable_trade_count_map,
                                            &profit_bp_ema_map,
                                            &last_position_updated_time_ms,
                                            &login_name,
                                        data_cache.clone(),
                                            &trade_settings,
                                            &is_healthy,
                                            &trade_settings.normalize_trade_count,
                                            &trade_callback,
                                            &order_manager,
                                        ) {
                                            error!("[GateOrderManager {}] Error handling message: {} (continuing...)", login_name, e);
                                            // Continue processing - don't break on message handling errors
                                        }
                                    }
                                    Some(Ok(Message::Ping(payload))) => {
                                        if !handle_for_reader.push_message(Message::Pong(payload)) {
                                            break;
                                        }
                                    }
                                    Some(Ok(Message::Pong(_))) => {
                                        // Received pong - connection is alive
                                        // No action needed
                                    }
                                    Some(Ok(Message::Close(frame))) => {
                                        if let Some(close_frame) = frame {
                                            warn!(
                                                "[GateOrderManager {}] WebSocket closed by server (code: {:?}, reason: {:?}) - will reconnect",
                                                login_name,
                                                close_frame.code,
                                                close_frame.reason
                                            );
                                        } else {
                                            warn!(
                                                "[GateOrderManager {}] WebSocket closed by server (no close frame) - will reconnect",
                                                login_name
                                            );
                                        }
                                        break;
                                    }
                                    Some(Err(e)) => {
                                        // Check if it's a connection reset error (common and recoverable)
                                        let error_str = format!("{}", e);
                                        if error_str.contains("connection reset")
                                            || error_str.contains("Connection reset")
                                        {
                                            warn!(
                                                "[GateOrderManager {}] WebSocket connection reset (will reconnect): {}",
                                                login_name, e
                                            );
                                        } else if error_str.contains("Broken pipe")
                                            || error_str.contains("broken pipe")
                                        {
                                            warn!(
                                                "[GateOrderManager {}] WebSocket broken pipe (will reconnect): {}",
                                                login_name, e
                                            );
                                        } else {
                                            error!(
                                                "[GateOrderManager {}] WebSocket error (will reconnect): {}",
                                                login_name, e
                                            );
                                        }
                                        break;
                                    }
                                    Some(Ok(_)) => {} // Ignore other message types
                                    None => {
                                        // Stream ended - connection closed
                                        warn!(
                                            "[GateOrderManager {}] WebSocket stream ended - will reconnect",
                                            login_name
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    handle_for_reader.shutdown.store(true, Ordering::SeqCst);
                    handle_for_reader.notify.notify_one();
                }
                Err(e) => {
                    error!(
                        "[GateOrderManager {}] Failed to connect WebSocket: {}",
                        login_name, e
                    );
                }
            }

            // Reconnect with exponential backoff
            info!(
                "[GateOrderManager {}] WebSocket disconnected. Reconnecting in {} seconds... (attempt #{})",
                login_name, reconnect_delay, reconnect_count
            );
            tokio::time::sleep(Duration::from_secs(reconnect_delay)).await;
            reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
        }
    }

    fn update_metrics_from_file_static(
        login_name: &str,
        order_count_map: &Arc<RwLock<HashMap<String, i64>>>,
        trade_count_map: &Arc<ArcSwap<HashMap<String, i64>>>,
        profitable_trade_count_map: &Arc<ArcSwap<HashMap<String, i64>>>,
        profit_bp_ema_map: &Arc<RwLock<HashMap<String, f64>>>,
    ) -> Result<(), std::io::Error> {
        let metrics_data = Self::load_metrics_from_file_static(login_name)?;

        let order_counts = metrics_data
            .get("order_counts")
            .and_then(|v| v.as_object())
            .unwrap_or(&serde_json::Map::new())
            .clone();
        let trade_counts = metrics_data
            .get("trade_counts")
            .and_then(|v| v.as_object())
            .unwrap_or(&serde_json::Map::new())
            .clone();
        let profitable_trade_counts = metrics_data
            .get("profitable_trade_counts")
            .and_then(|v| v.as_object())
            .unwrap_or(&serde_json::Map::new())
            .clone();
        let profit_bp_emas = metrics_data
            .get("profit_bp_emas")
            .and_then(|v| v.as_object())
            .unwrap_or(&serde_json::Map::new())
            .clone();

        for (symbol, count) in order_counts {
            order_count_map
                .write()
                .insert(symbol.to_string(), count.as_i64().unwrap_or(0));
        }
        {
            let mut map = (**trade_count_map.load()).clone();
            for (symbol, count) in trade_counts {
                map.insert(symbol.to_string(), count.as_i64().unwrap_or(0));
            }
            trade_count_map.store(Arc::new(map));
        }
        {
            let mut map = (**profitable_trade_count_map.load()).clone();
            for (symbol, count) in profitable_trade_counts {
                map.insert(symbol.to_string(), count.as_i64().unwrap_or(0));
            }
            profitable_trade_count_map.store(Arc::new(map));
        }
        for (symbol, ema) in profit_bp_emas {
            profit_bp_ema_map
                .write()
                .insert(symbol.to_string(), ema.as_f64().unwrap_or(0.0));
        }
        Ok(())
    }

    fn handle_ws_message(
        text: &str,
        positions: &Arc<ArcSwap<HashMap<String, GatePosition>>>,
        contracts: &Arc<ArcSwap<HashMap<String, GateContract>>>,
        _order_count_map: &Arc<RwLock<HashMap<String, i64>>>,
        trade_count_map: &Arc<ArcSwap<HashMap<String, i64>>>,
        profitable_trade_count_map: &Arc<ArcSwap<HashMap<String, i64>>>,
        profit_bp_ema_map: &Arc<RwLock<HashMap<String, f64>>>,
        last_position_updated_time_ms: &Arc<RwLock<i64>>,
        login_name: &str,
        data_cache: Option<Arc<crate::data_cache::DataCache>>,
        trade_settings: &TradeSettings,
        is_healthy: &Arc<RwLock<bool>>,
        normalize_trade_count: &i64,
        trade_callback: &Arc<RwLock<Option<Box<dyn Fn(&str, i64, f64, f64, f64) + Send + Sync>>>>,
        order_manager: &GateOrderManager,
    ) -> Result<()> {
        let data: serde_json::Value = serde_json::from_str(text)?;

        // Check header status (same as Python)
        if let Some(header) = data.get("header") {
            if let Some(status) = header.get("status").and_then(|v| v.as_str()) {
                if status != "200" {
                    if let Some(data_obj) = data.get("data") {
                        if let Some(errs) = data_obj.get("errs") {
                            if let Some(label) = errs.get("label").and_then(|v| v.as_str()) {
                                if label != "ORDER_FOK" {
                                    elog!("[GateOrderManager {}] WebSocket error: status={}, label={}, data={}", login_name, status, label, data_obj);
                                }
                            }
                        }
                    }
                }
            }
            // Store rate limit info from header (x_gate_ratelimit_requests_remain, x_gate_ratelimit_limit, x_gate_ratelimit_reset_timestamp / x_gat_ratelimit_reset_timestamp)
            let mut current = (**order_manager.gate_rate_limit.load()).clone();
            let mut updated = false;
            if let Some(v) = header.get("x_gate_ratelimit_requests_remain") {
                if let Some(n) = v
                    .as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                {
                    current.requests_remain = Some(n);
                    updated = true;
                }
            }
            if let Some(v) = header.get("x_gate_ratelimit_limit") {
                if let Some(n) = v
                    .as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                {
                    current.limit = Some(n);
                    updated = true;
                }
            }
            let reset_ts = header
                .get("x_gate_ratelimit_reset_timestamp")
                .or_else(|| header.get("x_gat_ratelimit_reset_timestamp"));
            if let Some(v) = reset_ts {
                if let Some(n) = v
                    .as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                {
                    current.reset_timestamp = Some(n);
                    updated = true;
                }
            }
            if updated {
                order_manager.gate_rate_limit.store(Arc::new(current));
            }
        }

        let event = if let Some(event) = data.get("event").and_then(|v| v.as_str()) {
            Some(event)
        } else if let Some(header) = data.get("header").and_then(|v| v.as_object()) {
            if let Some(event) = header.get("event").and_then(|v| v.as_str()) {
                Some(event)
            } else {
                None
            }
        } else {
            None
        };

        let channel = if let Some(channel) = data.get("channel").and_then(|v| v.as_str()) {
            Some(channel)
        } else if let Some(header) = data.get("header").and_then(|v| v.as_object()) {
            if let Some(channel) = header.get("channel").and_then(|v| v.as_str()) {
                Some(channel)
            } else {
                None
            }
        } else {
            None
        };

        // Log successful subscription to usertrades (helps confirm WS wiring)
        if event == Some("subscribe") && channel == Some("futures.usertrades") {
            info!(
                "[GateOrderManager {}] Subscribe futures.usertrades response: {}",
                login_name, data
            );
        }
        if event == Some("subscribe") && channel == Some("futures.positions") {
            info!(
                "[GateOrderManager {}] Subscribe futures.positions response: {}",
                login_name, data
            );
        }
        if event == Some("subscribe") && channel == Some("futures.orders") {
            info!(
                "[GateOrderManager {}] Subscribe futures.orders response: {}",
                login_name, data
            );
        }

        if event == Some("update") {
            match channel {
                Some("futures.positions") => {
                    trace!(
                        "[GateOrderManager {}] Update futures.positions: {:?}",
                        login_name,
                        data
                    );
                    if let Some(results) = data.get("result").and_then(|v| v.as_array()) {
                        let current_time_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as i64;

                        *last_position_updated_time_ms.write() = current_time_ms;

                        // Collect updates first, then apply in one write to minimize lock time
                        let mut updates = Vec::new();

                        for result in results {
                            // Gate WebSocket returns size as String, not i64

                            if let Some(contract) = result.get("contract").and_then(|v| v.as_str())
                            {
                                // Python extracts time_ms from result (same as Python line 1245)
                                let _time_ms = result
                                    .get("time_ms")
                                    .and_then(|v| v.as_i64())
                                    .or_else(|| {
                                        result
                                            .get("time_ms")
                                            .and_then(|v| v.as_str())
                                            .and_then(|s| s.parse::<i64>().ok())
                                    })
                                    .unwrap_or(current_time_ms);

                                if let Some(size) = result.get("size").and_then(|v| v.as_i64()) {
                                    // Get previous position before collecting update (lock-free load)
                                    let previous_position = positions.load().get(contract).cloned();

                                    // Python keeps position even if size is 0, but we'll remove it for consistency
                                    // However, Python doesn't explicitly remove positions with size 0
                                    // So we'll keep the position but mark size as 0

                                    // Create GatePosition from WebSocket data (same as Python)
                                    let position = GatePosition {
                                        contract: contract.to_string(),
                                        size,
                                        risk_limit: result
                                            .get("risk_limit")
                                            .and_then(|v| v.as_f64())
                                            .unwrap_or(0.0),
                                        entry_price: result
                                            .get("entry_price")
                                            .and_then(|v| v.as_f64())
                                            .unwrap_or(0.0),
                                        mark_price: result
                                            .get("mark_price")
                                            .and_then(|v| v.as_f64())
                                            .unwrap_or(0.0),
                                        realised_pnl: result
                                            .get("realised_pnl")
                                            .and_then(|v| v.as_f64())
                                            .unwrap_or(0.0),
                                        last_close_pnl: result
                                            .get("last_close_pnl")
                                            .and_then(|v| v.as_f64())
                                            .unwrap_or(0.0),
                                        history_pnl: result
                                            .get("history_pnl")
                                            .and_then(|v| v.as_f64())
                                            .unwrap_or(0.0),
                                        update_time: result
                                            .get("time")
                                            .and_then(|v| v.as_i64())
                                            .unwrap_or(0),
                                    };

                                    debug!(
                                        "[GateOrderManager {}] Position: {:?}",
                                        login_name, position
                                    );

                                    // Collect update for batch apply (with previous position for logging)
                                    let prev_size =
                                        previous_position.as_ref().map(|p| p.size).unwrap_or(0);
                                    updates.push((contract.to_string(), position, size, prev_size));
                                } else {
                                    warn!("[GateOrderManager {}] Error parsing size in futures.positions: {:?}", login_name, result);
                                }
                            }
                        }

                        // Apply all updates in one atomic store (clone-modify-store; read never blocked)
                        if !updates.is_empty() {
                            let mut pos_map = (**positions.load()).clone();
                            for (contract, position, size, prev_size) in updates {
                                // Print position change (same as Python print_position)
                                if prev_size != size {
                                    crate::metrics::print_position(
                                        &contract,
                                        prev_size,
                                        size,
                                        position.realised_pnl,
                                        position.last_close_pnl,
                                        position.history_pnl,
                                        login_name,
                                        order_manager,
                                    );
                                }

                                // Python always inserts position (even if size is 0)
                                // But we'll remove it if size is 0 to save memory
                                if size != 0 {
                                    pos_map.insert(contract.clone(), position);
                                } else {
                                    pos_map.remove(&contract);
                                }
                            }
                            positions.store(Arc::new(pos_map));
                        } else {
                            warn!(
                                "[GateOrderManager {}] No updates in futures.positions: {:?}",
                                login_name, data
                            );
                        }
                    }
                }
                Some("futures.usertrades") => {
                    if let Some(results) = data.get("result").and_then(|v| v.as_array()) {
                        for result in results {
                            if let Some(contract) = result.get("contract").and_then(|v| v.as_str())
                            {
                                let trade_time_ms = result
                                    .get("create_time_ms")
                                    .and_then(|v| v.as_i64())
                                    .or_else(|| {
                                        result
                                            .get("create_time_ms")
                                            .and_then(|v| v.as_str())
                                            .and_then(|s| s.parse::<i64>().ok())
                                    })
                                    .unwrap_or_else(|| {
                                        SystemTime::now()
                                            .duration_since(UNIX_EPOCH)
                                            .unwrap()
                                            .as_millis()
                                            as i64
                                    });
                                {
                                    let mut map = order_manager.last_trade_time_map.write();
                                    map.insert(contract.to_string(), trade_time_ms);
                                }

                                let text =
                                    result.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                if text.contains("api") || text.contains("ws") {
                                    continue;
                                }
                                // Size can be either number or string
                                let size: i64 = if let Some(size_val) = result.get("size") {
                                    if let Some(size_num) = size_val.as_i64() {
                                        size_num
                                    } else if let Some(size_str) = size_val.as_str() {
                                        size_str.parse().unwrap_or(0)
                                    } else if let Some(size_f64) = size_val.as_f64() {
                                        size_f64 as i64
                                    } else {
                                        0
                                    }
                                } else {
                                    0
                                };

                                // Price can be either number or string
                                let price: f64 = if let Some(price_val) = result.get("price") {
                                    if let Some(price_num) = price_val.as_f64() {
                                        price_num
                                    } else if let Some(price_str) = price_val.as_str() {
                                        price_str.parse().unwrap_or(0.0)
                                    } else if let Some(price_i64) = price_val.as_i64() {
                                        price_i64 as f64
                                    } else {
                                        0.0
                                    }
                                } else {
                                    0.0
                                };

                                let fee: f64 =
                                    result.get("fee").and_then(|v| v.as_f64()).unwrap_or(0.0);

                                if size.abs() > 0 {
                                    {
                                        let mut map = (**trade_count_map.load()).clone();
                                        *map.entry(contract.to_string()).or_insert(0) += 1;
                                        trade_count_map.store(Arc::new(map));
                                    }

                                    // Get create_time_ms for book ticker wait logic (same as Python)
                                    let create_time_ms = trade_time_ms;

                                    // Get last book ticker from DataCache (wait up to 100ms for a fresh one, like Python)
                                    let wait_after_trade_ms: i64 =
                                        std::env::var(WAIT_FOR_BOOK_TICKER_AFTER_TRADE_ENV)
                                            .ok()
                                            .and_then(|v| v.parse::<i64>().ok())
                                            .unwrap_or(WAIT_FOR_BOOK_TICKER_AFTER_TRADE_MS);
                                    let (last_bid, last_ask) = if let Some(cache) =
                                        data_cache.as_deref()
                                    {
                                        let deadline = SystemTime::now()
                                            .duration_since(UNIX_EPOCH)
                                            .unwrap_or_else(|_| Duration::from_millis(0))
                                            .as_millis()
                                            as i64
                                            + wait_after_trade_ms;
                                        loop {
                                            let bookticker = cache.get_gate_bookticker(contract);
                                            if let Some(ref bt) = bookticker {
                                                if bt.server_time >= create_time_ms {
                                                    break (Some(bt.bid_price), Some(bt.ask_price));
                                                }
                                            }
                                            let now_ms = SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap_or_else(|_| Duration::from_millis(0))
                                                .as_millis()
                                                as i64;
                                            if now_ms >= deadline {
                                                break bookticker
                                                    .map(|bt| {
                                                        (Some(bt.bid_price), Some(bt.ask_price))
                                                    })
                                                    .unwrap_or((None, None));
                                            }
                                            std::thread::sleep(Duration::from_millis(1));
                                        }
                                    } else {
                                        (None, None)
                                    };
                                    let (last_bid, last_ask) =
                                        if let Some(cache) = data_cache.as_deref() {
                                            let bookticker = cache.get_gate_bookticker(contract);
                                            if let Some(ref bt) = bookticker {
                                                (Some(bt.bid_price), Some(bt.ask_price))
                                            } else {
                                                (None, None)
                                            }
                                        } else {
                                            (None, None)
                                        };

                                    // Get contract for print_trade — lock-free (contracts)
                                    let contract_info = contracts.load().get(contract).cloned();

                                    // Get success rate and profitable rate (same as Python) — lock-free
                                    let success_rate = {
                                        let order_map = _order_count_map.read();
                                        let trade_count = trade_count_map
                                            .load()
                                            .get(contract)
                                            .copied()
                                            .unwrap_or(0);
                                        let order_count =
                                            order_map.get(contract).copied().unwrap_or(0);
                                        if order_count > 0 {
                                            trade_count as f64 / order_count as f64
                                        } else {
                                            0.0
                                        }
                                    };

                                    let profitable_rate = {
                                        let profitable_count = profitable_trade_count_map
                                            .load()
                                            .get(contract)
                                            .copied()
                                            .unwrap_or(0);
                                        let trade_count = trade_count_map
                                            .load()
                                            .get(contract)
                                            .copied()
                                            .unwrap_or(0);
                                        if trade_count > 0 {
                                            profitable_count as f64 / trade_count as f64
                                        } else {
                                            0.0
                                        }
                                    };

                                    let usdt_size = order_manager
                                        .get_usdt_amount_from_size(contract, size.abs() as i64);
                                    let fee_bp = fee.abs() / usdt_size * 10000.0;

                                    // Check if profitable (same as Python)
                                    if let (Some(bid), Some(ask)) = (last_bid, last_ask) {
                                        let instant_profit_bp = if size > 0 {
                                            (bid - price) / price * 10000.0
                                        } else {
                                            (price - ask) / price * 10000.0
                                        };

                                        let order_side_profit_bp = if size > 0 {
                                            (ask - price) / price * 10000.0
                                        } else {
                                            (price - bid) / price * 10000.0
                                        };

                                        let mid_profit_bp =
                                            (instant_profit_bp + order_side_profit_bp) * 0.5;

                                        if mid_profit_bp
                                            > order_manager
                                                .trade_settings
                                                .load()
                                                .close_raw_mid_profit_bp
                                            && fee_bp > 2.0
                                        {
                                            info!(
                                                "[GateOrderManager {}] Profitable trade detected for {} (mid_profit_bp={:.4})",
                                                login_name, contract, mid_profit_bp
                                            );
                                            {
                                                let mut map =
                                                    (**profitable_trade_count_map.load()).clone();
                                                *map.entry(contract.to_string()).or_insert(0) += 1;
                                                profitable_trade_count_map.store(Arc::new(map));
                                            }
                                            // Call repeat_profitable_order in background so WS message handler returns quickly
                                            if order_manager
                                                .trade_settings
                                                .load()
                                                .repeat_profitable_order
                                            {
                                                if let Some(cache) = data_cache.clone() {
                                                    let manager = order_manager.clone();
                                                    let login_name = login_name.to_string();
                                                    let contract = contract.to_string();
                                                    std::thread::spawn(move || {
                                                        if let Err(err) =
                                                            Self::repeat_profitable_order(
                                                                &manager,
                                                                &login_name,
                                                                &contract,
                                                                size,
                                                                price,
                                                                cache.as_ref(),
                                                            )
                                                        {
                                                            warn!(
                                                                "[GateOrderManager {}] Failed to repeat profitable order for {}: {}",
                                                                login_name, contract, err
                                                            );
                                                        }
                                                    });
                                                } else {
                                                    warn!(
                                                        "[GateOrderManager {}] No data cache for repeat profitable order for {}",
                                                        login_name, contract
                                                    );
                                                }
                                            }
                                        }

                                        let mut profit_bp_ema_map = profit_bp_ema_map.write();
                                        *profit_bp_ema_map
                                            .entry(contract.to_string())
                                            .or_insert(0.0) = profit_bp_ema_map
                                            .get(&contract.to_string())
                                            .copied()
                                            .unwrap_or_else(|| mid_profit_bp.clamp(0.0, 2.0))
                                            * (1.0 - trade_settings.profit_bp_ema_alpha)
                                            + mid_profit_bp * trade_settings.profit_bp_ema_alpha;
                                        drop(profit_bp_ema_map);
                                    }
                                    let profit_bp_ema = profit_bp_ema_map
                                        .read()
                                        .get(&contract.to_string())
                                        .copied()
                                        .unwrap_or(0.0);

                                    let profit_count = profitable_trade_count_map
                                        .load()
                                        .get(contract)
                                        .copied()
                                        .unwrap_or(0);

                                    let trade_count =
                                        trade_count_map.load().get(contract).copied().unwrap_or(0);

                                    // Normalize trade count and success count, TODO: make it configurable
                                    if trade_count > *normalize_trade_count {
                                        let success_rate = profit_count as f64 / trade_count as f64;
                                        let normalized_success_count =
                                            (success_rate * *normalize_trade_count as f64) as i64;

                                        {
                                            let mut map =
                                                (**profitable_trade_count_map.load()).clone();
                                            *map.entry(contract.to_string()).or_insert(1) =
                                                normalized_success_count.max(1);
                                            profitable_trade_count_map.store(Arc::new(map));
                                        }
                                        {
                                            let mut map = (**trade_count_map.load()).clone();
                                            *map.entry(contract.to_string())
                                                .or_insert_with(|| *normalize_trade_count) =
                                                *normalize_trade_count;
                                            trade_count_map.store(Arc::new(map));
                                        }
                                    }

                                    // Print trade (same as Python print_trade)
                                    crate::metrics::print_trade(
                                        contract,
                                        size,
                                        price,
                                        fee,
                                        last_bid,
                                        last_ask,
                                        login_name,
                                        contract_info.as_ref(),
                                        success_rate,
                                        profitable_rate,
                                        profit_bp_ema,
                                        create_time_ms,
                                    );
                                }
                            }
                        }
                    }
                }
                Some("futures.orders") => {
                    if let Some(results) = data.get("result").and_then(|v| v.as_array()) {
                        let open_orders = &order_manager.open_orders;
                        let mut orders_map = (**open_orders.load()).clone();

                        for result in results {
                            let is_healthy_value = *is_healthy.read();
                            if !is_healthy_value {
                                *is_healthy.write() = true;
                                info!(
                                    "\x1b[92m[GateOrderManager] 🟢 Account is healthy: {}\x1b[0m",
                                    login_name
                                );
                            }

                            let id = result
                                .get("id")
                                .and_then(|v| v.as_i64())
                                .or_else(|| {
                                    result
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .and_then(|s| s.parse().ok())
                                })
                                .map(|n| n.to_string());
                            let status = result
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();

                            if let Some(id_str) = id {
                                if status == "open" {
                                    let contract = result
                                        .get("contract")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let create_time_ms = result
                                        .get("create_time_ms")
                                        .and_then(|v| v.as_i64())
                                        .or_else(|| {
                                            result
                                                .get("create_time_ms")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| s.parse().ok())
                                        })
                                        .unwrap_or(0);
                                    let price = result
                                        .get("price")
                                        .and_then(|v| v.as_f64())
                                        .or_else(|| {
                                            result
                                                .get("price")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| s.parse().ok())
                                        })
                                        .unwrap_or(0.0);
                                    let size = result
                                        .get("size")
                                        .and_then(|v| v.as_i64())
                                        .or_else(|| {
                                            result
                                                .get("size")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| s.parse().ok())
                                        })
                                        .unwrap_or(0);
                                    let left = result
                                        .get("left")
                                        .and_then(|v| v.as_i64())
                                        .or_else(|| {
                                            result
                                                .get("left")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| s.parse().ok())
                                        })
                                        .unwrap_or(0);
                                    let is_reduce_only = result
                                        .get("is_reduce_only")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);
                                    let is_close = result
                                        .get("is_close")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);
                                    let tif = result
                                        .get("tif")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let text = result
                                        .get("text")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let update_time = result
                                        .get("update_time")
                                        .and_then(|v| v.as_i64())
                                        .or_else(|| {
                                            result
                                                .get("update_time")
                                                .and_then(|v| v.as_str())
                                                .and_then(|s| s.parse().ok())
                                        })
                                        .unwrap_or(0);
                                    let amend_text = result
                                        .get("amend_text")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();

                                    let order = GateOpenOrder {
                                        id: id_str.clone(),
                                        contract,
                                        create_time_ms,
                                        price,
                                        size,
                                        status,
                                        left,
                                        is_reduce_only,
                                        is_close,
                                        tif,
                                        text,
                                        update_time,
                                        amend_text,
                                    };
                                    if order_manager.print_orders {
                                        crate::metrics::print_order(
                                            &order,
                                            login_name,
                                            &order_manager,
                                        );
                                    }
                                    orders_map.insert(id_str, order);
                                } else {
                                    orders_map.remove(&id_str);
                                }
                            }
                        }

                        open_orders.store(Arc::new(orders_map));
                    }
                }

                _ => {}
            }
        }

        if channel == Some("futures.order_amend") {
            if let Some(result) = data.get("data").and_then(|d| d.get("result")) {
                let id = result
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .or_else(|| {
                        result
                            .get("id")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse().ok())
                    })
                    .map(|n| n.to_string());
                let status = result
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let finish_as = result
                    .get("finish_as")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if let Some(id_str) = id {
                    let open_orders = &order_manager.open_orders;
                    let mut orders_map = (**open_orders.load()).clone();
                    if status == "open" {
                        let id_str_for_amend = id_str.clone();
                        let contract = result
                            .get("contract")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let gate_contract = order_manager.get_contract(contract.as_str());
                        let create_time = result
                            .get("create_time")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        let create_time_ms = result
                            .get("create_time_ms")
                            .and_then(|v| v.as_i64())
                            .or_else(|| {
                                result
                                    .get("create_time_ms")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| s.parse().ok())
                            })
                            .unwrap_or((create_time * 1000.0) as i64);
                        let price = result
                            .get("price")
                            .and_then(|v| v.as_f64())
                            .or_else(|| {
                                result
                                    .get("price")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| s.parse().ok())
                            })
                            .unwrap_or(0.0);
                        let size = result
                            .get("size")
                            .and_then(|v| v.as_i64())
                            .or_else(|| {
                                result
                                    .get("size")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| s.parse().ok())
                            })
                            .unwrap_or(0);
                        let left = result
                            .get("left")
                            .and_then(|v| v.as_i64())
                            .or_else(|| {
                                result
                                    .get("left")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| s.parse().ok())
                            })
                            .unwrap_or(0);
                        let is_reduce_only = result
                            .get("is_reduce_only")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let is_close = result
                            .get("is_close")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let tif = result
                            .get("tif")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let tif_str = tif.clone();
                        let text = result
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let update_time_f = result
                            .get("update_time")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        let update_time = result
                            .get("update_time")
                            .and_then(|v| v.as_i64())
                            .or_else(|| {
                                result
                                    .get("update_time")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| s.parse().ok())
                            })
                            .unwrap_or(update_time_f as i64);
                        let amend_text = result
                            .get("amend_text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        if tif_str == "poc" {
                            return Ok(());
                        }

                        // info!("[GateOrderManager] Amended order: contract={}, price={}, size={}, left={}", contract, price, size, left);
                        if left == 0 {
                            warn!("[GateOrderManager] Invalid left size, result: {:?}", result);
                        }

                        let order = GateOpenOrder {
                            id: id_str.clone(),
                            contract,
                            create_time_ms,
                            price,
                            size,
                            status,
                            left,
                            is_reduce_only,
                            is_close,
                            tif,
                            text,
                            update_time,
                            amend_text,
                        };
                        orders_map.insert(id_str, order);

                        if let Some(gate_contract) = gate_contract {
                            // if left size if larger than min_size * 2, change price to far away from bookticker

                            let min_size = (gate_contract.order_size_min as i64).max(1);

                            if left.abs() > min_size * 2 {
                                info!("[GateOrderManager] Amending order: contract={}, price={}, size={}, left={}, min_size={}", gate_contract.name, price, size, left, gate_contract.order_size_min);
                                let mark_price =
                                    gate_contract.mark_price.parse::<f64>().unwrap_or(0.0);
                                let (new_size, new_price, side) = if size > 0 {
                                    let new_price = (price * 0.7).max(mark_price * 0.5);

                                    (min_size, new_price, "buy")
                                } else {
                                    let new_price = (price * 1.3).min(mark_price * 1.3);
                                    (-min_size, new_price, "sell")
                                };

                                if let Err(e) = order_manager.amend_websocket_order(
                                    id_str_for_amend.as_str(),
                                    new_price,
                                    Some(new_size),
                                    orders_map
                                        .get(&id_str_for_amend)
                                        .map(|o| o.contract.as_str()),
                                    None,
                                    Some(side),
                                    orders_map.get(&id_str_for_amend).cloned(),
                                    None,
                                ) {
                                    error!("[GateOrderManager] Failed to amend order: {}", e);
                                }
                            }
                        }
                    } else if finish_as == "filled" {
                        orders_map.remove(&id_str);

                        let contract = result
                            .get("contract")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let gate_contract = order_manager.get_contract(contract.as_str());

                        if let Some(gate_contract) = gate_contract {
                            let price = gate_contract.mark_price.parse::<f64>().unwrap_or(0.0);

                            let size = result.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
                            let contract = result
                                .get("contract")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let gate_contract = order_manager.get_contract(contract.as_str());

                            if let Some(gate_contract) = gate_contract {
                                let min_order_size = (gate_contract.order_size_min as i64).max(1);
                                let (new_size, new_price, side) = if size > 0 {
                                    let new_price = order_manager.get_valid_order_price(
                                        contract.as_str(),
                                        price * 0.5,
                                        Some("buy"),
                                    );
                                    (min_order_size as i64, new_price, "buy")
                                } else {
                                    let new_price = order_manager.get_valid_order_price(
                                        contract.as_str(),
                                        price * 1.5,
                                        Some("sell"),
                                    );
                                    (-(min_order_size as i64), new_price, "sell")
                                };

                                let usdt_size = order_manager
                                    .get_usdt_amount_from_size(contract.as_str(), new_size)
                                    .abs();

                                let order_request = crate::order_manager_client::OrderRequest {
                                    login_name: order_manager.login_name.clone(),
                                    symbol: contract.clone(),
                                    side: side.to_string(),
                                    price: new_price,
                                    size: new_size,
                                    tif: "gtc".to_string(),
                                    level: "limit_open".to_string(),
                                    if_not_healthy_ws: false,
                                    only_reduce_only_for_ws: false,
                                    bypass_safe_limit_close: true,
                                    uid: order_manager.get_uid(),
                                    usdt_size,
                                };

                                info!(
                                "[GateOrderManager {}] Placing follow-up order after fill ({} {}): {}",
                                order_manager.login_name, contract, side, new_size
                            );

                                // place_order uses OrderManagerClient which may block_on; must run off the tokio runtime to avoid "Cannot start a runtime from within a runtime"
                                let manager = order_manager.clone();
                                let contract = contract.clone();
                                let side = side.to_string();
                                let client = order_manager
                                    .order_manager_client
                                    .clone()
                                    .unwrap_or_else(|| {
                                        crate::order_manager_client::OrderManagerClient::instance()
                                    });
                                std::thread::spawn(move || {
                                    if let Err(e) = manager.place_order(&client, order_request) {
                                        error!(
                                        "[GateOrderManager {}] Failed to place follow-up order after fill ({} {}): {}",
                                        manager.login_name, contract, side, e
                                    );
                                    }
                                });
                            }
                        }
                    } else {
                        orders_map.remove(&id_str);
                        info!("[GateOrderManager] Cancelled order: {:?}", result);
                    }
                    open_orders.store(Arc::new(orders_map));
                }
            }
        }
        Ok(())
    }

    pub fn is_ws_rate_limit_dangerous(&self) -> bool {
        let gate_rate_limit = self.gate_rate_limit.load();
        let requests_remain = gate_rate_limit.requests_remain.unwrap_or(0);
        let limit = gate_rate_limit.limit.unwrap_or(0);

        requests_remain <= limit / 5 * 3
    }

    pub fn start_small_orders_loop(&self) -> Result<(), String> {
        let self_clone = self.clone();

        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(10));

            let max_position_size = self_clone.trade_settings.load().max_position_size;
            let account_symbols = self_clone.symbols.read().clone();
            let position_symbols = self_clone
                .positions
                .load()
                .iter()
                .filter(|(_, p)| p.size != 0)
                .map(|(s, _)| s.clone())
                .collect::<Vec<String>>();

            // all symbols = account_symbols + position_symbols
            let mut all_symbols = account_symbols
                .iter()
                .chain(position_symbols.iter())
                .cloned()
                .collect::<Vec<String>>();

            let open_orders = self_clone.open_orders.load().clone();

            for (order_id, order) in open_orders.iter() {
                let contract = order.contract.clone();
                // if contract not in all_symbols, cancel order

                let left_usdt_size = self_clone
                    .get_usdt_amount_from_size(contract.as_str(), order.left)
                    .abs();
                if left_usdt_size.abs() > 200.0 {
                    // cancel order
                    if let Err(e) = self_clone.cancel_order(order_id) {
                        error!("[GateOrderManager] Failed to cancel order: {}", e);
                    }
                    continue;
                }

                if !all_symbols.contains(&contract) {
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64;
                    if now_ms - order.create_time_ms < 1000 * 60 * 10 {
                        continue;
                    }
                    if let Err(e) = self_clone.cancel_order(order_id) {
                        error!("[GateOrderManager] Failed to cancel order: {}", e);
                    }
                }
            }

            all_symbols.shuffle(&mut rand::rng());

            for symbol in all_symbols.iter() {
                // if self_clone.is_limit_open_blocked(symbol) {
                //     continue;
                // }

                let symbol_open_orders = self_clone.get_open_orders_for_contract(symbol).clone();

                let has_long_order = symbol_open_orders.iter().any(|o| o.size > 0);
                let has_short_order = symbol_open_orders.iter().any(|o| o.size < 0);

                if has_long_order && has_short_order {
                    continue;
                }
                let has_long_position = self_clone.get_position_size(symbol) > 0;
                let has_short_position = self_clone.get_position_size(symbol) < 0;

                let total_net_positions_usdt_size = self_clone
                    .total_net_positions_usdt_size_all_accounts
                    .as_ref()
                    .map(|slot| **slot.load())
                    .unwrap_or(0.0);

                let open_order_available = max_position_size > 0.0;
                let long_order_available = max_position_size > 0.0
                    && (!self_clone.is_limit_open_blocked(symbol)
                        || total_net_positions_usdt_size < 0.0);
                let short_order_available = max_position_size > 0.0
                    && (!self_clone.is_limit_open_blocked(symbol)
                        || total_net_positions_usdt_size > 0.0);

                let gate_contract = self_clone.get_contract(symbol.as_str());
                if let Some(gate_contract) = gate_contract {
                    let min_order_size = (gate_contract.order_size_min as i64).max(1);
                    let long_price = self_clone.get_valid_order_price(
                        symbol.as_str(),
                        gate_contract.mark_price.parse::<f64>().unwrap_or(0.0) * 0.5,
                        Some("buy"),
                    );
                    let short_price = self_clone.get_valid_order_price(
                        symbol.as_str(),
                        gate_contract.mark_price.parse::<f64>().unwrap_or(0.0) * 1.5,
                        Some("sell"),
                    );

                    let long_order = OrderRequest {
                        symbol: symbol.clone(),
                        side: "buy".to_string(),
                        price: long_price,
                        size: min_order_size.max(1),
                        tif: "gtc".to_string(),
                        level: if open_order_available {
                            "limit_open".to_string()
                        } else {
                            "limit_close".to_string()
                        },
                        login_name: self_clone.login_name.clone(),
                        uid: self_clone.get_uid(),
                        if_not_healthy_ws: false,
                        bypass_safe_limit_close: true,
                        only_reduce_only_for_ws: false,
                        usdt_size: min_order_size.max(1) as f64,
                    };
                    let short_order = OrderRequest {
                        symbol: symbol.clone(),
                        side: "sell".to_string(),
                        price: short_price,
                        size: -min_order_size.max(1),
                        tif: "gtc".to_string(),
                        level: if open_order_available {
                            "limit_open".to_string()
                        } else {
                            "limit_close".to_string()
                        },
                        login_name: self_clone.login_name.clone(),
                        uid: self_clone.get_uid(),
                        if_not_healthy_ws: false,
                        bypass_safe_limit_close: true,
                        only_reduce_only_for_ws: false,
                        usdt_size: -min_order_size.max(1) as f64,
                    };
                    let client = self_clone.order_manager_client.clone().unwrap_or_else(|| {
                        crate::order_manager_client::OrderManagerClient::instance()
                    });

                    let mut has_placed_order = false;

                    if !has_long_order && (has_short_position || long_order_available) {
                        info!(
                            "[GateOrderManager] Placing small long order: {} {} {}",
                            self_clone.login_name,
                            symbol,
                            min_order_size.max(1)
                        );
                        match self_clone.place_order(&client, long_order) {
                            Ok(response) => {
                                info!("[GateOrderManager] Placed long order: {:?}", response);
                                has_placed_order = true;
                            }
                            Err(e) => {
                                error!("[GateOrderManager] Failed to place long order: {}", e);
                                continue;
                            }
                        }
                    }
                    if !has_short_order && (has_long_position || short_order_available) {
                        info!(
                            "[GateOrderManager] Placing small short order: {} {} {}",
                            self_clone.login_name,
                            symbol,
                            min_order_size.max(1)
                        );
                        match self_clone.place_order(&client, short_order) {
                            Ok(response) => {
                                info!("[GateOrderManager] Placed short order: {:?}", response);
                                has_placed_order = true;
                            }
                            Err(e) => {
                                error!("[GateOrderManager] Failed to place short order: {}", e);
                                continue;
                            }
                        }
                    }
                    if has_placed_order {
                        break;
                    }
                }
            }
        });
        Ok(())
    }

    /// Amend order via WebSocket (same as Python: channel futures.order_amend, event api).
    /// Uses a single time read; no extra syscalls so background order flow is not delayed.
    /// req_param: order_id, price (optional: size, amend_text per Gate API)
    pub fn amend_websocket_order(
        &self,
        order_id: &str,
        price: f64,
        size: Option<i64>,
        contract: Option<&str>,
        req_id: Option<&str>,
        side: Option<&str>,
        previous_order: Option<GateOpenOrder>,
        x_gate_expire_time: Option<i64>,
    ) -> Result<(), String> {
        if self.is_monitoring {
            return Ok(());
        }
        // Flipster has no amend — emulate by cancel + resubmit at new price.
        if crate::flipster_cookie_store::global().is_some() {
            let prev = previous_order
                .or_else(|| self.open_orders.load().get(order_id).cloned());
            let Some(prev) = prev else { return Ok(()) };
            let _ = self.flipster_cancel_http(&prev.contract, order_id);
            let new_size = size.unwrap_or(prev.size);
            let side_str = side
                .map(|s| s.to_string())
                .unwrap_or_else(|| if new_size > 0 { "buy".into() } else { "sell".into() });
            let _ = self.flipster_place_order_http(
                contract.unwrap_or(&prev.contract),
                price,
                new_size,
                prev.is_reduce_only,
                &prev.tif,
                Some(&side_str),
            );
            return Ok(());
        }
        let _ = (req_id, x_gate_expire_time); // suppress unused warnings in Flipster mode
        let new_previous_order = if let Some(order) = previous_order {
            Some(order)
        } else {
            self.open_orders.load().get(order_id).cloned()
        };

        let previous_order = if let Some(order) = new_previous_order {
            order
        } else {
            return Ok(());
        };

        // let left_size = previous_order.left;

        // if left_size * previous_order.size < 0 {
        //     warn!(
        //         "[GateOrderManager] Invalid left size: left_size={}, size={}",
        //         left_size, previous_order.size
        //     );
        //     return Ok(());
        // }

        if previous_order.left == 0 {
            warn!(
                "[GateOrderManager] Invalid left size: left_size={}, size={}, contract={}",
                previous_order.left, previous_order.size, previous_order.contract
            );
        }

        let new_size = if size.is_some() && previous_order.left != 0 {
            let previous_filled_size = previous_order.size - previous_order.left;

            Some(size.unwrap_or(0) + previous_filled_size)
        } else {
            None
        };

        let is_same_size = match new_size {
            Some(s) => s == previous_order.size,
            None => true,
        };

        let price_str = match contract {
            Some(c) => self.get_valid_order_price(c, price, side),
            None => format!("{:.6}", price),
        };

        let is_same_price =
            (previous_order.price - price_str.parse::<f64>().unwrap_or(0.0)).abs() < EPS_F64;

        if is_same_price && is_same_size {
            return Ok(());
        }

        // info!(
        //     "[GateOrderManager] Amending order: {:?}, new_size={:?}, new_price={}",
        //     previous_order, new_size, price_str
        // );

        let guard = self.ws_sender.read();
        let handle = guard
            .as_ref()
            .ok_or_else(|| "WebSocket not connected".to_string())?
            .clone();
        drop(guard);

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let timestamp = now.as_secs() as i64;
        let req_id = req_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{}-{}-amend", now.as_nanos() % 10000, order_id));

        let mut req_param = serde_json::Map::new();
        req_param.insert("order_id".to_string(), json!(order_id));
        req_param.insert("price".to_string(), json!(price_str));
        if let Some(sz) = new_size {
            req_param.insert("size".to_string(), json!(sz));
        }

        let mut headers = serde_json::Map::new();
        if let Some(x_gate_expire_time) = x_gate_expire_time {
            headers.insert(
                "x-gate-exptime".to_string(),
                Value::String(x_gate_expire_time.to_string()),
            );
        }

        let mut payload = serde_json::Map::new();
        payload.insert("req_id".to_string(), json!(req_id));
        payload.insert("req_param".to_string(), json!(req_param));

        if !headers.is_empty() {
            payload.insert("req_header".to_string(), json!(headers));
        }

        let request = json!({
            "time": timestamp,
            "channel": "futures.order_amend",
            "event": "api",
            "payload": payload
        });

        if !handle.push_message(Message::Text(request.to_string().into())) {
            warn!("[GateOrderManager] WS queue full, clearing handle");
            *self.ws_sender.write() = None;
            return Ok(());
        }

        // info!(
        //     "[GateOrderManager] Amended order: {:?}, new_size={:?}, new_price={}",
        //     previous_order, new_size, price_str
        // );

        // Update local open_orders (reuse timestamp; no extra now())
        if let Some(mut order) = self.open_orders.load().get(order_id).cloned() {
            order.price = price;
            // let previous_filled_size = order.size - order.left;
            // let new_left = new_size.unwrap_or(0) - previous_filled_size;

            // order.left = new_left;
            // order.size = new_size.unwrap_or(0);

            order.update_time = timestamp;
            let mut orders_map = (**self.open_orders.load()).clone();
            orders_map.insert(order_id.to_string(), order);
            self.open_orders.store(Arc::new(orders_map));
        }

        Ok(())
    }

    /// Request open order list via WebSocket (futures.order_list, event api). Response updates open_orders in handle_ws_message.
    pub fn send_order_list_request(&self) -> Result<(), String> {
        // Flipster mode: private WS pushes order updates continuously — no poll needed.
        if crate::flipster_cookie_store::global().is_some() {
            return Ok(());
        }
        let guard = self.ws_sender.read();
        let handle = guard
            .as_ref()
            .ok_or_else(|| "WebSocket not connected".to_string())?
            .clone();
        drop(guard);

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let timestamp = now.as_secs() as i64;
        let req_id = format!(
            "orderlist-{}-{}",
            now.as_millis() % 100_000,
            self.login_name
        );
        let request = json!({
            "time": timestamp,
            "channel": "futures.order_list",
            "event": "api",
            "payload": {
                "req_id": req_id,
                "req_param": { "status": "open" }
            }
        });
        if !handle.push_message(Message::Text(request.to_string().into())) {
            *self.ws_sender.write() = None;
        }
        Ok(())
    }

    fn repeat_profitable_order(
        order_manager: &GateOrderManager,
        login_name: &str,
        contract: &str,
        size: i64,
        price: f64,
        data_cache: &crate::data_cache::DataCache,
    ) -> Result<(), String> {
        if size == 0 || price <= 0.0 {
            info!(
                "[RepeatProfitableOrder] 🌀 Skip {}: invalid size/price (size={}, price={})",
                contract, size, price
            );
            return Ok(());
        }
        let side = if size > 0 { "buy" } else { "sell" };

        // Only repeat when the trade side reduces portfolio net exposure (same idea as
        // strategy `is_total_net_positions_usdt_size_close_side`). ~Flat net: allow either.
        let total_net_usdt = order_manager
            .total_net_positions_usdt_size_all_accounts
            .as_ref()
            .map(|slot| **slot.load())
            .unwrap_or_else(|| order_manager.get_net_positions_usdt_size());
        let is_net_position_close_side = if total_net_usdt > EPS_F64 {
            side == "sell"
        } else if total_net_usdt < -EPS_F64 {
            side == "buy"
        } else {
            true
        };
        if !is_net_position_close_side {
            info!(
                "[RepeatProfitableOrder] 🌀 Skip {}: not net-position close side (side={}, total_net_usdt={:.4})",
                contract, side, total_net_usdt
            );
            return Ok(());
        }

        debug!(
            "[RepeatProfitableOrder] Start {} side={} size={} price={}",
            contract, side, size, price
        );

        // One get_gate_booktickers = 2 read() calls instead of 2 separate get_*
        let gate_booktickers = data_cache.get_gate_booktickers(contract);
        let (gate_last_book_ticker, gate_last_web_book_ticker) = match gate_booktickers {
            Some((a, b)) => (Some(a), Some(b)),
            None => (None, None),
        };

        let web_book_ticker_order_price = if side == "buy" {
            gate_last_web_book_ticker.as_ref().map(|bt| bt.ask_price)
        } else {
            gate_last_web_book_ticker.as_ref().map(|bt| bt.bid_price)
        };

        if gate_last_book_ticker.is_none() || gate_last_web_book_ticker.is_none() {
            info!(
                "[RepeatProfitableOrder] 🌀 Skip {}: missing bookticker (gate={}, web={})",
                contract,
                gate_last_book_ticker.is_some(),
                gate_last_web_book_ticker.is_some()
            );
            return Ok(());
        }

        let mid_profit_bp = calculate_mid_profit_bp(
            side,
            gate_last_book_ticker.as_ref().unwrap(),
            gate_last_web_book_ticker.as_ref().unwrap(),
        );

        if mid_profit_bp < order_manager.trade_settings.load().close_raw_mid_profit_bp {
            info!(
                "[RepeatProfitableOrder] 🌀 Skip {}: mid_profit_bp {:.4} < threshold {:.4}",
                contract,
                mid_profit_bp,
                order_manager.trade_settings.load().close_raw_mid_profit_bp
            );
            return Ok(());
        }

        if web_book_ticker_order_price.is_some() {
            if side == "buy" && web_book_ticker_order_price.unwrap() > price {
                info!(
                    "[RepeatProfitableOrder] 🌀 Skip {}: web ask {:.6} > trade price {:.6}",
                    contract,
                    web_book_ticker_order_price.unwrap(),
                    price
                );
                return Ok(());
            }
            if side == "sell" && web_book_ticker_order_price.unwrap() < price {
                info!(
                    "[RepeatProfitableOrder] 🌀 Skip {}: web bid {:.6} < trade price {:.6}",
                    contract,
                    web_book_ticker_order_price.unwrap(),
                    price
                );
                return Ok(());
            }
        }
        let current_position_usdt = order_manager.get_usdt_position_size(contract);

        let is_open_order = if side == "buy" {
            current_position_usdt >= 0.0
        } else {
            current_position_usdt <= 0.0
        };

        let ts = order_manager.trade_settings.load();
        let order_size_usdt = if is_open_order {
            ts.order_size
        } else {
            ts.close_order_size.unwrap_or_else(|| ts.order_size)
        };

        if is_open_order {
            if current_position_usdt.abs() + order_size_usdt.abs() > ts.max_position_size {
                info!(
                    "[RepeatProfitableOrder] 🌀 Skip {}: position limit (current_usdt={:.4}, order_usdt={:.4}, max={:.4})",
                    contract,
                    current_position_usdt,
                    order_size_usdt,
                    ts.max_position_size
                );
                return Ok(());
            }
        }

        let order_size_abs =
            order_manager.get_size_from_usdt_amount(contract, order_size_usdt.abs());
        let order_size_abs_max = if side == "buy" {
            gate_last_web_book_ticker
                .as_ref()
                .map(|bt| bt.ask_size)
                .unwrap_or(0.0)
        } else {
            gate_last_web_book_ticker
                .as_ref()
                .map(|bt| bt.bid_size)
                .unwrap_or(0.0)
        };

        let order_size = if side == "buy" {
            order_size_abs.min(order_size_abs_max as i64)
        } else {
            -order_size_abs.min(order_size_abs_max as i64)
        };

        let side_emoji = if side == "buy" { "🔷" } else { "🔶" };
        let new_price = web_book_ticker_order_price.unwrap();

        // v11 series: use WebSocket amend instead of order_manager_client place_order
        if ts.repeat_profitable_order_use_amend == Some(true) {
            let open_orders = order_manager.get_open_orders_for_contract(contract);
            let same_side_orders: Vec<_> = if side == "buy" {
                open_orders.into_iter().filter(|o| o.size > 0).collect()
            } else {
                open_orders.into_iter().filter(|o| o.size < 0).collect()
            };
            if let Some(open_order) = same_side_orders.into_iter().next() {
                info!(
                    "\x1b[92m[RepeatProfitableOrder] ✨ {} Amend:\x1b[0m | {} | order_id={} | size={} | price={} | {}",
                    side_emoji, contract, open_order.id, order_size, new_price, login_name
                );
                if let Err(err) = order_manager.amend_websocket_order(
                    &open_order.id,
                    new_price,
                    Some(order_size),
                    Some(contract),
                    None,
                    Some(side),
                    None,
                    None,
                ) {
                    warn!(
                        "[GateOrderManager {}] Failed to repeat profitable order (amend) for {}: {}",
                        login_name, contract, err
                    );
                }
            } else {
                info!(
                    "[RepeatProfitableOrder] 🌀 Skip {}: no open {} order to amend",
                    contract, side
                );
            }
            return Ok(());
        }

        // Print order (same as Python print_order)
        info!(
            "\x1b[92m[RepeatProfitableOrder] ✨ {} Order:\x1b[0m | {} | {} | {} | {} | {}",
            side_emoji, contract, order_size, price, new_price, login_name
        );

        let level = if is_open_order {
            "limit_open".to_string()
        } else {
            "limit_close".to_string()
        };

        let tif = if is_open_order {
            order_manager.trade_settings.load().limit_open_tif.clone()
        } else {
            order_manager.trade_settings.load().limit_close_tif.clone()
        };

        let order_request = crate::order_manager_client::OrderRequest {
            symbol: contract.to_string(),
            side: side.to_string(),
            price: new_price.to_string(),
            size: order_size,
            tif: tif,
            level: level,
            login_name: login_name.to_string(),
            uid: order_manager.get_uid(),
            if_not_healthy_ws: false,
            only_reduce_only_for_ws: false,
            bypass_safe_limit_close: false,
            usdt_size: order_size_usdt.abs(),
        };

        let order_manager = order_manager.clone();
        let login_name = login_name.to_string();
        let contract = contract.to_string();
        let client = order_manager
            .order_manager_client
            .clone()
            .unwrap_or_else(|| crate::order_manager_client::OrderManagerClient::instance());
        std::thread::spawn(move || {
            if let Err(err) = order_manager.place_order(&client, order_request) {
                warn!(
                    "[GateOrderManager {}] Failed to repeat profitable order for {}: {}",
                    login_name, contract, err
                );
            }
        });

        Ok(())
    }

    /// Sign request with HMAC SHA512
    fn sign(&self, payload: &str) -> String {
        let mut mac = Hmac::<Sha512>::new_from_slice(self.api_secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Hash body with SHA512
    fn hash_body(&self, body: &str) -> String {
        use sha2::Digest;
        let mut hasher = Sha512::new();
        hasher.update(body.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Build authenticated request headers
    fn build_auth_headers(
        &self,
        method: &str,
        request_url: &str,
        query: Option<&str>,
        body: Option<&str>,
    ) -> Result<HashMap<String, String>> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();

        let query_str = query.unwrap_or("");
        let body_hash = body
            .map(|b| self.hash_body(b))
            .unwrap_or_else(|| self.hash_body(""));

        let signature_string = format!(
            "{}\n{}\n{}\n{}\n{}",
            method, request_url, query_str, body_hash, timestamp
        );
        let signature = self.sign(&signature_string);

        let mut headers = HashMap::new();
        headers.insert("KEY".to_string(), self.api_key.clone());
        headers.insert("Timestamp".to_string(), timestamp);
        headers.insert("SIGN".to_string(), signature);
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        Ok(headers)
    }

    /// Update positions from Gate API
    pub fn update_positions(&self) -> Result<()> {
        // Flipster mode: positions are populated live by flipster_private_ws.
        // REST bootstrap is not needed — just return OK.
        if crate::flipster_cookie_store::global().is_some() {
            return Ok(());
        }

        let url = format!("{}/positions", GATE_FUTURES_API_BASE);
        let request_url = "/api/v4/futures/usdt/positions";

        let headers = self.build_auth_headers("GET", request_url, None, None)?;

        let response = self
            .client
            .get(&url)
            .headers({
                let mut h = reqwest::header::HeaderMap::new();
                for (k, v) in &headers {
                    h.insert(
                        reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                        reqwest::header::HeaderValue::from_str(v).unwrap(),
                    );
                }
                h
            })
            .send()
            .context("Failed to fetch positions from Gate API")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow::anyhow!("Gate API error ({}): {}", status, text));
        }

        let positions_json: Value = response
            .json()
            .context("Failed to parse positions response")?;

        let positions: Vec<GatePosition> = positions_json
            .as_array()
            .unwrap()
            .iter()
            .map(|v| GatePosition {
                contract: v
                    .get("contract")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                size: v.get("size").and_then(|v| v.as_i64()).unwrap_or(0),
                risk_limit: v
                    .get("risk_limit")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0),
                entry_price: v
                    .get("entry_price")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0),
                mark_price: v
                    .get("mark_price")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0),
                realised_pnl: v
                    .get("realised_pnl")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0),
                last_close_pnl: v
                    .get("last_close_pnl")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0),
                history_pnl: v
                    .get("history_pnl")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0),
                update_time: v.get("update_time").and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect();

        let mut pos_map = (**self.positions.load()).clone();
        pos_map.clear();
        for pos in positions {
            let pos_clone = pos.clone();

            pos_map.insert(pos_clone.contract.clone(), pos_clone);
            debug!("[GateOrderManager] \x1b[93mUpdate positions:\x1b[0m login_name={}, contract={}, size={}", self.login_name, pos.contract.clone(), pos.size);
        }
        self.positions.store(Arc::new(pos_map));

        *self.last_position_updated_time_ms.write() = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_millis(0))
            .as_millis() as i64;

        Ok(())
    }

    pub fn is_warn_stale_position(
        &self,
        position: &GatePosition,
        current_time_sec: Option<i64>,
        warn_stale_minutes: Option<i64>,
    ) -> bool {
        let current_time_sec = current_time_sec.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
        });
        let warn_stale_minutes = warn_stale_minutes
            .unwrap_or_else(|| self.trade_settings.load().clone().warn_stale_minutes);
        position.update_time > 0
            && position.size != 0
            && position.update_time < current_time_sec - warn_stale_minutes * 60
    }

    pub fn is_stale_position(
        &self,
        position: &GatePosition,
        current_time_sec: Option<i64>,
    ) -> bool {
        let current_time_sec = current_time_sec.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
        });
        let trade_settings = self.trade_settings.load().clone();
        let close_stale_minutes = trade_settings.close_stale_minutes;
        position.update_time > 0
            && position.size != 0
            && position.update_time < current_time_sec - close_stale_minutes * 60
    }

    pub fn is_contract_stale_position(
        &self,
        contract: &str,
        current_time_sec: Option<i64>,
    ) -> bool {
        let position = self.get_position(contract);
        if position.is_none() {
            return false;
        }
        self.is_stale_position(&position.unwrap(), current_time_sec)
    }

    pub fn try_to_close_stale_positions(&self) -> Result<()> {
        let current_time_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let trade_settings = self.trade_settings.load().clone();
        // let close_stale_minutes = trade_settings.close_stale_minutes;
        let positions = self.positions.load().clone();
        for (contract, position) in positions.iter() {
            if self.is_contract_stale_position(contract, Some(current_time_sec)) {
                info!(
                    "[GateOrderManager {}] Closing stale position for {}: {}",
                    self.login_name, contract, position.size
                );
                while self.has_position(contract) {
                    // let open_orders = self.get_open_orders_for_contract(contract);

                    let max_size = self.get_size_from_usdt_amount(
                        contract,
                        trade_settings
                            .market_close_order_size
                            .unwrap_or(trade_settings.close_order_size.unwrap_or(0.0)),
                    );

                    let position = self.get_position(contract);
                    if position.is_none() {
                        break;
                    }

                    // close all open orders
                    if let Err(e) = self.cancel_websocket_orders_matched(Some(contract), None) {
                        warn!(
                            "[GateOrderManager {}] Failed to cancel open orders for {}: {}",
                            self.login_name, contract, e
                        );
                    }

                    let position = position.unwrap();

                    let stale_time_sec = current_time_sec - position.update_time;
                    let stale_hours = stale_time_sec / 3600;

                    let order_size = max_size.min(position.size.abs() as i64);
                    let usdt_size = self.get_usdt_amount_from_size(contract, order_size);

                    // // Only market-close stale legs when the close side reduces portfolio net exposure
                    // // (same rule as is_total_net_positions_usdt_size_close_side: sell only if net long,
                    // // buy only if net short; ~flat net allows either).
                    // let total_net_usdt = self.get_net_positions_usdt_size();
                    // if position.size > 0 && total_net_usdt < -EPS_F64 {
                    //     warn!(
                    //         "[GateOrderManager {}] Skip stale market close (sell {}) — portfolio net short (total_net_usdt={:.4}), not a net-reducing close",
                    //         self.login_name, contract, total_net_usdt
                    //     );
                    //     break;
                    // }
                    // if position.size < 0 && total_net_usdt > EPS_F64 {
                    //     warn!(
                    //         "[GateOrderManager {}] Skip stale market close (buy {}) — portfolio net long (total_net_usdt={:.4}), not a net-reducing close",
                    //         self.login_name, contract, total_net_usdt
                    //     );
                    //     break;
                    // }

                    if position.size > 0 {
                        let order = OrderRequest {
                            symbol: contract.clone(),
                            side: "sell".to_string(),
                            price: "0.0".to_string(),
                            size: -order_size,
                            tif: "ioc".to_string(),
                            level: "market_close".to_string(),
                            login_name: self.login_name.clone(),
                            uid: self.get_uid(),
                            if_not_healthy_ws: false,
                            bypass_safe_limit_close: false,
                            only_reduce_only_for_ws: false,
                            usdt_size: usdt_size,
                        };
                        let client = self.order_manager_client.clone().unwrap_or_else(|| {
                            crate::order_manager_client::OrderManagerClient::instance()
                        });
                        info!(
                        "[GateOrderManager {}] Placing market close order for {}: {} (stale hours: {})",
                        self.login_name, contract, order_size, stale_hours
                    );
                        if let Err(e) = self.place_order(&client, order) {
                            warn!(
                            "[GateOrderManager {}] Failed to place market close order for {}: {}",
                            self.login_name, contract, e
                        );
                        }
                        // let open_order = open_orders.into_iter().filter(|o| o.size < 0).next();
                    } else if position.size < 0 {
                        // let open_order = open_orders.into_iter().filter(|o| o.size > 0).next();
                        let order = OrderRequest {
                            symbol: contract.clone(),
                            side: "buy".to_string(),
                            price: "0.0".to_string(),
                            size: order_size,
                            tif: "ioc".to_string(),
                            level: "market_close".to_string(),
                            login_name: self.login_name.clone(),
                            uid: self.get_uid(),
                            if_not_healthy_ws: false,
                            bypass_safe_limit_close: false,
                            only_reduce_only_for_ws: false,
                            usdt_size: -usdt_size,
                        };
                        let client = self.order_manager_client.clone().unwrap_or_else(|| {
                            crate::order_manager_client::OrderManagerClient::instance()
                        });
                        info!(
                        "[GateOrderManager {}] Placing market close order for {}: {} (stale hours: {})",
                        self.login_name, contract, order_size, stale_hours
                    );
                        if let Err(e) = self.place_order(&client, order) {
                            warn!(
                            "[GateOrderManager {}] Failed to place market close order for {}: {}",
                            self.login_name, contract, e
                        );
                        }
                    } else {
                        break;
                    }

                    std::thread::sleep(Duration::from_millis(1000));
                    self.update_positions()?;
                    std::thread::sleep(Duration::from_millis(1000));
                }
            }
        }
        Ok(())
    }

    /// If data_cache is set, normalize open order prices when close to gate bookticker: size>0 => price/2, size<0 => price*2.
    fn normalize_open_order_prices_from_bookticker(
        &self,
        orders_map: &mut HashMap<String, GateOpenOrder>,
    ) {
        let cache = match &self.data_cache {
            Some(c) => c,
            None => return,
        };
        for order in orders_map.values_mut() {
            if order.status != "open" || order.size == 0 {
                continue;
            }
            let bt = match cache.get_gate_bookticker(&order.contract) {
                Some(b) => b,
                None => continue,
            };
            let ref_price = if order.size > 0 {
                bt.ask_price
            } else {
                bt.bid_price
            };
            if ref_price <= 0.0 {
                continue;
            }
            let diff_ratio = (order.price - ref_price).abs() / ref_price;
            if diff_ratio <= OPEN_ORDER_PRICE_CLOSE_TO_BOOKTICKER_RATIO {
                if order.size > 0 {
                    order.price /= 2.0;
                } else {
                    order.price *= 2.0;
                }
            }
        }
    }

    /// Update open orders from Gate REST API (GET /futures/usdt/orders?status=open).
    /// Complements WebSocket futures.orders updates with a periodic REST snapshot.
    pub fn update_open_orders(&self) -> Result<()> {
        // Flipster mode: private WS continuously populates open_orders — REST poll is noop.
        if crate::flipster_cookie_store::global().is_some() {
            return Ok(());
        }

        let url = format!("{}/orders", GATE_FUTURES_API_BASE);
        let request_url = "/api/v4/futures/usdt/orders";

        let headers = self.build_auth_headers("GET", request_url, Some("status=open"), None)?;

        let response = self
            .client
            .get(&url)
            .query(&[("status", "open")])
            .headers({
                let mut h = reqwest::header::HeaderMap::new();
                for (k, v) in &headers {
                    h.insert(
                        reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                        reqwest::header::HeaderValue::from_str(v).unwrap(),
                    );
                }
                h
            })
            .send()
            .context("Failed to fetch open orders from Gate API")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow::anyhow!("Gate API error ({}): {}", status, text));
        }

        let orders_json: Vec<Value> = response.json().context("Failed to parse orders response")?;

        let mut orders_map = HashMap::new();
        for v in orders_json {
            let status = v.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status != "open" {
                continue;
            }
            let id = v
                .get("id")
                .and_then(|x| x.as_i64())
                .or_else(|| {
                    v.get("id")
                        .and_then(|x| x.as_str())
                        .and_then(|s| s.parse().ok())
                })
                .map(|n| n.to_string());
            let id_str = match id {
                Some(s) => s,
                None => continue,
            };
            let contract = v
                .get("contract")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let create_time = v.get("create_time").and_then(|x| x.as_i64()).unwrap_or(0);
            let create_time_ms = v
                .get("create_time_ms")
                .and_then(|x| x.as_i64())
                .or_else(|| {
                    v.get("create_time_ms")
                        .and_then(|x| x.as_str())
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(create_time * 1000);
            let price = v
                .get("price")
                .and_then(|x| x.as_f64())
                .or_else(|| {
                    v.get("price")
                        .and_then(|x| x.as_str())
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(0.0);
            let size = v
                .get("size")
                .and_then(|x| x.as_i64())
                .or_else(|| {
                    v.get("size")
                        .and_then(|x| x.as_str())
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(0);
            let left = v
                .get("left")
                .and_then(|x| x.as_i64())
                .or_else(|| {
                    v.get("left")
                        .and_then(|x| x.as_str())
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(0);
            let is_reduce_only = v
                .get("is_reduce_only")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            let is_close = v.get("is_close").and_then(|x| x.as_bool()).unwrap_or(false);
            let tif = v
                .get("tif")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let text = v
                .get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let update_time = v
                .get("update_time")
                .and_then(|x| x.as_i64())
                .or_else(|| {
                    v.get("update_time")
                        .and_then(|x| x.as_str())
                        .and_then(|s| s.parse().ok())
                })
                .unwrap_or(create_time);
            let amend_text = v
                .get("amend_text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();

            let order = GateOpenOrder {
                id: id_str.clone(),
                contract: contract.clone(),
                create_time_ms,
                price,
                size,
                status: "open".to_string(),
                left,
                is_reduce_only,
                is_close,
                tif,
                text,
                update_time,
                amend_text,
            };

            orders_map.insert(id_str.clone(), order);

            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;

            let left_usdt_amount = self.get_usdt_amount_from_size(&contract, left);

            let gate_contract = self.get_contract(&contract);

            let min_order_size = gate_contract
                .map(|c| (c.order_size_min as i64).max(1))
                .unwrap_or(1);

            let created_ago_ms = now_ms - create_time_ms;

            if !self.is_account_symbol(&contract) && created_ago_ms > 1000 * 60 * 10 {
                if let Err(e) = self.cancel_websocket_orders_matched(Some(&contract), None) {
                    warn!(
                        "[GateOrderManager {}] Failed to cancel non-account-symbol order {} ({}): {}",
                        self.login_name, id_str, contract, e
                    );
                }
                orders_map.remove(&id_str);
            } else if now_ms - update_time > 1000
                && left_usdt_amount.abs() > 100.0
                && left.abs() > min_order_size.max(1) * 2
            {
                debug!(
                    "[GateOrderManager {}] Cancelling order {} ({}) because it has been open for 1s and left_usdt_amount={}",
                    self.login_name, id_str, contract, left_usdt_amount
                );
                if let Err(e) = self.cancel_order(&id_str) {
                    warn!(
                        "[GateOrderManager {}] Failed to cancel order {} ({}): {}",
                        self.login_name, id_str, contract, e
                    );
                }
                orders_map.remove(&id_str);
            }
        }

        self.open_orders.store(Arc::new(orders_map));
        Ok(())
    }

    pub fn update_account_symbols(&self, symbols: Vec<String>) -> Result<()> {
        let mut account_symbols = self.symbols.write();
        *account_symbols = symbols;
        Ok(())
    }

    /// Update leverage for a contract (same as Python update_position_leverage)
    /// POST /api/v4/futures/usdt/positions/{contract}/leverage?leverage={leverage}
    pub fn update_position_leverage(&self, contract: &str, leverage: i32) -> Result<()> {
        // Flipster: leverage is set per-order in the POST body, no account-level REST update.
        if crate::flipster_cookie_store::global().is_some() {
            let _ = (contract, leverage);
            return Ok(());
        }

        let url = format!("{}/positions/{}/leverage", GATE_FUTURES_API_BASE, contract);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let method = "POST";
        let path = format!("/api/v4/futures/usdt/positions/{}/leverage", contract);
        let query_string = format!("leverage=0&cross_leverage_limit={}", leverage);
        let payload = format!("{}\n{}\n{}\n{}", method, path, query_string, timestamp);

        let signature = GateOrderManager::sign_static(&self.api_secret, &payload);
        let timestamp_str = timestamp.to_string();

        let response = self
            .client
            .post(&format!("{}?{}", url, query_string))
            .headers({
                use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
                let mut h = HeaderMap::new();
                h.insert(
                    HeaderName::from_static("key"),
                    HeaderValue::from_str(&self.api_key).unwrap(),
                );
                h.insert(
                    HeaderName::from_static("timestamp"),
                    HeaderValue::from_str(&timestamp_str).unwrap(),
                );
                h.insert(
                    HeaderName::from_static("sign"),
                    HeaderValue::from_str(&signature).unwrap(),
                );
                h
            })
            .send()
            .context("Failed to update leverage from Gate API")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow::anyhow!("Gate API error ({}): {}", status, text));
        }

        Ok(())
    }

    /// Update leverage for all symbols if needed (same as Python update_leverage logic)
    pub fn update_leverage_if_needed(&self, max_leverage: i32) -> Result<()> {
        let contracts = self.contracts.load();
        let symbols = self.symbols.read();
        for symbol in symbols.iter() {
            if let Some(contract) = contracts.get(symbol) {
                let leverage_max: i32 = contract.leverage_max.parse().unwrap_or(0);
                if leverage_max > max_leverage {
                    elog!(
                        "[GateOrderManager {}] Updating contract {} leverage to {}",
                        self.login_name,
                        symbol,
                        max_leverage
                    );
                    if let Err(e) = self.update_position_leverage(symbol, max_leverage) {
                        elog!(
                            "[GateOrderManager {}] Error updating contract {} leverage: {}",
                            self.login_name,
                            symbol,
                            e
                        );
                    } else {
                        // Sleep 300ms between updates (same as Python time.sleep(0.3))
                        std::thread::sleep(Duration::from_millis(300));
                    }
                }
            }
        }
        Ok(())
    }

    /// Start background update loop (REST API fallback + WebSocket)
    pub fn start_update_loop(&self) {
        if self.save_metrics {
            if let Err(e) = Self::update_metrics_from_file_static(
                &self.login_name,
                &self.order_count_map,
                &self.trade_count_map,
                &self.profitable_trade_count_map,
                &self.profit_bp_ema_map,
            ) {
                error!(
                    "[GateOrderManager] Failed to update metrics from file: {}",
                    e
                );
            }
            if let Err(e) = Self::save_metrics_to_file_static(
                &self.login_name,
                &self.order_count_map,
                &self.trade_count_map,
                &self.profitable_trade_count_map,
                &self.profit_bp_ema_map,
            ) {
                error!("[GateOrderManager] Failed to save metrics: {}", e);
            }
        }
        // Start WebSocket for real-time updates
        self.start_websocket();

        // Poll order list via WebSocket every 1 second (futures.order_list api)
        let manager_orderlist = self.clone_for_update();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            if !manager_orderlist.is_monitoring {
                if let Err(e) = manager_orderlist.send_order_list_request() {
                    trace!(
                    "[GateOrderManager {}] order_list request: {} (WS may not be connected yet)",
                        manager_orderlist.login_name,
                        e
                    );
                }
            }
        });

        // Start futures account streaming (10 seconds interval, same as Python)
        if self.update_futures_account_data {
            self.start_futures_account_streaming();
        }

        if self.run_health_check {
            // Start health check (300 seconds interval, same as Python)
            self.start_health_check();
        }

        if self.run_small_orders_loop && !self.is_monitoring {
            let _ = self.start_small_orders_loop();
        }

        // Start change_percentage-based limit_open blocker
        self.start_change_percentage_blocker();
        // Start redis dangerous symbol blocker
        self.start_dangerous_symbol_blocker();
        // Start redis market close symbol blocker
        self.start_market_close_symbol_blocker();

        // Also start REST API fallback (periodic contracts update, single positions bootstrap)
        let manager = self.clone_for_update();

        std::thread::spawn(move || {
            // Initial update
            if let Err(e) =
                update_gate_contracts_cached(&manager.client, manager.contracts.as_ref())
            {
                error!("[GateOrderManager] Failed to update contracts: {}", e);
            }
            if let Err(e) = manager.update_positions() {
                error!("[GateOrderManager] Failed to update positions: {}", e);
            }
            if !manager.is_monitoring {
                if let Err(e) = manager.try_to_close_stale_positions() {
                    error!(
                        "[GateOrderManager] Failed to try to close stale positions: {}",
                        e
                    );
                }
                if let Err(e) = manager.update_open_orders() {
                    error!("[GateOrderManager] Failed to update open orders: {}", e);
                }
            }

            // Periodic refresh: open orders every 5s, contracts every 60s
            let mut last_contract_update = std::time::Instant::now();
            let mut last_open_orders_update = std::time::Instant::now();
            let mut last_save_metrics = std::time::Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(5));
                if last_open_orders_update.elapsed() >= Duration::from_secs(5) {
                    if let Err(e) =
                        update_gate_contracts_cached(&manager.client, manager.contracts.as_ref())
                    {
                        error!("[GateOrderManager] Failed to update contracts: {}", e);
                    }
                    if let Err(e) = manager.update_open_orders() {
                        error!("[GateOrderManager] Failed to update open orders: {}", e);
                    }
                    last_open_orders_update = std::time::Instant::now();
                    if let Err(e) = manager.cancel_duplicate_orders() {
                        error!(
                            "[GateOrderManager] Failed to cancel duplicate orders: {}",
                            e
                        );
                    }

                    if let Err(e) = manager.update_positions() {
                        error!("[GateOrderManager] Failed to update positions: {}", e);
                    }
                    if let Err(e) = manager.try_to_close_stale_positions() {
                        error!(
                            "[GateOrderManager] Failed to try to close stale positions: {}",
                            e
                        );
                    }
                }
                if last_contract_update.elapsed() >= Duration::from_secs(60) {
                    if let Err(e) =
                        update_gate_contracts_cached(&manager.client, manager.contracts.as_ref())
                    {
                        error!("[GateOrderManager] Failed to update contracts: {}", e);
                    }
                    last_contract_update = std::time::Instant::now();
                }
                if manager.save_metrics && last_save_metrics.elapsed() >= Duration::from_secs(60) {
                    if let Err(e) = Self::save_metrics_to_file_static(
                        &manager.login_name,
                        &manager.order_count_map,
                        &manager.trade_count_map,
                        &manager.profitable_trade_count_map,
                        &manager.profit_bp_ema_map,
                    ) {
                        error!("[GateOrderManager] Failed to save metrics: {}", e);
                    }
                    last_save_metrics = std::time::Instant::now();
                }
            }
        });
    }

    pub fn set_trade_callback<F>(&self, callback: F)
    where
        F: Fn(&str, i64, f64, f64, f64) + Send + Sync + 'static,
    {
        *self.trade_callback.write() = Some(Box::new(callback));
    }

    pub fn is_limit_open_blocked(&self, symbol: &str) -> bool {
        if self
            .limit_open_blocked_symbols
            .try_read()
            .map(|g| g.contains(symbol))
            .unwrap_or(false)
        {
            return true;
        }
        self.dangerous_limit_open_symbols
            .try_read()
            .map(|g| g.contains(symbol))
            .unwrap_or(false)
    }

    /// Open orders for one contract. Uses a per-tick cache when open_orders Arc is unchanged to avoid iterating all orders every symbol.
    pub fn get_open_orders_for_contract(&self, symbol: &str) -> Vec<GateOpenOrder> {
        let current = self.open_orders.load();
        {
            let guard = self.open_orders_by_contract_cache.read();
            if let Some(ref cached_arc) = guard.0 {
                if Arc::ptr_eq(cached_arc, &current) {
                    return guard.1.get(symbol).cloned().unwrap_or_default();
                }
            }
        }
        let mut guard = self.open_orders_by_contract_cache.write();
        if let Some(ref cached_arc) = guard.0 {
            if Arc::ptr_eq(cached_arc, &current) {
                return guard.1.get(symbol).cloned().unwrap_or_default();
            }
        }
        guard.0 = Some(current.clone());
        guard.1.clear();
        for order in current.values() {
            guard
                .1
                .entry(order.contract.clone())
                .or_default()
                .push(order.clone());
        }
        guard.1.get(symbol).cloned().unwrap_or_default()
    }

    pub fn should_market_close(&self, symbol: &str) -> bool {
        let market_close = self.market_close_symbols.read();
        if !market_close.contains(symbol) {
            return false;
        }
        drop(market_close);

        let interval_seconds = self
            .trade_settings
            .load()
            .should_market_close_inverval_seconds
            .max(0);
        let interval_ms = interval_seconds.saturating_mul(1000);
        if interval_ms == 0 {
            return true;
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let last_trade_ms = self
            .last_trade_time_map
            .read()
            .get(symbol)
            .copied()
            .unwrap_or(0);

        if last_trade_ms <= 0 {
            return true;
        }
        now_ms.saturating_sub(last_trade_ms) >= interval_ms
    }

    fn start_change_percentage_blocker(&self) {
        let top_n: usize = std::env::var("LIMIT_OPEN_BLOCK_CHANGE_PERCENTAGE_TOP")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        if top_n == 0 {
            return;
        }

        let interval_secs: u64 = std::env::var("LIMIT_OPEN_BLOCK_CHANGE_PERCENTAGE_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        let blocked_symbols = self.limit_open_blocked_symbols.clone();
        let client = self.client.clone();

        std::thread::spawn(move || loop {
            match fetch_top_change_percentage_symbols_cached(&client, top_n) {
                Ok(symbols) => {
                    let mut next_set = HashSet::new();
                    for symbol in symbols {
                        if symbol.contains("BTC") || symbol.contains("ETH") {
                            continue;
                        }
                        next_set.insert(symbol);
                    }
                    *blocked_symbols.write() = next_set;
                }
                Err(e) => {
                    warn!(
                        "[GateOrderManager] Failed to fetch top change_percentage symbols: {}",
                        e
                    );
                }
            }
            std::thread::sleep(Duration::from_secs(interval_secs));
        });
    }

    fn start_dangerous_symbol_blocker(&self) {
        let redis_url = std::env::var("REDIS_URL").ok();
        let redis_key = std::env::var("REDIS_DANGEROUS_SYMBOLS_KEY")
            .unwrap_or_else(|_| "gate_hft:dangerous_symbols".to_string());
        let interval_secs: u64 = std::env::var("DANGEROUS_SYMBOLS_REDIS_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        let dangerous_symbols = self.dangerous_limit_open_symbols.clone();

        let redis_url = match redis_url {
            Some(url) => url,
            None => return,
        };

        std::thread::spawn(move || {
            let client = match RedisClient::open(redis_url.as_str()) {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        "[GateOrderManager] Failed to create Redis client for dangerous symbols: {}",
                        e
                    );
                    return;
                }
            };

            loop {
                let mut conn = match client.get_connection() {
                    Ok(conn) => conn,
                    Err(e) => {
                        warn!(
                            "[GateOrderManager] Failed to connect to Redis for dangerous symbols: {}",
                            e
                        );
                        std::thread::sleep(Duration::from_secs(interval_secs));
                        continue;
                    }
                };

                match Self::fetch_dangerous_symbols(&mut conn, &redis_key) {
                    Ok(symbols) => {
                        *dangerous_symbols.write() = symbols;
                    }
                    Err(e) => {
                        warn!(
                            "[GateOrderManager] Failed to fetch dangerous symbols from Redis: {}",
                            e
                        );
                    }
                }

                std::thread::sleep(Duration::from_secs(interval_secs));
            }
        });
    }

    fn start_market_close_symbol_blocker(&self) {
        let redis_url = std::env::var("REDIS_URL").ok();
        let redis_key = std::env::var("REDIS_MARKET_CLOSE_SYMBOLS_KEY")
            .unwrap_or_else(|_| "gate_hft:market_close_symbols".to_string());
        let interval_secs: u64 = std::env::var("MARKET_CLOSE_SYMBOLS_REDIS_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        let market_close_symbols = self.market_close_symbols.clone();

        let redis_url = match redis_url {
            Some(url) => url,
            None => return,
        };

        std::thread::spawn(move || {
            let client = match RedisClient::open(redis_url.as_str()) {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        "[GateOrderManager] Failed to create Redis client for market close symbols: {}",
                        e
                    );
                    return;
                }
            };

            loop {
                let mut conn = match client.get_connection() {
                    Ok(conn) => conn,
                    Err(e) => {
                        warn!(
                            "[GateOrderManager] Failed to connect to Redis for market close symbols: {}",
                            e
                        );
                        std::thread::sleep(Duration::from_secs(interval_secs));
                        continue;
                    }
                };

                match Self::fetch_dangerous_symbols(&mut conn, &redis_key) {
                    Ok(symbols) => {
                        *market_close_symbols.write() = symbols;
                    }
                    Err(e) => {
                        warn!(
                            "[GateOrderManager] Failed to fetch market close symbols from Redis: {}",
                            e
                        );
                    }
                }

                std::thread::sleep(Duration::from_secs(interval_secs));
            }
        });
    }

    fn fetch_dangerous_symbols(
        conn: &mut redis::Connection,
        redis_key: &str,
    ) -> Result<HashSet<String>> {
        let entries: HashMap<String, String> = conn
            .hgetall(redis_key)
            .context("Failed to read dangerous symbols from Redis")?;

        let mut values: HashMap<String, bool> = HashMap::new();
        let mut updated_ms: HashMap<String, i64> = HashMap::new();

        for (field, value) in entries {
            if let Some(symbol) = field.strip_suffix(":updated_at_ms") {
                if let Ok(ts) = value.parse::<i64>() {
                    updated_ms.insert(symbol.to_string(), ts);
                }
                continue;
            }

            if let Ok(json) = serde_json::from_str::<Value>(&value) {
                let val = json
                    .get("value")
                    .and_then(|v| v.as_bool())
                    .or_else(|| json.get("value").and_then(|v| v.as_i64()).map(|v| v != 0));
                if let Some(is_true) = val {
                    values.insert(field.clone(), is_true);
                }
                if let Some(ts) = json.get("updated_at_ms").and_then(|v| v.as_i64()) {
                    updated_ms.insert(field.clone(), ts);
                }
                continue;
            }

            let is_true = matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "y" | "on"
            );
            values.insert(field.clone(), is_true);
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let max_age_ms = 60 * 60 * 1000;

        let mut result = HashSet::new();
        for (symbol, is_true) in values {
            if !is_true {
                continue;
            }
            if let Some(ts) = updated_ms.get(&symbol) {
                if now_ms - ts <= max_age_ms {
                    result.insert(symbol);
                }
            }
        }

        Ok(result)
    }

    /// Start futures account streaming (same as Python start_futures_account_streaming)
    fn start_futures_account_streaming(&self) {
        let login_name = self.login_name.clone();
        let api_key = self.api_key.clone();
        let api_secret = self.api_secret.clone();
        let futures_account_data = self.futures_account_data.clone();
        let client = self.client.clone();

        std::thread::spawn(move || {
            // Silent - match Python behavior (no streaming start message)
            loop {
                match Self::fetch_futures_account(&client, &api_key, &api_secret) {
                    Ok(account_data) => {
                        futures_account_data.store(Arc::new(Some(account_data)));
                    }
                    Err(e) => {
                        elog!(
                            "[GateOrderManager {}] Failed to fetch futures account: {}",
                            login_name,
                            e
                        );
                    }
                }

                std::thread::sleep(Duration::from_secs(10)); // Same as Python (10 seconds)
            }
        });
    }

    /// Fetch futures account data from Gate API
    fn fetch_futures_account(
        client: &Client,
        api_key: &str,
        api_secret: &str,
    ) -> Result<FuturesAccountData> {
        // Flipster mode: noop — data is pushed live by flipster_private_ws into
        // futures_account_data; fetch_futures_account is called from a background
        // thread that *stores* its return into futures_account_data, so we return an
        // error here to let it skip overwriting the live WS data.
        if crate::flipster_cookie_store::global().is_some() {
            let _ = (client, api_key, api_secret);
            return Err(anyhow::anyhow!("flipster-mode: use private WS margins"));
        }

        let url = format!("{}/accounts", GATE_FUTURES_API_BASE);
        let request_url = "/api/v4/futures/usdt/accounts";

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();

        let query_str = "";
        let body_hash = Self::hash_body_static("");
        let signature_string = format!(
            "GET\n{}\n{}\n{}\n{}",
            request_url, query_str, body_hash, timestamp
        );
        let signature = Self::sign_static(api_secret, &signature_string);

        let response = client
            .get(&url)
            .header("KEY", api_key)
            .header("Timestamp", &timestamp)
            .header("SIGN", &signature)
            .send()
            .context("Failed to fetch futures account")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow::anyhow!("Gate API error ({}): {}", status, text));
        }

        let account_data: FuturesAccountData = response
            .json()
            .context("Failed to parse futures account response")?;

        Ok(account_data)
    }

    /// Static helper for signing (used in background thread)
    fn sign_static(api_secret: &str, payload: &str) -> String {
        let mut mac = Hmac::<Sha512>::new_from_slice(api_secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Static helper for hashing body (used in background thread)
    fn hash_body_static(body: &str) -> String {
        use sha2::Digest;
        let mut hasher = Sha512::new();
        hasher.update(body.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Get futures account data — lock-free load; write never blocks read.
    pub fn get_futures_account_data(&self) -> Option<(f64, f64)> {
        let guard = self.futures_account_data.load();
        guard.as_ref().as_ref().map(|data| {
            let total: f64 = data.total.parse().unwrap_or(0.0);
            let unrealised_pnl: f64 = data.unrealised_pnl.parse().unwrap_or(0.0);
            (total, unrealised_pnl)
        })
    }

    pub fn update_strategy_config(&self, symbols: Vec<String>, trade_settings: TradeSettings) {
        self.update_account_symbols(symbols).unwrap();
        self.order_size.store(Arc::new(trade_settings.order_size));
        self.trade_settings.store(Arc::new(trade_settings));
    }

    pub fn is_close_symbol(&self, symbol: &str) -> bool {
        // symbol not in self.symbols but has position
        !self.symbols.read().contains(&symbol.to_string()) && self.has_position(symbol)
    }

    /// Clone for background thread (only what's needed)
    fn clone_for_update(&self) -> Self {
        Self {
            login_name: self.login_name.clone(),
            api_key: self.api_key.clone(),
            api_secret: self.api_secret.clone(),
            uid: self.uid,
            client: self.client.clone(),
            positions: self.positions.clone(),
            contracts: self.contracts.clone(),
            open_orders: self.open_orders.clone(),
            open_orders_by_contract_cache: Arc::new(RwLock::new((None, HashMap::new()))),
            order_count_map: self.order_count_map.clone(),
            trade_count_map: self.trade_count_map.clone(),
            profitable_trade_count_map: self.profitable_trade_count_map.clone(),
            profit_bp_ema_map: self.profit_bp_ema_map.clone(),
            last_update: self.last_update.clone(),
            last_position_updated_time_ms: self.last_position_updated_time_ms.clone(),
            symbols: self.symbols.clone(),
            close_symbols: self.close_symbols.clone(),
            order_size: self.order_size.clone(),
            futures_account_data: Arc::new(ArcSwap::from_pointee(None)),
            data_cache: None, // Don't clone for background thread (not needed for REST API updates)
            is_healthy: Arc::clone(&self.is_healthy),
            gate_rate_limit: self.gate_rate_limit.clone(),
            last_too_many_orders_time_ms: Arc::clone(&self.last_too_many_orders_time_ms),
            limit_open_blocked_symbols: self.limit_open_blocked_symbols.clone(),
            dangerous_limit_open_symbols: self.dangerous_limit_open_symbols.clone(),
            market_close_symbols: self.market_close_symbols.clone(),
            trade_callback: Arc::new(RwLock::new(None)), // Background thread doesn't need callback
            trade_settings: self.trade_settings.clone(),
            last_orders: Arc::clone(&self.last_orders),
            last_ws_orders: Arc::clone(&self.last_ws_orders),
            recent_orders_queue: self.recent_orders_queue.clone(),
            order_record_tx: self.order_record_tx.clone(),
            ws_sender: self.ws_sender.clone(),
            last_trade_time_map: self.last_trade_time_map.clone(),
            ws_order_scale: self.ws_order_scale,
            save_metrics: self.save_metrics,
            update_futures_account_data: self.update_futures_account_data,
            run_health_check: self.run_health_check,
            run_small_orders_loop: self.run_small_orders_loop,
            print_orders: self.print_orders,
            login_status_url_template: self.login_status_url_template.clone(),
            order_manager_client: None, // clone_for_update doesn't need it
            is_monitoring: self.is_monitoring,
            total_net_positions_usdt_size_all_accounts: self
                .total_net_positions_usdt_size_all_accounts
                .clone(),
        }
    }

    /// Get position size (contract units) — lock-free.
    pub fn get_position_size(&self, symbol: &str) -> i64 {
        self.positions
            .load()
            .get(symbol)
            .map(|p| p.size)
            .unwrap_or(0)
    }

    /// Get USDT position size for a symbol — lock-free (positions + contracts ArcSwap).
    pub fn get_usdt_position_size(&self, symbol: &str) -> f64 {
        let positions = self.positions.load();
        let contracts = self.contracts.load();

        if let Some(position) = positions.get(symbol) {
            if let Some(contract) = contracts.get(symbol) {
                let size = position.size as f64;
                let mark_price: f64 = contract.mark_price.parse().unwrap_or(0.0);
                let quanto_multiplier: f64 = contract.quanto_multiplier.parse().unwrap_or(1.0);
                return size * mark_price * quanto_multiplier;
            }
        }

        0.0
    }

    /// Get order count size (position size / order_size).
    /// Returns `None` when no position/size.
    pub fn get_order_count_size(&self, symbol: &str) -> Option<f64> {
        let usdt_position_size = self.get_usdt_position_size(symbol);
        let order_size = **self.order_size.load();
        if usdt_position_size == 0.0 || order_size == 0.0 {
            return Some(0.0);
        }
        Some(usdt_position_size / order_size)
    }

    /// Get quanto multiplier — lock-free (contracts).
    pub fn get_quanto_multiplier(&self, symbol: &str) -> f64 {
        self.contracts
            .load()
            .get(symbol)
            .and_then(|c| c.quanto_multiplier.parse().ok())
            .unwrap_or(1.0)
    }

    /// Get funding rate — lock-free (contracts).
    pub fn get_funding_rate(&self, symbol: &str) -> f64 {
        self.contracts
            .load()
            .get(symbol)
            .and_then(|c| c.funding_rate.parse().ok())
            .unwrap_or(0.0)
    }

    /// Get contract info — lock-free load; write never blocks read.
    pub fn get_contract(&self, symbol: &str) -> Option<GateContract> {
        self.contracts.load().get(symbol).cloned()
    }

    /// One-shot snapshot for strategy: read contracts (lock-free), order_size, positions, futures_account_data.
    /// Returns None if contract missing; order_size/positions are lock-free.
    pub fn try_snapshot_for_symbol(&self, symbol: &str) -> Option<StrategySymbolSnapshot> {
        let contracts = self.contracts.load();
        let contract = contracts.get(symbol).cloned()?;
        let order_size = Some(**self.order_size.load());
        let positions = self.positions.load();
        let pos = positions.get(symbol);
        let usdt_position_size = pos
            .and_then(|p| {
                let size = p.size as f64;
                let mark_price: f64 = contract.mark_price.parse().unwrap_or(0.0);
                let qm: f64 = contract.quanto_multiplier.parse().unwrap_or(1.0);
                Some(size * mark_price * qm)
            })
            .unwrap_or(0.0);
        let avg_entry_price = pos.and_then(|p| {
            if p.entry_price > 0.0 {
                Some(p.entry_price)
            } else {
                None
            }
        });
        let order_count_size = order_size.and_then(|os| {
            if usdt_position_size == 0.0 || os == 0.0 {
                Some(0.0)
            } else {
                Some(usdt_position_size / os)
            }
        });
        let futures_account_data = self.futures_account_data.load().as_ref().as_ref().map(|d| {
            let total: f64 = d.total.parse().unwrap_or(0.0);
            let pnl: f64 = d.unrealised_pnl.parse().unwrap_or(0.0);
            (total, pnl)
        });
        Some(StrategySymbolSnapshot {
            contract,
            order_size,
            order_count_size,
            usdt_position_size,
            avg_entry_price,
            futures_account_data,
        })
    }

    /// Check if has position (lock-free)
    pub fn has_position(&self, symbol: &str) -> bool {
        self.positions
            .load()
            .get(symbol)
            .map(|p| p.size != 0)
            .unwrap_or(false)
    }

    /// Get position entry price (average entry) — lock-free
    pub fn get_position_entry_price(&self, symbol: &str) -> Option<f64> {
        self.positions.load().get(symbol).and_then(|p| {
            if p.entry_price > 0.0 {
                Some(p.entry_price)
            } else {
                None
            }
        })
    }

    /// Increment order count (called when order is placed)
    pub fn increment_order_count(&self, symbol: &str) {
        let mut map = self.order_count_map.write();
        *map.entry(symbol.to_string()).or_insert(0) += 1;
    }

    /// Get trade count — try_read: no wait
    /// Get trade count — lock-free
    pub fn get_trade_count(&self, symbol: &str) -> i64 {
        self.trade_count_map
            .load()
            .get(symbol)
            .copied()
            .unwrap_or(0)
    }

    /// Get profitable trade count — lock-free
    pub fn get_profitable_trade_count(&self, symbol: &str) -> i64 {
        self.profitable_trade_count_map
            .load()
            .get(symbol)
            .copied()
            .unwrap_or(0)
    }

    /// Get minimum order size for a contract — lock-free (contracts).
    pub fn get_min_order_size(&self, symbol: &str) -> i64 {
        self.contracts
            .load()
            .get(symbol)
            .map(|c| c.order_size_min as i64)
            .unwrap_or(1)
    }

    /// Convert USDT amount to contract size — lock-free (contracts).
    pub fn get_size_from_usdt_amount(&self, symbol: &str, usdt_amount: f64) -> i64 {
        let contracts = self.contracts.load();
        let Some(contract) = contracts.get(symbol) else {
            return 0;
        };
        let mut mark_price: f64 = contract.mark_price.parse().unwrap_or(0.0);
        let quanto_multiplier: f64 = contract.quanto_multiplier.parse().unwrap_or(1.0);
        if mark_price <= 0.0 {
            if let Some(cache) = self.data_cache.as_ref() {
                if let Some(bt) = cache.get_gate_bookticker(symbol) {
                    let mid = 0.5 * (bt.bid_price + bt.ask_price);
                    if mid > 0.0 {
                        mark_price = mid;
                    }
                }
            }
        }
        if mark_price == 0.0 {
            return 0;
        }
        let num_tokens = usdt_amount / mark_price;
        (num_tokens / quanto_multiplier) as i64
    }

    /// Convert contract size to USDT amount — lock-free (contracts).
    pub fn get_usdt_amount_from_size(&self, symbol: &str, size: i64) -> f64 {
        let contracts = self.contracts.load();
        contracts
            .get(symbol)
            .map(|contract| {
                let mut mark_price: f64 = contract.mark_price.parse().unwrap_or(0.0);
                let quanto_multiplier: f64 = contract.quanto_multiplier.parse().unwrap_or(1.0);
                // Flipster mode: synthetic contracts start with mark_price=0; fall back to
                // live api_bt mid from data_cache so usdt conversion isn't stuck at zero.
                if mark_price <= 0.0 {
                    if let Some(cache) = self.data_cache.as_ref() {
                        if let Some(bt) = cache.get_gate_bookticker(symbol) {
                            let mid = 0.5 * (bt.bid_price + bt.ask_price);
                            if mid > 0.0 {
                                mark_price = mid;
                            }
                        }
                    }
                }
                (size as f64) * mark_price * quanto_multiplier
            })
            .unwrap_or(0.0)
    }

    /// Get order count — try_read: no wait
    pub fn get_order_count(&self, symbol: &str) -> i64 {
        self.order_count_map
            .try_read()
            .map(|g| *g.get(symbol).unwrap_or(&0))
            .unwrap_or(0)
    }

    pub fn get_profit_bp_ema(&self, symbol: &str) -> f64 {
        self.profit_bp_ema_map
            .try_read()
            .map(|g| *g.get(symbol).unwrap_or(&0.0))
            .unwrap_or(0.0)
    }

    /// Check if symbol is in this account's symbol list — try_read: no wait
    pub fn is_account_symbol(&self, symbol: &str) -> bool {
        let max_position_size = self.trade_settings.load().max_position_size;

        if max_position_size > 0.0 {
            return self
                .symbols
                .try_read()
                .map(|g| g.iter().any(|s| s == symbol))
                .unwrap_or(false)
                || self.has_position(symbol);
        } else {
            return self.has_position(symbol);
        }
    }

    /// Get order size (for handle_chance) — lock-free.
    pub fn get_order_size(&self) -> Option<f64> {
        Some(**self.order_size.load())
    }

    /// Get symbols list (for handle_chance) — try_read: no wait
    pub fn get_symbols(&self) -> Vec<String> {
        self.symbols
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn get_position(&self, symbol: &str) -> Option<GatePosition> {
        self.positions.load().get(symbol).cloned()
    }

    /// All open orders (status "open"); lock-free read.
    pub fn get_open_orders(&self) -> HashMap<String, GateOpenOrder> {
        (**self.open_orders.load()).clone()
    }

    /// Single open order by id.
    pub fn get_open_order(&self, order_id: &str) -> Option<GateOpenOrder> {
        self.open_orders.load().get(order_id).cloned()
    }

    /// Check if account is healthy — try_read: no wait.
    /// Returns `None` when lock is busy (caller should treat as unknown, e.g. don't skip orders).
    pub fn is_healthy(&self) -> Option<bool> {
        self.is_healthy.try_read().map(|g| *g)
    }

    /// Count non-zero positions for state reporting — lock-free
    pub fn num_positions(&self) -> usize {
        self.positions
            .load()
            .values()
            .filter(|p| p.size != 0)
            .count()
    }

    /// Get uid for order payload (same as Python uses)
    pub fn get_uid(&self) -> i64 {
        self.uid as i64
    }

    pub fn cancel_order(&self, order_id: &str) -> Result<(), String> {
        if self.is_monitoring {
            debug!(
                "[GateOrderManager {}] Cancelling order is not allowed in monitoring mode: order_id={}",
                self.login_name, order_id
            );
            return Ok(());
        }

        // Flipster mode: HTTP DELETE to /api/v2/trade/orders/{sym}/{order_id}
        if crate::flipster_cookie_store::global().is_some() {
            return self.flipster_cancel_order_http(order_id);
        }

        let guard = self.ws_sender.read();
        let handle = guard
            .as_ref()
            .ok_or_else(|| "WebSocket not connected".to_string())?
            .clone();
        drop(guard);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let req_id = format!("{}-{}-7", timestamp % 1000000, self.login_name);

        let request = json!({
            "time": timestamp,
            "channel": "futures.order_cancel",
            "event": "api",
            "payload": {
                "req_id": req_id,
                "req_param": {
                    "order_id": order_id,
                }
            }
        });

        if !handle.push_message(Message::Text(request.to_string().into())) {
            *self.ws_sender.write() = None;
            return Err("WS queue full".to_string());
        }
        Ok(())
    }

    /// Flipster-specific: HTTP DELETE.
    fn flipster_cancel_order_http(&self, order_id: &str) -> Result<(), String> {
        // Resolve contract from open_orders cache (Flipster URL requires symbol).
        let contract = match self.open_orders.load().get(order_id) {
            Some(o) => o.contract.clone(),
            None => return Ok(()), // order already gone
        };
        self.flipster_cancel_http(&contract, order_id)
    }

    fn flipster_place_order_http(
        &self,
        contract: &str,
        price: f64,
        size: i64,
        reduce_only: bool,
        tif: &str,
        side: Option<&str>,
    ) -> Result<(), String> {
        let store = crate::flipster_cookie_store::global()
            .ok_or_else(|| "cookie store not init".to_string())?;
        let side_str = match side {
            Some("buy") => "Long",
            Some("sell") => "Short",
            _ => {
                if size > 0 {
                    "Long"
                } else {
                    "Short"
                }
            }
        };
        let order_type = if price == 0.0 {
            "ORDER_TYPE_MARKET"
        } else {
            "ORDER_TYPE_LIMIT"
        };
        let post_only = tif.eq_ignore_ascii_case("poc");
        // Convert signed contract size back to Flipster USDT amount via mark price.
        let mark_price = self
            .contracts
            .load()
            .get(contract)
            .and_then(|c| c.mark_price.parse::<f64>().ok())
            .unwrap_or(price);
        let qty_coins = (size as f64).abs() / crate::flipster_private_ws::POSITION_SIZE_SCALE;
        let usdt_amount = qty_coins * mark_price.max(price);
        let body = json!({
            "side": side_str,
            "requestId": uuid::Uuid::new_v4().to_string(),
            "timestamp": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos().to_string(),
            "reduceOnly": reduce_only,
            "refServerTimestamp": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos().to_string(),
            "refClientTimestamp": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos().to_string(),
            "leverage": 10,
            "price": format!("{}", price),
            "amount": format!("{:.4}", usdt_amount),
            "marginType": "Cross",
            "orderType": order_type,
            "postOnly": post_only,
        });
        let url = format!(
            "https://api.flipster.io/api/v2/trade/one-way/order/{}",
            contract
        );
        let resp = self
            .client
            .post(&url)
            .header("Cookie", store.cookie_header())
            .header(
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64) Chrome/141.0.0.0",
            )
            .header("Origin", "https://flipster.io")
            .header("Referer", "https://flipster.io/")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .map_err(|e| format!("flipster place http: {}", e))?;
        if resp.status().is_success() {
            return Ok(());
        }
        Err(format!(
            "flipster place {}: {}",
            resp.status().as_u16(),
            resp.text().unwrap_or_default()
        ))
    }

    fn flipster_cancel_http(&self, contract: &str, order_id: &str) -> Result<(), String> {
        let store = crate::flipster_cookie_store::global()
            .ok_or_else(|| "cookie store not init".to_string())?;
        let url = format!(
            "https://api.flipster.io/api/v2/trade/orders/{}/{}",
            contract, order_id
        );
        let body = json!({
            "requestId": uuid::Uuid::new_v4().to_string(),
            "timestamp": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                .to_string(),
            "attribution": "TRADE_PERPETUAL_DEFAULT",
        });
        let resp = self
            .client
            .delete(&url)
            .header("Cookie", store.cookie_header())
            .header(
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64) Chrome/141.0.0.0",
            )
            .header("Origin", "https://flipster.io")
            .header("Referer", "https://flipster.io/")
            .json(&body)
            .send()
            .map_err(|e| format!("flipster cancel http: {}", e))?;
        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            return Ok(());
        }
        let text = resp.text().unwrap_or_default();
        if text.contains("AlreadyFilled") || text.contains("NotFound") {
            return Ok(());
        }
        Err(format!("flipster cancel {}: {}", status.as_u16(), text))
    }

    /// Cancel duplicate open orders: for each (contract, side), keep only one order (oldest by create_time_ms) and cancel the rest.
    pub fn cancel_duplicate_orders(&self) -> Result<(), String> {
        let orders = self.get_open_orders();
        if orders.is_empty() {
            return Ok(());
        }
        type Key = (String, String); // (contract, side)
        let mut by_contract_side: HashMap<Key, Vec<GateOpenOrder>> = HashMap::new();
        for order in orders.values() {
            let side = if order.size > 0 { "buy" } else { "sell" };
            by_contract_side
                .entry((order.contract.clone(), side.to_string()))
                .or_default()
                .push(order.clone());
        }
        for (_key, mut group) in by_contract_side {
            if group.len() <= 1 {
                continue;
            }
            group.sort_by_key(|o| o.create_time_ms);
            for order in group.into_iter().skip(1) {
                if let Err(e) = self.cancel_order(&order.id) {
                    return Err(format!(
                        "cancel_duplicate_orders: failed to cancel order {} ({}): {}",
                        order.id, order.contract, e
                    ));
                }
                info!(
                    "[GateOrderManager {}] Cancelled duplicate order: id={} contract={} side={}",
                    self.login_name,
                    order.id,
                    order.contract,
                    if order.size > 0 { "buy" } else { "sell" }
                );
            }
        }
        Ok(())
    }

    /// Get valid order price with proper rounding (same as Python _get_valid_order_price)
    /// Python: ROUND_HALF_UP by default, ROUND_UP for sell, ROUND_DOWN for buy
    /// Python quantize: Decimal(price).quantize(Decimal(order_price_round), rounding=rounding)
    pub fn get_valid_order_price(&self, contract: &str, price: f64, side: Option<&str>) -> String {
        if price == 0.0 {
            return "0".to_string();
        }

        let contract_info = self.get_contract(contract);
        if contract_info.is_none() {
            warn!(
                "[GateOrderManager] Contract not found for {}: {}",
                self.login_name, contract
            );
            return price.to_string();
        }

        let contract = contract_info.unwrap();

        // Parse order_price_round as Decimal, then force to 1/10 of the original.
        let original_order_price_round: Decimal = contract
            .order_price_round
            .parse()
            .unwrap_or_else(|_| Decimal::from_str("0.0000000001").unwrap());
        let order_price_round = original_order_price_round / Decimal::from(10);

        if order_price_round == Decimal::ZERO {
            return price.to_string();
        }

        // Convert price to Decimal
        // Use from_str_exact or from_str to handle f64 conversion properly
        let price_decimal = Decimal::from_str_exact(&format!("{:.10}", price))
            .or_else(|_| Decimal::from_str(&price.to_string()))
            .unwrap_or_else(|_| Decimal::ZERO);

        // Quantize: divide by step, round, then multiply back
        // This mimics Python's Decimal.quantize() behavior
        let divided = price_decimal / order_price_round;

        // Round based on side (same as Python)
        // rust_decimal uses round_dp_with_strategy for rounding strategies
        let rounded_quotient = match side {
            Some("sell") => {
                // ROUND_UP: always round up (ceil)
                divided.ceil()
            }
            Some("buy") => {
                // ROUND_DOWN: always round down (floor)
                divided.floor()
            }
            _ => {
                // ROUND_HALF_UP: round half up (default)
                divided.round_dp(0)
            }
        };

        // Multiply back by order_price_round to get quantized price
        let rounded = rounded_quotient * order_price_round;

        // Format to avoid scientific notation and remove trailing zeros
        // Use to_string() which will format properly
        rounded.to_string()
    }

    /// Cancel orders via WebSocket (same as Python cancel_websocket_orders_matched)
    pub fn cancel_websocket_orders_matched(
        &self,
        contract: Option<&str>,
        side: Option<&str>,
    ) -> Result<(), String> {
        if self.is_monitoring {
            debug!(
                "[GateOrderManager {}] Cancelling orders via WebSocket is not allowed in monitoring mode: contract={}, side={}",
                self.login_name, contract.unwrap_or("None"), side.unwrap_or("None")
            );
            return Ok(());
        }

        // Flipster mode: iterate matching orders in open_orders cache, HTTP DELETE each.
        if crate::flipster_cookie_store::global().is_some() {
            let orders = (**self.open_orders.load()).clone();
            for (oid, order) in orders {
                if let Some(c) = contract {
                    if order.contract != c {
                        continue;
                    }
                }
                if let Some(s) = side {
                    let order_side = if order.size > 0 { "buy" } else { "sell" };
                    if order_side != s && s != "bid" && s != "ask" {
                        continue;
                    }
                    if (s == "bid" && order.size < 0) || (s == "ask" && order.size > 0) {
                        continue;
                    }
                }
                if let Err(e) = self.flipster_cancel_http(&order.contract, &oid) {
                    warn!("[flipster-cancel] {} {}: {}", order.contract, oid, e);
                }
            }
            return Ok(());
        }

        let guard = self.ws_sender.read();
        let handle = guard
            .as_ref()
            .ok_or_else(|| "WebSocket not connected".to_string())?
            .clone();
        drop(guard);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let mut req_param = serde_json::Map::new();
        if let Some(contract) = contract {
            req_param.insert("contract".to_string(), json!(contract));
        }
        let valid_side = if side == Some("buy") {
            Some("bid")
        } else if side == Some("sell") {
            Some("ask")
        } else if side == Some("bid") {
            Some("bid")
        } else if side == Some("ask") {
            Some("ask")
        } else {
            None
        };
        if let Some(side) = valid_side {
            req_param.insert("side".to_string(), json!(side));
        }

        // Generate req_id: last 6 digits of timestamp + login_name + "-4"
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let req_id = format!("{}-{}-4", timestamp_ms % 1000000, self.login_name);

        let request = json!({
            "time": timestamp,
            "channel": "futures.order_cancel_cp",
            "event": "api",
            "payload": {
                "req_id": req_id,
                "req_param": req_param,
            }
        });

        if !handle.push_message(Message::Text(request.to_string().into())) {
            *self.ws_sender.write() = None;
            return Err("WS queue full".to_string());
        }
        Ok(())
    }

    /// Place order via WebSocket (same as Python place_websocket_order)
    pub fn place_websocket_order(
        &self,
        contract: &str,
        price: f64,
        size: i64,
        reduce_only: bool,
        tif: &str,
        side: Option<&str>,
        req_id: Option<&str>,
    ) -> Result<(), String> {
        // Flipster has no WS-based order placement — route through HTTP (same path as send_order).
        if crate::flipster_cookie_store::global().is_some() {
            return self.flipster_place_order_http(contract, price, size, reduce_only, tif, side);
        }

        let guard = self.ws_sender.read();
        let handle = guard
            .as_ref()
            .ok_or_else(|| "WebSocket not connected".to_string())?
            .clone();
        drop(guard);

        // Apply ws_order_scale to size (same as Python)
        let scaled_size = if let Some("buy") = side {
            (size as f64 * self.ws_order_scale).abs() as i64
        } else if let Some("sell") = side {
            -((size as f64 * self.ws_order_scale).abs() as i64)
        } else {
            (size as f64 * self.ws_order_scale) as i64
        };

        // Get valid order price with rounding
        let valid_price = self.get_valid_order_price(contract, price, side);

        // Generate req_id if not provided
        let req_id = req_id.map(|s| s.to_string()).unwrap_or_else(|| {
            let timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis();
            format!("{}-{}-2", timestamp_ms % 1000000, self.login_name)
        });

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let request = json!({
            "time": timestamp,
            "channel": "futures.order_place",
            "event": "api",
            "payload": {
                "req_id": req_id,
                "req_param": {
                    "contract": contract,
                    "price": valid_price,
                    "reduce_only": reduce_only,
                    "size": scaled_size,
                    "tif": tif,
                },
                "req_header": {
                    "x-gate-exptime": (timestamp_ms + 500).to_string(),
                }
            }
        });

        if !handle.push_message(Message::Text(request.to_string().into())) {
            *self.ws_sender.write() = None;
            return Err("WS queue full".to_string());
        }
        Ok(())
    }

    /// Cancel all orders via WebSocket and REST API (same as Python cancel_all_orders)
    pub fn cancel_all_orders(&self) -> Result<(), String> {
        // Flipster mode: iterate open_orders and HTTP DELETE each. Repeat until
        // private WS reports empty open_orders.
        if crate::flipster_cookie_store::global().is_some() {
            for _ in 0..30 {
                let orders = (**self.open_orders.load()).clone();
                if orders.is_empty() {
                    return Ok(());
                }
                for (oid, order) in orders {
                    if let Err(e) = self.flipster_cancel_http(&order.contract, &oid) {
                        warn!("[flipster-cancel-all] {} {}: {}", order.contract, oid, e);
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            return Ok(());
        }

        // First cancel via WebSocket
        if let Err(e) = self.cancel_websocket_orders_matched(None, None) {
            warn!(
                "[GateOrderManager {}] Failed to cancel orders via WebSocket: {}",
                self.login_name, e
            );
        }

        loop {
            std::thread::sleep(Duration::from_millis(100));

            // Cancel via WebSocket again
            if let Err(e) = self.cancel_websocket_orders_matched(None, None) {
                warn!(
                    "[GateOrderManager {}] Failed to cancel orders via WebSocket: {}",
                    self.login_name, e
                );
            }

            // List open orders via REST API
            let url = format!("{}/orders", GATE_FUTURES_API_BASE);
            let request_url = "/api/v4/futures/usdt/orders";

            let headers =
                match self.build_auth_headers("GET", request_url, Some("status=open"), None) {
                    Ok(h) => h,
                    Err(e) => {
                        error!(
                            "[GateOrderManager {}] Failed to build auth headers: {}",
                            self.login_name, e
                        );
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                };

            let response = match self
                .client
                .get(&url)
                .query(&[("status", "open")])
                .headers({
                    let mut h = reqwest::header::HeaderMap::new();
                    for (k, v) in &headers {
                        h.insert(
                            reqwest::header::HeaderName::from_bytes(k.as_bytes())
                                .map_err(|e| format!("Invalid header name: {}", e))?,
                            reqwest::header::HeaderValue::from_str(v)
                                .map_err(|e| format!("Invalid header value: {}", e))?,
                        );
                    }
                    h
                })
                .send()
            {
                Ok(r) => r,
                Err(e) => {
                    error!(
                        "[GateOrderManager {}] Error listing orders: {}",
                        self.login_name, e
                    );
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };

            if !response.status().is_success() {
                error!(
                    "[GateOrderManager {}] Failed to list orders: status {}",
                    self.login_name,
                    response.status()
                );
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }

            let orders: Vec<serde_json::Value> = match response.json() {
                Ok(o) => o,
                Err(e) => {
                    error!(
                        "[GateOrderManager {}] Failed to parse orders response: {}",
                        self.login_name, e
                    );
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };

            if orders.is_empty() {
                break; // All orders cancelled
            }

            // Cancel each order
            for order in orders {
                if let Some(order_id) = order.get("id").and_then(|v| v.as_str()) {
                    let cancel_url = format!("{}/orders/{}", GATE_FUTURES_API_BASE, order_id);
                    let cancel_request_url = format!("/api/v4/futures/usdt/orders/{}", order_id);

                    let cancel_headers = match self.build_auth_headers(
                        "DELETE",
                        &cancel_request_url,
                        None,
                        None,
                    ) {
                        Ok(h) => h,
                        Err(e) => {
                            error!(
                                "[GateOrderManager {}] Failed to build cancel headers for order {}: {}",
                                self.login_name, order_id, e
                            );
                            continue;
                        }
                    };

                    if let Err(e) = self
                        .client
                        .delete(&cancel_url)
                        .headers({
                            let mut h = reqwest::header::HeaderMap::new();
                            for (k, v) in &cancel_headers {
                                h.insert(
                                    reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                                    reqwest::header::HeaderValue::from_str(v).unwrap(),
                                );
                            }
                            h
                        })
                        .send()
                    {
                        error!(
                            "[GateOrderManager {}] Error canceling order {}: {}",
                            self.login_name, order_id, e
                        );
                    }
                }
            }

            std::thread::sleep(Duration::from_millis(100));
        }

        Ok(())
    }

    /// Place order via simple-order endpoint (wrapper around OrderManagerClient)
    /// This mirrors Python OrderManagerV3.place_order behavior (HTTP simple-order, not REST)
    pub fn place_order(
        &self,
        order_manager_client: &crate::order_manager_client::OrderManagerClient,
        req: crate::order_manager_client::OrderRequest,
    ) -> Result<PlaceOrderResponse, String> {
        // if req.level == "limit_open" {
        //     let position_size = self.get_position_size(&req.symbol);
        //     if (position_size <= 0 && req.side == "sell")
        //         || (position_size >= 0 && req.side == "buy")
        //     {
        //         if req.level == "limit_open" && self.is_limit_open_blocked(&req.symbol) {
        //             warn!(
        //                 "[GateOrderManager {}] limit_open blocked for {} (limit_open_blocked_symbols)",
        //                 self.login_name, req.symbol
        //             );
        //             return Err(format!(
        //                 "limit_open blocked for {} (limit_open_blocked_symbols)",
        //                 req.symbol
        //             ));
        //         }
        //     }
        // }
        let response = order_manager_client.send_order(req, false);
        match response {
            Ok(response) => Ok(response),
            Err(e) => Err(e),
        }
    }

    pub fn is_recently_too_many_orders(&self) -> bool {
        let last = self
            .last_too_many_orders_time_ms
            .try_read()
            .map(|g| *g)
            .unwrap_or(0);
        let current_ts_ms = chrono::Utc::now().timestamp_millis();
        current_ts_ms - last < 60 * 60 * 1000 // 1 hour
    }

    /// Save metrics state to local file (static version for use in background thread)
    fn save_metrics_to_file_static(
        login_name: &str,
        order_count_map: &Arc<RwLock<HashMap<String, i64>>>,
        trade_count_map: &Arc<ArcSwap<HashMap<String, i64>>>,
        profitable_trade_count_map: &Arc<ArcSwap<HashMap<String, i64>>>,
        profit_bp_ema_map: &Arc<RwLock<HashMap<String, f64>>>,
    ) -> Result<(), std::io::Error> {
        use std::fs;
        use std::io::Write;

        // Get metrics data
        let order_counts: HashMap<String, i64> = {
            let map = order_count_map.read();
            map.clone()
        };
        let trade_counts: HashMap<String, i64> = (**trade_count_map.load()).clone();
        let profitable_trade_counts: HashMap<String, i64> =
            (**profitable_trade_count_map.load()).clone();
        let profit_bp_emas: HashMap<String, f64> = {
            let map = profit_bp_ema_map.read();
            map.clone()
        };

        // Create metrics data structure
        let metrics_data = json!({
            "login_name": login_name,
            "timestamp": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            "order_counts": order_counts,
            "trade_counts": trade_counts,
            "profitable_trade_counts": profitable_trade_counts,
            "profit_bp_emas": profit_bp_emas,
        });

        // Determine file path
        let metrics_dir = std::env::var("METRICS_DIR").unwrap_or_else(|_| "./metrics".to_string());
        if !std::path::Path::new(&metrics_dir).exists() {
            // create directory
            std::fs::create_dir_all(&metrics_dir)?;
        }
        let filename = format!("{}/metrics_{}.json", &metrics_dir, &login_name);

        // Create directory if it doesn't exist
        if let Some(parent) = std::path::Path::new(&filename).parent() {
            fs::create_dir_all(parent)?;
        }

        // Write to file atomically (write to temp file then rename)
        let temp_filename = format!("{}.tmp", filename);
        let mut file = fs::File::create(&temp_filename)?;
        file.write_all(serde_json::to_string_pretty(&metrics_data)?.as_bytes())?;
        file.sync_all()?;
        drop(file);

        // Atomic rename
        fs::rename(&temp_filename, &filename)?;

        trace!(
            "[GateOrderManager {}] Saved metrics to {}",
            login_name,
            filename
        );
        Ok(())
    }

    fn load_metrics_from_file_static(login_name: &str) -> Result<Value, std::io::Error> {
        let metrics_dir = std::env::var("METRICS_DIR").unwrap_or_else(|_| "./metrics".to_string());
        if !std::path::Path::new(&metrics_dir).exists() {
            // create directory
            std::fs::create_dir_all(&metrics_dir)?;
        }
        let filename = format!("{}/metrics_{}.json", &metrics_dir, &login_name);
        let file = std::fs::File::open(filename)?;
        if let Ok(metrics_data) = serde_json::from_reader::<_, serde_json::Map<String, Value>>(file)
        {
            return Ok(serde_json::Value::Object(metrics_data));
        }
        Ok(serde_json::Value::Null)
    }
    pub fn get_status_url(&self) -> String {
        let mut status_url = self.login_status_url_template.clone();
        if status_url.contains("{login_name}") {
            status_url = status_url.replace("{login_name}", &self.login_name);
        }
        status_url
    }

    pub fn check_login_status(&self) -> bool {
        // Flipster mode: liveness = cookie store initialized + private WS is_healthy flag.
        if let Some(store) = crate::flipster_cookie_store::global() {
            let _ = store; // presence is the signal
            return *self.is_healthy.read();
        }

        let status_url = self.get_status_url();
        let response = match self.client.get(&status_url).send() {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to check login status: {}", e);
                return false;
            }
        };
        if !response.status().is_success() {
            return false;
        }
        let json: Value = response.json().unwrap();
        let logged_in = json
            .get("isLoggedIn")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        logged_in
    }

    /// Start health check loop (runs every 300 seconds, same as Python)
    pub fn start_health_check(&self) {
        let login_name = self.login_name.clone();
        let is_healthy = self.is_healthy.clone();
        let health_check_try_count = Arc::new(RwLock::new(0));
        let last_too_many_orders_time_ms = Arc::clone(&self.last_too_many_orders_time_ms);
        let uid = self.get_uid();
        let status_url = self.get_status_url();
        let order_client = self
            .order_manager_client
            .clone()
            .unwrap_or_else(|| crate::order_manager_client::OrderManagerClient::instance());
        let hc_client = self.client.clone();

        std::thread::spawn(move || {
            let run_once = || {
                // Flipster mode: private WS's subscribe already sets is_healthy=true.
                // Skip gate's puppeteer-based health check entirely (login-status URL
                // is local and returns 404 here, which would permanently mark unhealthy).
                if crate::flipster_cookie_store::global().is_some() {
                    *is_healthy.write() = true;
                    return;
                }
                // Login status check via puppeteer service (no test order)
                info!(
                    "[GateOrderManager {}] Health check login-status URL: {}",
                    login_name, status_url
                );

                match hc_client.get(&status_url).send() {
                    Ok(resp) => {
                        let status = resp.status();
                        if !status.is_success() {
                            let body = resp.text().unwrap_or_else(|_| "Unknown error".to_string());
                            *is_healthy.write() = false;
                            debug!("\x1b[93m{}\x1b[0m - \x1b[41m\x1b[37mHealth check login-status HTTP {} body: {}\x1b[0m", login_name, status, body);
                            return;
                        }
                        let json: Value = match resp.json() {
                            Ok(v) => v,
                            Err(e) => {
                                *is_healthy.write() = false;
                                error!("\x1b[93m{}\x1b[0m - \x1b[41m\x1b[37mHealth check login-status parse error: {}\x1b[0m", login_name, e);
                                return;
                            }
                        };

                        let logged_in = json
                            .get("isLoggedIn")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);

                        if logged_in {
                            *is_healthy.write() = true;
                            info!(
                                "\x1b[93m{}\x1b[0m - \x1b[92mHealthy (login-status ok)\x1b[0m",
                                login_name
                            );
                        } else {
                            *is_healthy.write() = false;
                            error!("\x1b[93m{}\x1b[0m - \x1b[41m\x1b[37mHealth check login-status: not logged in\x1b[0m {:?}", login_name, json);
                        }
                    }
                    Err(e) => {
                        // with emoji red cross
                        error!(
                            "\x1b[91m[GateOrderManager {}] ❌ Health check login-status error: {}\x1b[0m",
                            login_name, e
                        );
                        *is_healthy.write() = false;
                    }
                }

                // first set healthy to false
                *is_healthy.write() = false;

                let result = order_client
                    .send_order(
                        crate::order_manager_client::OrderRequest {
                            login_name: login_name.clone(),
                            symbol: "BTC_USDT".to_string(),
                            side: "buy".to_string(),
                            size: 1,
                            price: "40000.0".to_string(),
                            tif: "ioc".to_string(),
                            level: "limit_open".to_string(),
                            if_not_healthy_ws: false,
                            only_reduce_only_for_ws: false,
                            bypass_safe_limit_close: true,
                            uid: uid,
                            usdt_size: 40000.0,
                        },
                        true,
                    )
                    .is_ok();
                if result {
                    // with emoji green checkmark
                    info!(
                        "\x1b[92m[GateOrderManager {}] 🟢 Health check order sent successfully\x1b[0m",
                        login_name
                    );
                } else {
                    error!(
                        "\x1b[91m[GateOrderManager {}] 🔴 Health check order failed\x1b[0m",
                        login_name
                    );
                    *last_too_many_orders_time_ms.write() = chrono::Utc::now().timestamp_millis();
                }
            };

            // Run immediately once at startup (align with Python behavior)
            run_once();

            // sleep for 15 seconds
            std::thread::sleep(Duration::from_secs(15));

            // Run immediately once at startup (align with Python behavior)
            run_once();

            loop {
                std::thread::sleep(Duration::from_secs(5));

                if !*is_healthy.read() {
                    // max 10
                    let sleep_multiplier = std::cmp::min(10, *health_check_try_count.read());
                    let sleep_seconds = 60 * sleep_multiplier;
                    *health_check_try_count.write() += 1;
                    std::thread::sleep(Duration::from_secs(sleep_seconds));
                } else {
                    *health_check_try_count.write() = 0;
                    std::thread::sleep(Duration::from_secs(60 * 10));
                }
                run_once();
            }
        });
    }
}

// Implement Clone manually (Client doesn't implement Clone, but we can work around it)
impl Clone for GateOrderManager {
    fn clone(&self) -> Self {
        Self {
            login_name: self.login_name.clone(),
            api_key: self.api_key.clone(),
            api_secret: self.api_secret.clone(),
            uid: self.uid,
            client: self.client.clone(),
            positions: self.positions.clone(),
            contracts: self.contracts.clone(),
            open_orders: self.open_orders.clone(),
            open_orders_by_contract_cache: Arc::new(RwLock::new((None, HashMap::new()))),
            order_count_map: self.order_count_map.clone(),
            trade_count_map: self.trade_count_map.clone(),
            profitable_trade_count_map: self.profitable_trade_count_map.clone(),
            profit_bp_ema_map: self.profit_bp_ema_map.clone(),
            last_update: self.last_update.clone(),
            last_position_updated_time_ms: self.last_position_updated_time_ms.clone(),
            symbols: self.symbols.clone(),
            close_symbols: self.close_symbols.clone(),
            order_size: self.order_size.clone(),
            futures_account_data: Arc::new(ArcSwap::from_pointee(None)),
            data_cache: self.data_cache.clone(), // Clone data_cache for Clone trait
            is_healthy: self.is_healthy.clone(), // Clone is_healthy
            gate_rate_limit: self.gate_rate_limit.clone(),
            last_too_many_orders_time_ms: self.last_too_many_orders_time_ms.clone(),
            limit_open_blocked_symbols: self.limit_open_blocked_symbols.clone(),
            dangerous_limit_open_symbols: self.dangerous_limit_open_symbols.clone(),
            market_close_symbols: self.market_close_symbols.clone(),
            trade_callback: Arc::new(RwLock::new(None)), // Clone should not share callback by default
            trade_settings: self.trade_settings.clone(),
            last_orders: self.last_orders.clone(),
            recent_orders_queue: self.recent_orders_queue.clone(),
            order_record_tx: self.order_record_tx.clone(),
            ws_sender: self.ws_sender.clone(),
            last_trade_time_map: self.last_trade_time_map.clone(),
            last_ws_orders: self.last_ws_orders.clone(),
            ws_order_scale: self.ws_order_scale,
            save_metrics: self.save_metrics,
            update_futures_account_data: self.update_futures_account_data,
            run_health_check: self.run_health_check,
            run_small_orders_loop: self.run_small_orders_loop,
            print_orders: self.print_orders,
            login_status_url_template: self.login_status_url_template.clone(),
            order_manager_client: self.order_manager_client.clone(),
            is_monitoring: self.is_monitoring,
            total_net_positions_usdt_size_all_accounts: self
                .total_net_positions_usdt_size_all_accounts
                .clone(),
        }
    }
}

// OrderManager client for communicating with Gate simple-order HTTP endpoint
// This mimics the behavior of Python OrderManagerV3.place_order (HTTP part only).

use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock, RwLock,
    },
    time::{Duration, Instant},
};
use tokio::time::timeout;

use crate::order_processor_grpc::order_processor::PlaceOrderResponse;

/// Parse Flipster order response JSON to extract orderId.
/// Known shape: {"order": {"orderId": "..."}}. Falls back to scanning for
/// "orderId" anywhere in the JSON in case the shape differs across endpoints.
fn parse_flipster_order_id(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    if let Some(id) = value
        .get("order")
        .and_then(|o| o.get("orderId"))
        .and_then(|v| v.as_str())
    {
        return Some(id.to_string());
    }
    if let Some(id) = value.get("orderId").and_then(|v| v.as_str()) {
        return Some(id.to_string());
    }
    if let Some(id) = value
        .get("data")
        .and_then(|d| d.get("orderId"))
        .and_then(|v| v.as_str())
    {
        return Some(id.to_string());
    }
    None
}

fn read_env_flag(name: &str, default: bool) -> bool {
    let value = match std::env::var(name) {
        Ok(value) => value,
        Err(_) => return default,
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => true,
        "0" | "false" | "no" | "n" | "off" => false,
        _ => default,
    }
}

fn read_env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

fn read_env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

/// 경량 클라이언트 rate limit: 1초 N회 + 5초 M회. 연산은 O(1), 할당 없음.
struct ClientRateLimiter {
    per_sec_cap: f64,
    per_sec_tokens: f64,
    per_sec_refill: f64,
    per_sec_last: Instant,
    per_5sec_cap: f64,
    per_5sec_tokens: f64,
    per_5sec_refill: f64,
    per_5sec_last: Instant,
}

impl ClientRateLimiter {
    fn new(per_sec: u32, per_5sec: u32) -> Self {
        let per_sec = per_sec.max(1);
        let per_5sec = per_5sec.max(1);
        let now = Instant::now();
        Self {
            per_sec_cap: per_sec as f64,
            per_sec_tokens: per_sec as f64,
            per_sec_refill: per_sec as f64,
            per_sec_last: now,
            per_5sec_cap: per_5sec as f64,
            per_5sec_tokens: per_5sec as f64,
            per_5sec_refill: per_5sec as f64 / 5.0,
            per_5sec_last: now,
        }
    }

    /// 한 번의 now()와 정수 연산만 사용. true면 통과 후 토큰 차감.
    #[inline(always)]
    fn allow(&mut self, now: Instant) -> bool {
        let e1 = now.duration_since(self.per_sec_last).as_secs_f64();
        self.per_sec_tokens =
            (self.per_sec_tokens + e1 * self.per_sec_refill).min(self.per_sec_cap);
        self.per_sec_last = now;
        let e5 = now.duration_since(self.per_5sec_last).as_secs_f64();
        self.per_5sec_tokens =
            (self.per_5sec_tokens + e5 * self.per_5sec_refill).min(self.per_5sec_cap);
        self.per_5sec_last = now;
        if self.per_sec_tokens >= 1.0 && self.per_5sec_tokens >= 1.0 {
            self.per_sec_tokens -= 1.0;
            self.per_5sec_tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// 30분 슬라이딩 윈도우: 1단계(200회→100 USDT), 2단계(500회→1000 USDT) 등 tier별 최소 크기만 허용.
const TRIM_EVERY: u32 = 16;

struct Window30Min {
    window: Duration,
    max_count: usize,
    min_usdt_size: f64,
    /// 2단계: 이 건수 이상이면 min_usdt_size_tier2 이상만 허용.
    max_count_tier2: Option<usize>,
    min_usdt_size_tier2: Option<f64>,
    timestamps: Vec<Instant>,
    calls: u32,
}

impl Window30Min {
    fn new(
        window_min: u32,
        max_count: u32,
        min_usdt_size: f64,
        max_count_tier2: Option<u32>,
        min_usdt_size_tier2: Option<f64>,
    ) -> Self {
        let min = (window_min as u64).max(1);
        Self {
            window: Duration::from_secs(min * 60),
            max_count: max_count as usize,
            min_usdt_size,
            max_count_tier2: max_count_tier2.map(|c| c as usize),
            min_usdt_size_tier2,
            timestamps: Vec::with_capacity(550),
            calls: 0,
        }
    }

    /// 기록 후 허용 여부 및 현재 윈도우 건수. 500회 이상이면 1000 USDT, 200회 이상이면 100 USDT 이상만 허용.
    #[inline]
    fn record_and_allow(&mut self, now: Instant, usdt_size: f64) -> (bool, usize) {
        self.calls = self.calls.wrapping_add(1);
        let do_trim = self.calls % TRIM_EVERY == 0;
        if do_trim {
            let cutoff = now.checked_sub(self.window).unwrap_or(now);
            self.timestamps.retain(|t| *t >= cutoff);
            if self.timestamps.len() > 600 {
                self.timestamps.drain(0..(self.timestamps.len() - 600));
            }
        }
        let count = self.timestamps.len();
        // 2단계 먼저: 500회 이상이면 1000 USDT 이상만
        if let (Some(t2_count), Some(t2_min)) = (self.max_count_tier2, self.min_usdt_size_tier2) {
            if count >= t2_count && usdt_size < t2_min {
                return (false, count);
            }
        }
        // 1단계: 200회 이상이면 100 USDT 이상만
        if count >= self.max_count && usdt_size < self.min_usdt_size {
            return (false, count);
        }
        self.timestamps.push(now);
        (true, self.timestamps.len())
    }
}

/// 30분 제한을 login_name별로 따로 계산.
struct Window30MinPerLogin {
    map: HashMap<String, Window30Min>,
    window_min: u32,
    max_count: u32,
    min_usdt_size: f64,
    max_count_tier2: Option<u32>,
    min_usdt_size_tier2: Option<f64>,
}

impl Window30MinPerLogin {
    fn new(
        window_min: u32,
        max_count: u32,
        min_usdt_size: f64,
        max_count_tier2: Option<u32>,
        min_usdt_size_tier2: Option<f64>,
    ) -> Self {
        Self {
            map: HashMap::new(),
            window_min,
            max_count,
            min_usdt_size,
            max_count_tier2,
            min_usdt_size_tier2,
        }
    }

    fn record_and_allow(
        &mut self,
        login_name: &str,
        now: Instant,
        usdt_size: f64,
    ) -> (bool, usize) {
        let entry = self.map.entry(login_name.to_string()).or_insert_with(|| {
            Window30Min::new(
                self.window_min,
                self.max_count,
                self.min_usdt_size,
                self.max_count_tier2,
                self.min_usdt_size_tier2,
            )
        });
        entry.record_and_allow(now, usdt_size)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OrderRequest {
    pub symbol: String,                // contract name, e.g. "BTC_USDT"
    pub side: String,                  // "buy" or "sell"
    pub price: String,                 // order price (0 for market)
    pub size: i64,   // contract size (can be negative for close? we use absolute here)
    pub tif: String, // time in force: "fok", "ioc", etc.
    pub level: String, // "limit_open" or "limit_close"
    pub login_name: String, // account / login name
    pub uid: i64,    // Gate account uid (same as Python)
    pub if_not_healthy_ws: bool, // currently unused (WS-only in Python)
    pub only_reduce_only_for_ws: bool, // currently unused (WS-only in Python)
    pub bypass_safe_limit_close: bool, // currently unused (handled in Python)
    pub usdt_size: f64, // order size in USDT
}

pub struct OrderManagerClient {
    client: reqwest::blocking::Client,
    default_uid: i64,
    grpc_timeout_ms: u64,
    grpc_retry_every: u64,
    grpc_last_failed: AtomicBool,
    grpc_runtime: Option<Arc<tokio::runtime::Runtime>>,
    grpc_log_enabled: bool,
    /// 클라이언트 rate limit (1초 N회, 5초 M회). None이면 미적용.
    grpc_client_rate_limiter: Option<Mutex<ClientRateLimiter>>,
    /// 30분 200회 이상이면 최소 주문크기 이상만 허용. login_name별로 별도 계산. None이면 미적용.
    grpc_30min_limiter: Option<Mutex<Window30MinPerLogin>>,
    /// Per-login trading_runtime overrides (Supabase strategy_settings.trading_runtime). Resolved by login_name at send time.
    overrides_by_login: Arc<RwLock<HashMap<String, crate::config::TradingRuntimeOverrides>>>,
}

/// Singleton instance. Initialized on first `instance()` call.
static INSTANCE: OnceLock<Arc<OrderManagerClient>> = OnceLock::new();

#[derive(Debug, Deserialize)]
struct OrderResponse {
    ok: Option<bool>,
    status: Option<i64>,
    #[serde(rename = "statusText")]
    status_text: Option<String>,
    data: Option<Value>,
}

impl OrderManagerClient {
    /// Returns the singleton instance. Initializes on first call (reads .env once).
    /// Use this instead of `new()` so the process shares a single client and overrides_by_login map.
    pub fn instance() -> Arc<Self> {
        INSTANCE
            .get_or_init(|| Arc::new(OrderManagerClient::new()))
            .clone()
    }

    /// Create a new OrderManagerClient. Prefer `instance()` for process-wide singleton.
    ///
    /// Environment variables:
    /// - ORDER_URL: HTTP endpoint for simple-order (default: http://localhost:3005/puppeteer/simple-order)
    /// - ORDER_UID: UID for the account (default: 0)
    /// - ORDER_GRPC_LOG: gRPC logging on/off (default: 0)
    /// - ORDER_GRPC_CLIENT_RATE_LIMIT: 클라이언트 rate limit 사용 (default: 0)
    /// - ORDER_GRPC_CLIENT_RATE_LIMIT_PER_SEC: 1초당 최대 건수 (default: 20)
    /// - ORDER_GRPC_CLIENT_RATE_LIMIT_PER_5SEC: 5초당 최대 건수 (default: 50)
    /// - ORDER_GRPC_CLIENT_30MIN_LIMIT_ENABLED: 30분 N회 초과 시 최소 크기만 허용 (default: 0)
    /// - ORDER_GRPC_CLIENT_30MIN_LIMIT_WINDOW_MIN: 윈도우 분 (default: 30)
    /// - ORDER_GRPC_CLIENT_30MIN_LIMIT_COUNT: 1단계 건수, 초과 시 MIN_SIZE 이상만 허용 (default: 200)
    /// - ORDER_GRPC_CLIENT_30MIN_LIMIT_MIN_SIZE: 1단계 최소 USDT (default: 100)
    /// - ORDER_GRPC_CLIENT_30MIN_LIMIT_COUNT_TIER2: 2단계 건수, 초과 시 MIN_SIZE_TIER2 이상만 (default: 500)
    /// - ORDER_GRPC_CLIENT_30MIN_LIMIT_MIN_SIZE_TIER2: 2단계 최소 USDT (default: 1000)
    pub fn new() -> Self {
        // .env 로드 (이미 로드됐으면 무시). runner에서 먼저 불러도 되고, 여기서 해두면 단독 사용 시에도 .env 적용됨.
        dotenvy::dotenv().ok();

        let default_uid = std::env::var("ORDER_UID")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let grpc_timeout_ms = std::env::var("ORDER_GRPC_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(80);
        let grpc_retry_every = std::env::var("ORDER_GRPC_RETRY_EVERY")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(100);
        let grpc_log_enabled = read_env_flag("ORDER_GRPC_LOG", false);
        let client_rate_limit_enabled = read_env_flag("ORDER_GRPC_CLIENT_RATE_LIMIT", false);
        let per_sec = read_env_u32("ORDER_GRPC_CLIENT_RATE_LIMIT_PER_SEC", 20);
        let per_5sec = read_env_u32("ORDER_GRPC_CLIENT_RATE_LIMIT_PER_5SEC", 50);
        let grpc_client_rate_limiter = if client_rate_limit_enabled {
            Some(Mutex::new(ClientRateLimiter::new(per_sec, per_5sec)))
        } else {
            None
        };
        let client_30min_enabled = read_env_flag("ORDER_GRPC_CLIENT_30MIN_LIMIT_ENABLED", false);
        let window30_min = read_env_u32("ORDER_GRPC_CLIENT_30MIN_LIMIT_WINDOW_MIN", 30);
        let window30_count = read_env_u32("ORDER_GRPC_CLIENT_30MIN_LIMIT_COUNT", 200);
        let window30_min_size = read_env_f64("ORDER_GRPC_CLIENT_30MIN_LIMIT_MIN_SIZE", 100.0);
        let window30_count_tier2 = std::env::var("ORDER_GRPC_CLIENT_30MIN_LIMIT_COUNT_TIER2")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        let window30_min_size_tier2 = std::env::var("ORDER_GRPC_CLIENT_30MIN_LIMIT_MIN_SIZE_TIER2")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok());
        let (count_tier2, min_tier2) = match (window30_count_tier2, window30_min_size_tier2) {
            (Some(c), Some(m)) => (Some(c), Some(m)),
            _ => (Some(500), Some(1000.0)), // 기본 2단계: 500회 → 1000 USDT
        };
        let grpc_30min_limiter = if client_30min_enabled {
            Some(Mutex::new(Window30MinPerLogin::new(
                window30_min,
                window30_count,
                window30_min_size,
                count_tier2,
                min_tier2,
            )))
        } else {
            None
        };
        let grpc_runtime = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to build gRPC runtime"),
        );

        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_millis(200))
            .timeout(Duration::from_millis(300))
            .build()
            .expect("Failed to build reqwest client");

        Self {
            client,
            default_uid,
            grpc_timeout_ms,
            grpc_retry_every,
            grpc_last_failed: AtomicBool::new(false),
            grpc_runtime: Some(grpc_runtime),
            grpc_log_enabled,
            grpc_client_rate_limiter,
            grpc_30min_limiter,
            overrides_by_login: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Set or replace per-login trading_runtime overrides (e.g. from Supabase strategy_settings). Call after construction or on config refresh.
    pub fn set_overrides_by_login(
        &self,
        map: HashMap<String, crate::config::TradingRuntimeOverrides>,
    ) {
        let mut guard = self.overrides_by_login.write().expect("overrides_by_login");
        *guard = map;
    }

    /// Resolve order_url for a login: overrides_by_login[login] > .env > default.
    pub fn order_url_for_login(&self, login_name: &str) -> String {
        let overrides = self.overrides_by_login.read().expect("overrides_by_login");
        overrides
            .get(login_name)
            .and_then(|o| o.order_url.as_ref())
            .cloned()
            .or_else(|| std::env::var("ORDER_URL").ok())
            .unwrap_or_else(|| "http://localhost:3005/puppeteer/simple-order".to_string())
    }

    /// Resolve gRPC targets for a login: overrides_by_login[login] > .env.
    fn grpc_targets_for_login(&self, login_name: &str) -> Vec<String> {
        let overrides = self.overrides_by_login.read().expect("overrides_by_login");
        let o = overrides.get(login_name);
        let grpc_lb_addr = o
            .and_then(|x| x.order_lb_grpc_addr.as_ref())
            .cloned()
            .or_else(|| std::env::var("ORDER_LB_GRPC_ADDR").ok());
        let grpc_addr = o
            .and_then(|x| x.order_grpc_addr.as_ref())
            .cloned()
            .or_else(|| std::env::var("ORDER_GRPC_ADDR").ok());
        let mut targets = Vec::new();
        if let Some(lb) = grpc_lb_addr {
            targets.push(lb);
        }
        let direct = grpc_addr.unwrap_or_else(|| "127.0.0.1:50051".to_string());
        if targets.first().map(|v| v != &direct).unwrap_or(true) {
            targets.push(direct);
        }
        targets
    }

    /// Resolve order_processor_ip for a login: overrides_by_login[login] > .env > "".
    fn order_processor_ip_for_login(&self, login_name: &str) -> String {
        let overrides = self.overrides_by_login.read().expect("overrides_by_login");
        overrides
            .get(login_name)
            .and_then(|o| o.order_processor_ip.as_ref())
            .cloned()
            .or_else(|| std::env::var("ORDER_PROCESSOR_IP").ok())
            .unwrap_or_default()
    }

    // fn should_attempt_grpc(&self) -> bool {
    //     if !self.grpc_last_failed.load(Ordering::Relaxed) {
    //         return true;
    //     }

    //     let count = self.grpc_attempt_counter.fetch_add(1, Ordering::Relaxed) + 1;
    //     count % self.grpc_retry_every == 0
    // }

    fn try_send_order_grpc(&self, req: &OrderRequest) -> Result<PlaceOrderResponse, String> {
        let contract = req.symbol.trim().to_ascii_lowercase();
        let bypass_rate_limit = contract == "btc_usdt";

        if !bypass_rate_limit {
            if let Some(limiter) = &self.grpc_client_rate_limiter {
                let now = Instant::now();
                if !limiter.lock().expect("rate_limiter").allow(now) {
                    return Err("client_rate_limit".to_string());
                }
            }
            if let Some(w) = &self.grpc_30min_limiter {
                let now = Instant::now();
                let (allowed, count) = w.lock().expect("30min_limiter").record_and_allow(
                    &req.login_name,
                    now,
                    req.usdt_size,
                );

                if log::log_enabled!(log::Level::Info) {
                    // count to yellow
                    info!(
                        "🕒 [30min] login_name={} count=(( \x1b[93m{}\x1b[0m )) usdt_size={} allowed={}",
                        req.login_name, count, req.usdt_size, allowed
                    );
                }

                if !allowed {
                    return Err("client_30min_limit_min_size".to_string());
                }
            }
        }
        if self.grpc_log_enabled {
            debug!("\x1b[92m[OrderManagerClient] ➡️ Trying to send order via gRPC\x1b[0m");
        }
        let grpc_start = std::time::Instant::now();

        let runtime = self
            .grpc_runtime
            .as_ref()
            .ok_or_else(|| "grpc_runtime_not_set".to_string())?;

        let targets = self.grpc_targets_for_login(&req.login_name);
        let order_processor_ip = self.order_processor_ip_for_login(&req.login_name);
        if self.grpc_log_enabled {
            debug!(
                "[OrderManagerClient] gRPC targets={:?} timeout_ms={} retry_every={} login_name={}",
                targets, self.grpc_timeout_ms, self.grpc_retry_every, req.login_name
            );
        }
        if targets.is_empty() {
            // with emoji red cross
            if self.grpc_log_enabled {
                error!("\x1b[91m[OrderManagerClient] ❌ gRPC target list is empty\x1b[0m");
            }
            return Err("grpc_target_missing".to_string());
        }

        let order_type = if req.price.parse::<f64>().unwrap_or(0.0) > 0.0 {
            "limit"
        } else {
            "market"
        };
        let price_str = req.price.clone();
        let size_str = format!("{}", req.size);
        let timestamp_ms = chrono::Utc::now().timestamp_millis();

        let overall_timeout =
            Duration::from_millis(self.grpc_timeout_ms.saturating_mul(4).max(500));
        if self.grpc_log_enabled {
            debug!(
                "[OrderManagerClient] gRPC overall_timeout_ms={}",
                overall_timeout.as_millis()
            );
        }

        if self.grpc_log_enabled {
            debug!("[OrderManagerClient] gRPC block_on enter");
        }
        let order_processor_ip = order_processor_ip.clone();
        let result = runtime.block_on(async move {
            match timeout(overall_timeout, async move {
                let mut last_err: Option<String> = None;
                for target in targets {
                    if self.grpc_log_enabled {
                        debug!("[OrderManagerClient] gRPC attempt target={}", target);
                    }
                    let response = match self
                        .send_grpc_to_target(
                            target.clone(),
                            &order_type,
                            &price_str,
                            &size_str,
                            timestamp_ms,
                            req,
                            &order_processor_ip,
                        )
                        .await
                    {
                        Ok(response) => {
                            if self.grpc_log_enabled {
                                info!(
                                    "\x1b[92m[OrderManagerClient] ✅ Order sent via gRPC to target={}\x1b[0m",
                                    target
                                );
                            }
                            return Ok(response);
                        }
                        Err(e) => {
                            last_err = Some(format!("target={} err={}", target, e));
                            if self.grpc_log_enabled {
                                error!(
                                    "\x1b[91m[OrderManagerClient] ❌ Error sending order via gRPC to target={}: {}\x1b[0m",
                                    target,
                                    e
                                );
                            }
                            continue;
                        }
                    };
                }
                // with emoji red cross
                if self.grpc_log_enabled {
                    error!("\x1b[91m[OrderManagerClient] ❌ Order failed to send via gRPC\x1b[0m");
                }

                Err(last_err.unwrap_or_else(|| "grpc_request_error".to_string()))
            })
            .await
            {
                Ok(result) => {
                    // with emoji with time emoji
                    if self.grpc_log_enabled {
                        info!(
                            "[OrderManagerClient] 🕙 gRPC order completed in {}ms",
                            grpc_start.elapsed().as_millis()
                        );
                    }
                    result
                }
                Err(_) => {
                    // with emoji red cross
                    if self.grpc_log_enabled {
                        error!(
                            "\x1b[91m[OrderManagerClient] ❌ gRPC overall timeout after {}ms\x1b[0m",
                            overall_timeout.as_millis()
                        );
                    }
                    Err("grpc_overall_timeout".to_string())
                }
            }
        });
        if self.grpc_log_enabled {
            debug!("[OrderManagerClient] gRPC block_on exit");
        }
        result
    }

    async fn send_grpc_to_target(
        &self,
        target: String,
        order_type: &str,
        price_str: &str,
        size_str: &str,
        timestamp_ms: i64,
        req: &OrderRequest,
        order_processor_ip: &str,
    ) -> Result<PlaceOrderResponse, String> {
        use crate::order_processor_grpc::order_processor::order_processor_client::OrderProcessorClient;
        use crate::order_processor_grpc::order_processor::PlaceOrderRequest;
        use tonic::transport::Endpoint;
        // use tonic::Code;

        let uri = if target.starts_with("http://") || target.starts_with("https://") {
            target.clone()
        } else {
            format!("http://{}", target)
        };
        if self.grpc_log_enabled {
            debug!(
                "[OrderManagerClient] gRPC dial target={} uri={} timeout_ms={}",
                target, uri, self.grpc_timeout_ms
            );
        }

        let endpoint = Endpoint::from_shared(uri.clone())
            .map_err(|e| format!("grpc_endpoint_error: {}", e))?
            .connect_timeout(Duration::from_millis(self.grpc_timeout_ms))
            .timeout(Duration::from_millis(self.grpc_timeout_ms));
        let channel = endpoint
            .connect()
            .await
            .map_err(|e| format!("grpc_connect_timeout: {}", e))?;
        if self.grpc_log_enabled {
            info!(
                "\x1b[92m[OrderManagerClient] ✅ gRPC connected target={} uri={}\x1b[0m",
                target,
                uri.clone()
            );
        }

        let mut client = OrderProcessorClient::new(channel);
        if self.grpc_log_enabled {
            debug!(
                "\x1b[92m[OrderManagerClient] ➡️ gRPC PlaceOrder start target={} order_type={} size={} price={} tif={}\x1b[0m",
                target, order_type, size_str, price_str, req.tif
            );
        }

        let request = PlaceOrderRequest {
            account_id: req.login_name.clone(),
            contract: req.symbol.clone(),
            order_type: order_type.to_string(),
            price: price_str.to_string(),
            reduce_only: req.level == "limit_close" || req.level == "market_close",
            size: size_str.to_string(),
            text: "web".to_string(),
            tif: req.tif.clone(),
            timestamp: timestamp_ms,
            order_processor_ip: order_processor_ip.to_string(),
            priority: 0,
        };

        let response = match timeout(
            Duration::from_millis(self.grpc_timeout_ms),
            client.place_order(request),
        )
        .await
        {
            Ok(Ok(resp)) => resp.into_inner(),
            Ok(Err(status)) => {
                if self.grpc_log_enabled {
                    error!(
                        "\x1b[91m[OrderManagerClient] ❌ gRPC request error: {}\x1b[0m",
                        status
                    );
                }
                return Err(format!("grpc_request_error: {}", status));
            }
            Err(err) => {
                if self.grpc_log_enabled {
                    error!(
                        "\x1b[91m[OrderManagerClient] ❌ gRPC request timeout: {}\x1b[0m",
                        err
                    );
                }
                return Err("grpc_request_timeout".to_string());
            }
        };

        if !response.ok {
            let data_suffix = if response.data_json.is_empty() {
                "".to_string()
            } else {
                format!(" data={}", response.data_json)
            };

            return Err(format!(
                "grpc_order_failed: status={} status_text={}{}",
                response.status, response.status_text, data_suffix
            ));
        }

        if self.grpc_log_enabled {
            debug!(
                "\x1b[92m[OrderManagerClient] ✅ gRPC order ok: status={} status_text={} data={}\x1b[0m",
                response.status, response.status_text, response.data_json
            );
        }

        Ok(response)
    }

    /// Send order to simple-order HTTP endpoint.
    ///
    /// This constructs the same JSON shape as Python's OrderManager.place_order after _build_order_param:
    /// {
    ///   "account": login_name,
    ///   "uid": uid,
    ///   "contract": symbol,
    ///   "price": "123.45",
    ///   "reduce_only": true/false,
    ///   "order_type": "limit" or "market",
    ///   "size": "10", // sign encodes side (buy:+, sell:-)
    ///   "tif": "ioc",
    ///   "text": "web",
    ///   "message": { "timestamp": <ms> }
    /// }
    pub fn send_order(
        &self,
        req: OrderRequest,
        _bypass_load_balancer: bool,
    ) -> Result<PlaceOrderResponse, String> {
        // Flipster: direct HTTP POST (no gRPC order_processor sidecar).
        // Uses global FlipsterCookieStore cookies; falls back to gRPC path if
        // cookie store not initialized (gate_hft legacy mode).
        if crate::flipster_cookie_store::global().is_some() {
            return match self.try_send_order_flipster(&req) {
                Ok(resp) => {
                    if log::log_enabled!(log::Level::Info) {
                        let side_emoji = if req.side == "buy" { "🔷" } else { "🔶" };
                        let usdt_sign = if req.size >= 0 { "+" } else { "-" };
                        let usdt_size_str = format!("{}{:.2}", usdt_sign, req.usdt_size.abs());
                        info!(
                            "\x1b[92m[FlipsterHTTP] {} \x1b[0m {} | {} | {} | {} | {}",
                            side_emoji, req.symbol, req.price, usdt_size_str, req.level, req.login_name
                        );
                    }
                    Ok(resp)
                }
                Err(e) => {
                    warn!("\x1b[91m❌ Flipster order failed: {}\x1b[0m", e);
                    Err(e)
                }
            };
        }

        match self.try_send_order_grpc(&req) {
            Ok(response) => {
                self.grpc_last_failed.store(false, Ordering::Relaxed);
                if log::log_enabled!(log::Level::Info) {
                    let side_emoji = if req.side == "buy" { "🔷" } else { "🔶" };
                    let usdt_sign = if req.size >= 0 { "+" } else { "-" };
                    let usdt_size_str = format!("{}{:.2}", usdt_sign, req.usdt_size.abs());
                    info!(
                        "\x1b[92m[OrderManagerClient] {} \x1b[0m {} | {} | {} | {} | {}",
                        side_emoji, req.symbol, req.price, usdt_size_str, req.level, req.login_name
                    );
                }
                Ok(response)
            }
            Err(e) => {
                if e.contains("grpc_connect_error") || e.contains("transport error") {
                    self.grpc_last_failed.store(true, Ordering::Relaxed);
                }
                if self.grpc_log_enabled {
                    warn!("\x1b[91m❌ gRPC order failed: {}\x1b[0m", e);
                }
                Err(e)
            }
        }
    }

    /// Flipster direct HTTP POST — converts gate-style OrderRequest to Flipster body format
    /// and synthesizes a PlaceOrderResponse compatible with the gate gRPC schema.
    fn try_send_order_flipster(&self, req: &OrderRequest) -> Result<PlaceOrderResponse, String> {
        let store = crate::flipster_cookie_store::global()
            .ok_or_else(|| "flipster_cookie_store not initialized".to_string())?;

        // Map gate-style level → Flipster orderType / reduceOnly.
        let (order_type, reduce_only, post_only) = match req.level.as_str() {
            "market_close" => ("ORDER_TYPE_MARKET", true, false),
            "market_open" => ("ORDER_TYPE_MARKET", false, false),
            "limit_close" => ("ORDER_TYPE_LIMIT", true, req.tif.eq_ignore_ascii_case("poc")),
            "limit_open" => ("ORDER_TYPE_LIMIT", false, req.tif.eq_ignore_ascii_case("poc")),
            other => {
                return Err(format!("unknown level '{}' in OrderRequest", other));
            }
        };

        // Pre-send edge reconfirmation: for stale-price LIMIT entries the edge can
        // evaporate between signal fire and HTTP submit. Re-read the latest api-side
        // bookticker (gate_booktickers = api feed in Flipster mode) and skip the
        // order if the web price we're about to submit no longer has an edge vs api.
        // Skip for market orders (they fill at whatever price) and when env disabled.
        if order_type == "ORDER_TYPE_LIMIT"
            && std::env::var("FLIPSTER_EDGE_RECHECK")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true)
        {
            if let Ok(limit_price) = req.price.parse::<f64>() {
                if limit_price > 0.0 {
                    if let Some(cache) = crate::data_cache::global() {
                        if let Some(api_bt) = cache.get_gate_bookticker(&req.symbol) {
                            let api_bid = api_bt.bid_price;
                            let api_ask = api_bt.ask_price;
                            if api_bid > 0.0 && api_ask > 0.0 {
                                let api_mid = (api_bid + api_ask) * 0.5;
                                let edge_gone = match req.side.as_str() {
                                    // BUY: we pay limit_price; need api_mid > limit_price
                                    // (api still sees true price above our stale buy price).
                                    "buy" => api_mid <= limit_price,
                                    // SELL: we receive limit_price; need api_mid < limit_price.
                                    "sell" => api_mid >= limit_price,
                                    _ => false,
                                };
                                if edge_gone {
                                    debug!(
                                        "[FlipsterHTTP] edge_gone skip: {} {} limit={:.6} api_mid={:.6} (bid={:.6} ask={:.6})",
                                        req.symbol, req.side, limit_price, api_mid, api_bid, api_ask
                                    );
                                    return Err(format!(
                                        "edge_gone: {} {} limit={} api_mid={:.6}",
                                        req.symbol, req.side, req.price, api_mid
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Flipster one-way order expects "Long"/"Short" side.
        let flipster_side = match req.side.as_str() {
            "buy" => "Long",
            "sell" => "Short",
            other => return Err(format!("unknown side '{}'", other)),
        };

        let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).to_string();
        let body = serde_json::json!({
            "side": flipster_side,
            "requestId": uuid::Uuid::new_v4().to_string(),
            "timestamp": now_ns,
            "reduceOnly": reduce_only,
            "refServerTimestamp": now_ns,
            "refClientTimestamp": now_ns,
            "leverage": 10,
            "price": req.price,
            "amount": format!("{:.4}", req.usdt_size.abs()),
            "marginType": "Cross",
            "orderType": order_type,
            "postOnly": post_only,
        });

        let url = format!(
            "https://api.flipster.io/api/v2/trade/one-way/order/{}",
            req.symbol
        );
        let t0 = Instant::now();
        let http_resp = self
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
            .map_err(|e| format!("flipster http send: {}", e))?;
        let http_time_us = t0.elapsed().as_micros() as i64;

        let status = http_resp.status();
        let status_code = status.as_u16() as i32;
        let status_text = status.canonical_reason().unwrap_or("").to_string();
        let text = http_resp
            .text()
            .unwrap_or_else(|e| format!("{{\"error\":\"read body: {}\"}}", e));

        // Schedule client-side expire cancel for LIMIT orders so stale-price orders
        // do not sit on Flipster's book waiting for adverse selection.
        // Flipster's v2 endpoint has no server-side expire field (unlike Gate's
        // x-gate-exptime), so we spawn a tokio task that DELETEs after expire_ms.
        if order_type == "ORDER_TYPE_LIMIT" && !status.is_success() {
            warn!(
                "[FlipsterHTTP] order rejected status={} body={}",
                status_code,
                &text.chars().take(200).collect::<String>()
            );
        }
        if status.is_success() && order_type == "ORDER_TYPE_LIMIT" {
            match parse_flipster_order_id(&text) {
                Some(order_id) => {
                    let expire_ms = match req.level.as_str() {
                        "limit_close" => std::env::var("FLIPSTER_CLOSE_EXPIRE_MS")
                            .ok()
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(30),
                        _ => std::env::var("FLIPSTER_OPEN_EXPIRE_MS")
                            .ok()
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(20),
                    };
                    if expire_ms > 0 {
                        info!(
                            "[FlipsterHTTP] expire-cancel scheduled: {} order_id={} in {}ms",
                            req.symbol, order_id, expire_ms
                        );
                        let client = self.client.clone();
                        let cookie_header = store.cookie_header();
                        let symbol = req.symbol.clone();
                        let order_id_for_task = order_id.clone();
                        let rt = crate::flipster_runtime::handle();
                        rt.spawn(async move {
                            tokio::time::sleep(Duration::from_millis(expire_ms)).await;
                            let result = tokio::task::spawn_blocking(move || {
                                let url = format!(
                                    "https://api.flipster.io/api/v2/trade/orders/{}/{}",
                                    symbol, order_id_for_task
                                );
                                let now_ns = chrono::Utc::now()
                                    .timestamp_nanos_opt()
                                    .unwrap_or(0)
                                    .to_string();
                                let body = serde_json::json!({
                                    "requestId": uuid::Uuid::new_v4().to_string(),
                                    "timestamp": now_ns,
                                    "attribution": "TRADE_PERPETUAL_DEFAULT",
                                });
                                let r = client
                                    .delete(&url)
                                    .header("Cookie", cookie_header)
                                    .header(
                                        "User-Agent",
                                        "Mozilla/5.0 (X11; Linux x86_64) Chrome/141.0.0.0",
                                    )
                                    .header("Origin", "https://flipster.io")
                                    .header("Referer", "https://flipster.io/")
                                    .json(&body)
                                    .send();
                                match r {
                                    Ok(resp) => (resp.status().as_u16(), resp.text().unwrap_or_default()),
                                    Err(e) => (0, e.to_string()),
                                }
                            })
                            .await;
                            if let Ok((code, body)) = result {
                                info!(
                                    "[FlipsterHTTP] expire-cancel DELETE: status={} body={}",
                                    code,
                                    &body.chars().take(120).collect::<String>()
                                );
                            }
                        });
                    }
                }
                None => {
                    warn!(
                        "[FlipsterHTTP] no orderId parsed from response body (first 200 chars): {}",
                        &text.chars().take(200).collect::<String>()
                    );
                }
            }
        }

        Ok(PlaceOrderResponse {
            ok: status.is_success(),
            status: status_code,
            status_text,
            data_json: text,
            response_time: http_time_us,
            order_processing_time: 0,
            server_processing_time: 0,
            request_parsing_time: 0,
            json_decode_time: 0,
            account_lookup_time: 0,
            cookie_retrieval_time: 0,
            payload_marshal_time: 0,
            request_build_time: 0,
            http_request_time: http_time_us,
        })
    }

    /// Placeholder: position query is still handled in Python OrderManager.
    /// For now, we return 0 and rely on Python-side risk management.
    pub fn get_position(&self, _symbol: &str, _login_name: &str) -> Result<f64, String> {
        // TODO: Expose position via HTTP or shared state if needed
        Ok(0.0)
    }

    /// Placeholder: order count size query is still handled in Python OrderManager.
    /// For now, we return 0 and rely on Python-side tracking.
    pub fn get_order_count_size(&self, _symbol: &str, _login_name: &str) -> Result<i64, String> {
        // TODO: Expose order count via HTTP or shared state if needed
        Ok(0)
    }
}

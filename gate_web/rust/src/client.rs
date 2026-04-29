use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Url;

use crate::error::GateError;
use crate::order::{OrderParams, OrderResponse};

pub const API_BASE: &str = "https://www.gate.com";
pub const ORDER_URL: &str = "https://www.gate.com/apiw/v2/futures/usdt/orders";
pub const USER_INFO_URL: &str = "https://www.gate.com/api/web/v1/usercenter/get_info";

#[derive(Debug, Clone)]
pub struct GateConfig {
    /// Map of cookie name → value, e.g. straight from CDP extraction.
    /// Gate doesn't have a single load-bearing token name we hardcode like
    /// Flipster's __cf_bm, so we store them all and replay verbatim.
    pub cookies: HashMap<String, String>,
    /// Optional HTTP/HTTPS proxy URL.
    pub proxy: Option<String>,
    /// If true, build the request but don't actually send it.
    pub dry_run: bool,
}

pub struct GateClient {
    http: reqwest::Client,
    jar: Arc<Jar>,
    dry_run: bool,
}

impl GateClient {
    pub fn new(config: GateConfig) -> Result<Self, GateError> {
        let jar = Arc::new(Jar::default());
        let api_url: Url = API_BASE.parse().expect("API_BASE parse");

        for (name, value) in &config.cookies {
            jar.add_cookie_str(
                &format!("{name}={value}; Domain=.gate.com; Path=/"),
                &api_url,
            );
        }

        let mut builder = reqwest::Client::builder()
            .cookie_provider(jar.clone())
            .default_headers(Self::default_headers())
            .gzip(true)
            .https_only(true)
            // Match Python `requests` (urllib3) — HTTP/1.1, no h2 ALPN.
            // Akamai's bot manager fingerprints HTTP/2 + rustls handshakes
            // distinctly from urllib3, so we mirror urllib3's wire shape.
            .http1_only();

        if let Some(proxy_url) = &config.proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy_url)?);
        }

        Ok(GateClient {
            http: builder.build()?,
            jar,
            dry_run: config.dry_run,
        })
    }

    /// Build a client straight from a cookie map (no proxy, no dry-run).
    pub fn from_cookies(cookies: &HashMap<String, String>) -> Result<Self, GateError> {
        Self::new(GateConfig {
            cookies: cookies.clone(),
            proxy: None,
            dry_run: false,
        })
    }

    /// Replace cookies in the existing jar (e.g. after dump_cookies refresh).
    pub fn reload_cookies(&self, cookies: &HashMap<String, String>) {
        let api_url: Url = API_BASE.parse().expect("API_BASE parse");
        for (name, value) in cookies {
            self.jar.add_cookie_str(
                &format!("{name}={value}; Domain=.gate.com; Path=/"),
                &api_url,
            );
        }
    }

    fn default_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        // NOTE: must match TLS fingerprint. Akamai's bot filter blocks
        // Chrome User-Agents from non-Chrome TLS handshakes (reqwest's
        // rustls/native-tls fingerprint differs from Chrome's BoringSSL).
        // Empirically, `python-requests/*` UA passes from this TLS stack
        // — Python's own `requests` library uses the same shape.
        h.insert(
            "User-Agent",
            HeaderValue::from_static("python-requests/2.32.3"),
        );
        h.insert("Accept", HeaderValue::from_static("application/json"));
        h.insert("Content-Type", HeaderValue::from_static("application/json"));
        h.insert("X-Gate-Applang", HeaderValue::from_static("en"));
        h.insert("X-Gate-Device-Type", HeaderValue::from_static("0"));
        h
    }

    /// Login probe — Gate returns code=0 + message="Success" when the
    /// session is valid. Useful as a smoke test after loading cookies.
    pub async fn check_login(&self) -> Result<serde_json::Value, GateError> {
        let resp = self.http.get(USER_INFO_URL).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        serde_json::from_str(&text).map_err(|_| GateError::NonJson { status, body: text })
    }

    pub async fn is_logged_in(&self) -> bool {
        match self.check_login().await {
            Ok(v) => {
                let code_ok = v.get("code").and_then(|x| x.as_i64()) == Some(0);
                let msg_ok = v
                    .get("message")
                    .and_then(|x| x.as_str())
                    .map(|s| s.eq_ignore_ascii_case("success"))
                    .unwrap_or(false);
                code_ok && msg_ok
            }
            Err(_) => false,
        }
    }

    /// Set leverage for a contract.
    /// `leverage = 0` → CROSS mode with `cross_leverage_limit` as cap.
    /// `leverage > 0` → ISOLATED at that leverage.
    pub async fn set_leverage(
        &self,
        contract: &str,
        leverage: u32,
        cross_leverage_limit: u32,
    ) -> Result<serde_json::Value, GateError> {
        let url = format!(
            "{API_BASE}/apiw/v2/futures/usdt/positions/{contract}/leverage?sub_website_id=0"
        );
        let body = serde_json::json!({
            "leverage": leverage.to_string(),
            "cross_leverage_limit": cross_leverage_limit.to_string(),
        });
        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        serde_json::from_str(&text).map_err(|_| GateError::NonJson { status, body: text })
    }

    /// Place a futures order. Mirrors the Python `place_order` shape:
    /// returns `OrderResponse { ok, status, data, elapsed_ms }`.
    pub async fn place_order(
        &self,
        contract: &str,
        params: OrderParams,
    ) -> Result<OrderResponse, GateError> {
        let body = params.to_body(contract);
        if self.dry_run {
            return Ok(OrderResponse {
                ok: true,
                status: 200,
                data: serde_json::json!({"dry_run": true, "body": body}),
                elapsed_ms: 0.0,
            });
        }
        self.send_order_post(&body).await
    }

    /// Reduce-only market IOC close. `signed_size` follows the same convention
    /// as Python: positive = close short (buy back), negative = close long.
    pub async fn close_position(
        &self,
        contract: &str,
        signed_size: i64,
        price: Option<f64>,
    ) -> Result<OrderResponse, GateError> {
        let body = OrderParams::close_body(contract, signed_size, price);
        if self.dry_run {
            return Ok(OrderResponse {
                ok: true,
                status: 200,
                data: serde_json::json!({"dry_run": true, "body": body}),
                elapsed_ms: 0.0,
            });
        }
        self.send_order_post(&body).await
    }

    /// Cancel an open order via DELETE /orders/{id}.
    pub async fn cancel_order(&self, order_id: &str) -> Result<OrderResponse, GateError> {
        let url = format!("{API_BASE}/apiw/v2/futures/usdt/orders/{order_id}");
        let t0 = Instant::now();
        let resp = self.http.delete(&url).send().await?;
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let data = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => v,
            Err(_) => {
                return Ok(OrderResponse {
                    ok: false,
                    status,
                    data: serde_json::json!({"text": text.chars().take(200).collect::<String>()}),
                    elapsed_ms,
                });
            }
        };
        let ok = status == 200
            && data.get("message").and_then(|v| v.as_str()) == Some("success");
        Ok(OrderResponse { ok, status, data, elapsed_ms })
    }

    pub async fn get_order(&self, order_id: &str) -> Result<OrderResponse, GateError> {
        let url = format!("{API_BASE}/apiw/v2/futures/usdt/orders/{order_id}");
        let t0 = Instant::now();
        let resp = self.http.get(&url).send().await?;
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let data: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return Err(GateError::NonJson { status, body: text }),
        };
        let ok = status == 200
            && data.get("message").and_then(|v| v.as_str()) == Some("success");
        Ok(OrderResponse { ok, status, data, elapsed_ms })
    }

    /// Fetch open positions (non-zero size). Filtering is done by the caller.
    pub async fn positions(&self) -> Result<serde_json::Value, GateError> {
        let url = format!("{API_BASE}/apiw/v2/futures/usdt/positions");
        let resp = self.http.get(&url).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        serde_json::from_str(&text).map_err(|_| GateError::NonJson { status, body: text })
    }

    /// Account balance + lifetime history (pnl/fee/fund/dnw).
    pub async fn accounts(&self) -> Result<serde_json::Value, GateError> {
        let url = format!("{API_BASE}/apiw/v2/futures/usdt/accounts");
        let resp = self.http.get(&url).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        serde_json::from_str(&text).map_err(|_| GateError::NonJson { status, body: text })
    }

    async fn send_order_post(
        &self,
        body: &serde_json::Value,
    ) -> Result<OrderResponse, GateError> {
        let t0 = Instant::now();
        let resp = self.http.post(ORDER_URL).json(body).send().await?;
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let status = resp.status().as_u16();
        let text = resp.text().await?;

        let data: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => {
                return Err(GateError::NonJson {
                    status,
                    body: text.chars().take(500).collect(),
                })
            }
        };

        let label = data.get("label").and_then(|v| v.as_str()).unwrap_or("");
        if label == "TOO_MANY_REQUEST" || status == 429 {
            return Err(GateError::RateLimited(data.to_string()));
        }
        if status == 401 || status == 403 {
            return Err(GateError::Auth);
        }

        let message = data.get("message").and_then(|v| v.as_str()).unwrap_or("");
        let ok = (status == 200 && message == "success") || label == "ErrorOrderFok";

        Ok(OrderResponse { ok, status, data, elapsed_ms })
    }

    /// Send a raw order body (debugging).
    pub async fn raw_request(
        &self,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, GateError> {
        if self.dry_run {
            return Ok(serde_json::json!({"dry_run": true, "body": body}));
        }
        let resp = self.http.post(ORDER_URL).json(&body).send().await?;
        let status = resp.status().as_u16();
        if status == 401 || status == 403 {
            return Err(GateError::Auth);
        }
        let text = resp.text().await?;
        serde_json::from_str(&text).map_err(|_| GateError::NonJson { status, body: text })
    }
}

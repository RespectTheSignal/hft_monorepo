use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{cookie::Jar, Url};

use crate::error::FlipsterError;
use crate::order::{OrderParams, OrderRequestBody, OrderResponse};

const API_BASE: &str = "https://api.flipster.io";

#[derive(Debug, Clone)]
pub struct FlipsterConfig {
    pub session_id_bolts: String,
    pub session_id_nuts: String,
    pub ajs_user_id: String,
    pub cf_bm: String,
    pub ga: String,
    pub ga_rh8fm2jkcm: String,
    pub ajs_anonymous_id: String,
    pub analytics_session_id: String,
    pub internal: String,
    pub referral_path: String,
    pub referrer_symbol: String,
    /// If true, build the request but don't actually send it.
    pub dry_run: bool,
    /// Optional HTTP proxy URL (e.g. "http://127.0.0.1:8080").
    pub proxy: Option<String>,
}

pub struct FlipsterClient {
    http: reqwest::Client,
    jar: Arc<Jar>,
    cf_bm: Arc<RwLock<String>>,
    dry_run: bool,
}

impl FlipsterClient {
    pub fn new(config: FlipsterConfig) -> Self {
        let jar = Arc::new(Jar::default());
        let api_url: Url = API_BASE.parse().unwrap();

        // Load all session cookies
        let cookies = [
            ("session_id_bolts", &config.session_id_bolts),
            ("session_id_nuts", &config.session_id_nuts),
            ("ajs_user_id", &config.ajs_user_id),
            ("__cf_bm", &config.cf_bm),
            ("_ga", &config.ga),
            ("_ga_RH8FM2JKCM", &config.ga_rh8fm2jkcm),
            ("ajs_anonymous_id", &config.ajs_anonymous_id),
            ("analytics_session_id", &config.analytics_session_id),
            ("internal", &config.internal),
            ("referral_path", &config.referral_path),
            ("referrer_symbol", &config.referrer_symbol),
        ];
        for (name, value) in &cookies {
            jar.add_cookie_str(
                &format!("{name}={value}; Domain=api.flipster.io; Path=/"),
                &api_url,
            );
        }

        let mut builder = reqwest::Client::builder()
            .cookie_provider(jar.clone())
            .default_headers(Self::default_headers())
            .gzip(true)
            .https_only(true);

        if let Some(proxy_url) = &config.proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy_url).expect("invalid proxy URL"));
        }

        FlipsterClient {
            http: builder.build().expect("failed to build HTTP client"),
            jar,
            cf_bm: Arc::new(RwLock::new(config.cf_bm)),
            dry_run: config.dry_run,
        }
    }

    /// Create a client from a HashMap of cookies (e.g. from CDP extraction).
    pub fn from_cookies(cookies: &HashMap<String, String>) -> Self {
        let config = FlipsterConfig {
            session_id_bolts: cookies.get("session_id_bolts").cloned().unwrap_or_default(),
            session_id_nuts: cookies.get("session_id_nuts").cloned().unwrap_or_default(),
            ajs_user_id: cookies.get("ajs_user_id").cloned().unwrap_or_default(),
            cf_bm: cookies.get("__cf_bm").cloned().unwrap_or_default(),
            ga: cookies.get("_ga").cloned().unwrap_or_default(),
            ga_rh8fm2jkcm: cookies.get("_ga_RH8FM2JKCM").cloned().unwrap_or_default(),
            ajs_anonymous_id: cookies.get("ajs_anonymous_id").cloned().unwrap_or_default(),
            analytics_session_id: cookies.get("analytics_session_id").cloned().unwrap_or_default(),
            internal: cookies.get("internal").cloned().unwrap_or("false".into()),
            referral_path: cookies.get("referral_path").cloned().unwrap_or_default(),
            referrer_symbol: cookies.get("referrer_symbol").cloned().unwrap_or_default(),
            dry_run: false,
            proxy: None,
        };
        Self::new(config)
    }

    /// Reload all cookies from a HashMap (e.g. after refresh_cookies).
    pub fn reload_cookies(&self, cookies: &HashMap<String, String>) {
        let api_url: Url = API_BASE.parse().unwrap();
        for (name, value) in cookies {
            let cookie_name = match name.as_str() {
                "cf_bm" => "__cf_bm",
                "ga" => "_ga",
                "ga_rh8fm2jkcm" => "_ga_RH8FM2JKCM",
                other => other,
            };
            self.jar.add_cookie_str(
                &format!("{cookie_name}={value}; Domain=api.flipster.io; Path=/"),
                &api_url,
            );
        }
    }

    fn default_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("User-Agent", HeaderValue::from_static("Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0"));
        h.insert("Accept", HeaderValue::from_static("application/json, text/plain, */*"));
        h.insert("Accept-Language", HeaderValue::from_static("en-US"));
        h.insert("Referer", HeaderValue::from_static("https://flipster.io/"));
        h.insert("X-Prex-Client-Platform", HeaderValue::from_static("web"));
        h.insert("X-Prex-Client-Version", HeaderValue::from_static("release-web-3.15.110"));
        h.insert("Origin", HeaderValue::from_static("https://flipster.io"));
        h.insert("Sec-GPC", HeaderValue::from_static("1"));
        h.insert("Sec-Fetch-Dest", HeaderValue::from_static("empty"));
        h.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
        h.insert("Sec-Fetch-Site", HeaderValue::from_static("same-site"));
        h
    }

    /// Update the Cloudflare __cf_bm cookie without recreating the client.
    /// cf_bm expires every ~30 minutes.
    pub async fn update_cf_bm(&self, new_cf_bm: &str) {
        let api_url: Url = API_BASE.parse().unwrap();
        self.jar.add_cookie_str(
            &format!("__cf_bm={new_cf_bm}; Domain=api.flipster.io; Path=/"),
            &api_url,
        );
        *self.cf_bm.write().await = new_cf_bm.to_string();
    }

    fn build_order_body(params: &OrderParams) -> OrderRequestBody {
        let now_nanos = chrono::Utc::now()
            .timestamp_nanos_opt()
            .expect("timestamp_nanos overflow") as u64;
        let ts = now_nanos.to_string();

        OrderRequestBody {
            side: params.side,
            request_id: uuid::Uuid::new_v4().to_string(),
            timestamp: ts.clone(),
            ref_server_timestamp: ts.clone(),
            ref_client_timestamp: ts,
            leverage: params.leverage,
            price: params.price.to_string(),
            amount: params.amount.to_string(),
            attribution: "SEARCH_PERPETUAL",
            margin_type: params.margin_type,
            order_type: params.order_type,
        }
    }

    /// Place an order. Returns the parsed API response.
    pub async fn place_order(
        &self,
        symbol: &str,
        params: OrderParams,
    ) -> Result<OrderResponse, FlipsterError> {
        let body = Self::build_order_body(&params);
        let url = format!("{API_BASE}/api/v2/trade/positions/{symbol}");

        if self.dry_run {
            let json = serde_json::to_value(&body).unwrap();
            return Ok(OrderResponse { raw: json });
        }

        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status().as_u16();

        if status == 401 || status == 403 {
            return Err(FlipsterError::Auth);
        }

        let text = resp.text().await?;

        if status >= 400 {
            return Err(FlipsterError::Api {
                status,
                message: text,
            });
        }

        let parsed: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
        Ok(OrderResponse { raw: parsed })
    }

    /// Close a position by setting size=0.
    /// PUT /api/v2/trade/positions/{symbol}/{slot}/size
    pub async fn close_position(
        &self,
        symbol: &str,
        slot: u32,
        price: f64,
    ) -> Result<OrderResponse, FlipsterError> {
        let now_nanos = chrono::Utc::now()
            .timestamp_nanos_opt()
            .expect("timestamp_nanos overflow") as u64;
        let ts = now_nanos.to_string();

        let body = serde_json::json!({
            "requestId": uuid::Uuid::new_v4().to_string(),
            "timestamp": ts,
            "refServerTimestamp": ts,
            "refClientTimestamp": ts,
            "size": "0",
            "price": price.to_string(),
            "attribution": "SEARCH_PERPETUAL",
            "orderType": "ORDER_TYPE_MARKET",
        });

        let url = format!("{API_BASE}/api/v2/trade/positions/{symbol}/{slot}/size");

        if self.dry_run {
            return Ok(OrderResponse { raw: body });
        }

        let resp = self.http.put(&url).json(&body).send().await?;
        let status = resp.status().as_u16();

        if status == 401 || status == 403 {
            return Err(FlipsterError::Auth);
        }

        let text = resp.text().await?;

        if status >= 400 {
            return Err(FlipsterError::Api {
                status,
                message: text,
            });
        }

        let parsed: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
        Ok(OrderResponse { raw: parsed })
    }

    /// Send a raw JSON body to the order endpoint for debugging.
    pub async fn raw_request(
        &self,
        symbol: &str,
        body: serde_json::Value,
    ) -> Result<OrderResponse, FlipsterError> {
        let url = format!("{API_BASE}/api/v2/trade/positions/{symbol}");

        if self.dry_run {
            return Ok(OrderResponse { raw: body });
        }

        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status().as_u16();

        if status == 401 || status == 403 {
            return Err(FlipsterError::Auth);
        }

        let text = resp.text().await?;

        if status >= 400 {
            return Err(FlipsterError::Api {
                status,
                message: text,
            });
        }

        let parsed: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
        Ok(OrderResponse { raw: parsed })
    }
}

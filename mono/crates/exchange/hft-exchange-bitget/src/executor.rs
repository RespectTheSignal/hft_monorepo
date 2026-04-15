//! Bitget USDT-M Futures (v2) 주문 executor.
//!
//! ## 엔드포인트
//! - Base: `https://api.bitget.com`
//! - Place: `POST /api/v2/mix/order/place-order`
//! - Cancel: `POST /api/v2/mix/order/cancel-order`
//!
//! ## 서명 스펙 (Bitget v2)
//!
//! ```text
//! pre_sign = timestamp + method_upper + path_with_query + body
//! sign     = Base64( HMAC_SHA256( api_secret, pre_sign ) )
//! ```
//!
//! - `timestamp` = epoch **ms** 문자열.
//! - `method_upper` = "GET" / "POST" / "DELETE".
//! - `path_with_query` — path + (GET 이면 `?query`).
//! - `body` — POST 의 JSON (GET 이면 빈 문자열).
//!
//! 헤더:
//! - `ACCESS-KEY: <api_key>`
//! - `ACCESS-SIGN: <base64 sign>`
//! - `ACCESS-TIMESTAMP: <ms>`
//! - `ACCESS-PASSPHRASE: <passphrase>`
//! - `locale: en-US`
//! - `Content-Type: application/json`
//!
//! ## OrderRequest → Bitget 매핑
//! | HFT | Bitget JSON | 규칙 |
//! |-----|-------------|------|
//! | symbol | `symbol` | `BTC_USDT` → `BTCUSDT` (대문자) |
//! | productType | 상수 `"USDT-FUTURES"` | Standard USDT-margined futures |
//! | marginMode | 상수 `"crossed"` | hedge/isolated 는 V2 |
//! | side | `side` | `buy` / `sell` — 소문자 |
//! | orderType | `orderType` | `limit` / `market` |
//! | size | `size` | 문자열 |
//! | price | `price` | 문자열, limit 만 |
//! | tif | `force` | `gtc` / `ioc` / `fok` 소문자 |
//! | client_id | `clientOid` | |

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Method;
use serde::Deserialize;
use tracing::debug;

use hft_exchange_api::{
    ApiError, ExchangeExecutor, OrderAck, OrderRequest, OrderSide, OrderType, TimeInForce,
};
use hft_exchange_rest::{
    headers_from_pairs, hmac_sha256_base64, now_epoch_ms, Credentials, RestClient,
};
use hft_types::ExchangeId;

#[derive(Debug, Clone)]
pub struct BitgetExecutorConfig {
    pub rest_base: String,
    pub place_path: String,
    pub cancel_path: String,
    pub product_type: String,
    pub margin_mode: String,
    pub margin_coin: String,
    pub locale: String,
}

impl Default for BitgetExecutorConfig {
    fn default() -> Self {
        Self {
            rest_base: "https://api.bitget.com".into(),
            place_path: "/api/v2/mix/order/place-order".into(),
            cancel_path: "/api/v2/mix/order/cancel-order".into(),
            product_type: "USDT-FUTURES".into(),
            margin_mode: "crossed".into(),
            margin_coin: "USDT".into(),
            locale: "en-US".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BitgetExecutor {
    pub cfg: BitgetExecutorConfig,
    pub creds: Credentials,
    pub http: RestClient,
}

impl BitgetExecutor {
    pub fn new(creds: Credentials) -> Result<Self, ApiError> {
        creds.validate()?;
        if creds.passphrase.is_none() {
            return Err(ApiError::Auth("bitget requires passphrase".into()));
        }
        Ok(Self {
            cfg: BitgetExecutorConfig::default(),
            creds,
            http: RestClient::new()?,
        })
    }

    pub fn with_config(mut self, cfg: BitgetExecutorConfig) -> Self {
        self.cfg = cfg;
        self
    }

    pub fn with_http(mut self, http: RestClient) -> Self {
        self.http = http;
        self
    }

    fn build_place_body(&self, req: &OrderRequest) -> Result<String, ApiError> {
        req.basic_validate()?;
        let symbol = req.symbol.as_str().replace('_', "").to_ascii_uppercase();
        let side = match req.side {
            OrderSide::Buy => "buy",
            OrderSide::Sell => "sell",
        };
        let client_oid = sanitize_client_oid(&req.client_id);

        let mut body = serde_json::Map::new();
        body.insert("symbol".into(), symbol.into());
        body.insert("productType".into(), self.cfg.product_type.clone().into());
        body.insert("marginMode".into(), self.cfg.margin_mode.clone().into());
        body.insert("marginCoin".into(), self.cfg.margin_coin.clone().into());
        body.insert("side".into(), side.into());
        body.insert("size".into(), format_num(req.qty).into());
        body.insert("clientOid".into(), client_oid.into());

        match req.order_type {
            OrderType::Market => {
                body.insert("orderType".into(), "market".into());
                body.insert("force".into(), "ioc".into());
            }
            OrderType::Limit => {
                let p = req.price.ok_or_else(|| {
                    ApiError::InvalidOrder("limit without price".into())
                })?;
                if !p.is_finite() || p <= 0.0 {
                    return Err(ApiError::InvalidOrder(format!("bad price: {p}")));
                }
                body.insert("orderType".into(), "limit".into());
                body.insert("price".into(), format_num(p).into());
                body.insert("force".into(), tif_string(req.tif).into());
            }
        }
        serde_json::to_string(&serde_json::Value::Object(body))
            .map_err(|e| ApiError::Decode(format!("bitget body: {e}")))
    }

    fn sign(&self, timestamp_ms: i64, method: &str, path_with_query: &str, body: &str) -> String {
        let pre = format!("{}{}{}{}", timestamp_ms, method, path_with_query, body);
        hmac_sha256_base64(self.creds.api_secret.as_bytes(), pre.as_bytes())
    }

    fn auth_headers(
        &self,
        timestamp_ms: i64,
        sign: &str,
    ) -> Result<reqwest::header::HeaderMap, ApiError> {
        let pass = self
            .creds
            .passphrase
            .as_ref()
            .ok_or_else(|| ApiError::Auth("passphrase missing".into()))?;
        headers_from_pairs([
            ("ACCESS-KEY", self.creds.api_key.as_ref()),
            ("ACCESS-SIGN", sign),
            ("ACCESS-TIMESTAMP", timestamp_ms.to_string().as_str()),
            ("ACCESS-PASSPHRASE", pass.as_ref()),
            ("locale", self.cfg.locale.as_str()),
        ])
    }

    async fn do_place(&self, req: OrderRequest) -> Result<OrderAck, ApiError> {
        self.creds.validate()?;
        let body = self.build_place_body(&req)?;
        let ts = now_epoch_ms();
        let sign = self.sign(ts, "POST", &self.cfg.place_path, &body);
        let url = format!("{}{}", self.cfg.rest_base, self.cfg.place_path);
        let headers = self.auth_headers(ts, &sign)?;

        debug!(
            target: "bitget::executor",
            symbol = req.symbol.as_str(),
            "placing order"
        );

        let resp = self.http.send(Method::POST, &url, &headers, Some(body)).await?;
        let resp_body = resp.into_ok_body()?;
        parse_place_response(&resp_body, req.exchange, req.client_id.clone())
    }

    async fn do_cancel(&self, exchange_order_id: &str) -> Result<(), ApiError> {
        self.creds.validate()?;
        let (symbol, order_id) = split_symbol_id(exchange_order_id)?;
        let body_json = serde_json::json!({
            "symbol": symbol,
            "productType": self.cfg.product_type,
            "marginCoin": self.cfg.margin_coin,
            "orderId": order_id,
        });
        let body = serde_json::to_string(&body_json)
            .map_err(|e| ApiError::Decode(format!("bitget cancel body: {e}")))?;
        let ts = now_epoch_ms();
        let sign = self.sign(ts, "POST", &self.cfg.cancel_path, &body);
        let url = format!("{}{}", self.cfg.rest_base, self.cfg.cancel_path);
        let headers = self.auth_headers(ts, &sign)?;

        let resp = self.http.send(Method::POST, &url, &headers, Some(body)).await?;
        let resp_body = resp.into_ok_body()?;
        parse_ack_only(&resp_body)
    }
}

#[async_trait]
impl ExchangeExecutor for BitgetExecutor {
    fn id(&self) -> ExchangeId {
        ExchangeId::Bitget
    }

    async fn place_order(&self, req: OrderRequest) -> anyhow::Result<OrderAck> {
        Ok(self.do_place(req).await?)
    }

    async fn cancel(&self, exchange_order_id: &str) -> anyhow::Result<()> {
        Ok(self.do_cancel(exchange_order_id).await?)
    }

    fn label(&self) -> String {
        "bitget:v2_mix".into()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 응답
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BitgetEnvelope {
    code: String,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    #[serde(rename = "requestTime")]
    request_time: Option<i64>,
    #[serde(default)]
    data: Option<BitgetPlaceData>,
}

#[derive(Debug, Deserialize)]
struct BitgetPlaceData {
    #[serde(rename = "orderId", default)]
    order_id: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "clientOid", default)]
    client_oid: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
}

fn parse_place_response(
    body: &str,
    exchange: ExchangeId,
    client_id: Arc<str>,
) -> Result<OrderAck, ApiError> {
    let env: BitgetEnvelope = serde_json::from_str(body)
        .map_err(|e| ApiError::Decode(format!("bitget resp: {e}")))?;
    if env.code != "00000" {
        return Err(ApiError::Rejected(format!(
            "bitget {}: {}",
            env.code, env.msg
        )));
    }
    let data = env
        .data
        .ok_or_else(|| ApiError::Decode("missing data".into()))?;
    let order_id = data
        .order_id
        .ok_or_else(|| ApiError::Decode("missing orderId".into()))?;
    // Bitget place-order 응답에는 symbol 이 없을 수 있어, 요청 symbol 을 caller 가
    // 가지고 있지 않아도 cancel 경로가 동작하도록 client_oid 는 별도 보관 안 함.
    let symbol = data.symbol.unwrap_or_else(|| "?".to_string());
    let ts_ms = env.request_time.unwrap_or_else(now_epoch_ms);
    Ok(OrderAck {
        exchange,
        exchange_order_id: format!("{symbol}:{order_id}"),
        client_id,
        ts_ms,
    })
}

fn parse_ack_only(body: &str) -> Result<(), ApiError> {
    let env: BitgetEnvelope = serde_json::from_str(body)
        .map_err(|e| ApiError::Decode(format!("bitget resp: {e}")))?;
    if env.code != "00000" {
        return Err(ApiError::Rejected(format!(
            "bitget {}: {}",
            env.code, env.msg
        )));
    }
    Ok(())
}

fn tif_string(t: TimeInForce) -> &'static str {
    match t {
        TimeInForce::Gtc => "gtc",
        TimeInForce::Ioc => "ioc",
        TimeInForce::Fok => "fok",
    }
}

fn format_num(x: f64) -> String {
    let raw = format!("{:.8}", x);
    let trimmed = raw.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

fn sanitize_client_oid(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(40));
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
            if out.len() >= 40 {
                break;
            }
        }
    }
    if out.is_empty() {
        out.push('x');
    }
    out
}

fn split_symbol_id(s: &str) -> Result<(String, String), ApiError> {
    let (sym, id) = s
        .split_once(':')
        .ok_or_else(|| ApiError::InvalidOrder(format!("expected SYMBOL:ID, got '{s}'")))?;
    if sym.is_empty() || id.is_empty() {
        return Err(ApiError::InvalidOrder(format!("malformed: '{s}'")));
    }
    Ok((sym.to_string(), id.to_string()))
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_types::Symbol;

    fn mk_exec() -> BitgetExecutor {
        let creds = Credentials::new("key", "secret").with_passphrase("pp");
        BitgetExecutor {
            cfg: BitgetExecutorConfig::default(),
            creds,
            http: RestClient::new().unwrap(),
        }
    }

    #[test]
    fn new_rejects_missing_passphrase() {
        let err = BitgetExecutor::new(Credentials::new("k", "s")).unwrap_err();
        match err {
            ApiError::Auth(m) => assert!(m.contains("passphrase")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn sign_is_base64() {
        let exec = mk_exec();
        let s = exec.sign(1_700_000_000_000, "POST", "/api/v2/mix/order/place-order", "{}");
        // Base64 표준은 == 또는 = 로 padding. HMAC-SHA256 → 32bytes → 44 char base64.
        assert_eq!(s.len(), 44);
        assert!(s.ends_with('='));
    }

    #[test]
    fn build_place_body_market_ioc() {
        let exec = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Bitget,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            qty: 0.1,
            price: None,
            tif: TimeInForce::Gtc, // 무시 — market 은 ioc
            client_id: Arc::from("oid1"),
        };
        let body = exec.build_place_body(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["orderType"], "market");
        assert_eq!(v["force"], "ioc");
        assert_eq!(v["side"], "buy");
        assert_eq!(v["symbol"], "BTCUSDT");
        assert_eq!(v["size"], "0.1");
        assert_eq!(v["productType"], "USDT-FUTURES");
    }

    #[test]
    fn build_place_body_limit_passes_price_and_force() {
        let exec = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Bitget,
            symbol: Symbol::new("ETH_USDT"),
            side: OrderSide::Sell,
            order_type: OrderType::Limit,
            qty: 3.0,
            price: Some(2_500.0),
            tif: TimeInForce::Fok,
            client_id: Arc::from("oid2"),
        };
        let body = exec.build_place_body(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["price"], "2500");
        assert_eq!(v["force"], "fok");
    }

    #[test]
    fn parse_place_response_reject_on_nonzero_code() {
        let body = r#"{"code":"40408","msg":"order not found"}"#;
        let err =
            parse_place_response(body, ExchangeId::Bitget, Arc::from("c")).unwrap_err();
        match err {
            ApiError::Rejected(m) => assert!(m.contains("40408")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn parse_place_response_happy() {
        let body = r#"{"code":"00000","msg":"success","requestTime":1700000000000,"data":{"orderId":"999","clientOid":"oid","symbol":"BTCUSDT"}}"#;
        let ack = parse_place_response(body, ExchangeId::Bitget, Arc::from("oid")).unwrap();
        assert_eq!(ack.exchange_order_id, "BTCUSDT:999");
        assert_eq!(ack.ts_ms, 1_700_000_000_000);
    }
}

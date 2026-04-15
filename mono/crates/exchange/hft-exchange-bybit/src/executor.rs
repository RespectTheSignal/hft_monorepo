//! Bybit v5 (Linear Futures / Unified Trading) 주문 executor.
//!
//! ## 엔드포인트
//! - Base: `https://api.bybit.com` (mainnet) 또는 `https://api-testnet.bybit.com`.
//! - Place: `POST /v5/order/create`
//! - Cancel: `POST /v5/order/cancel`
//!
//! ## 서명 스펙 (v5 private)
//!
//! ```text
//! pre_sign = timestamp + api_key + recv_window + param_str
//! sign     = HEX_LOWER( HMAC_SHA256( api_secret, pre_sign ) )
//! ```
//!
//! - `POST` 는 `param_str = <JSON body>`.
//! - `GET/DELETE` 는 `param_str = <sorted query string>`.
//! - `timestamp` 는 현재 epoch ms 문자열.
//! - `recv_window` 는 응답 허용 지연 — 5000 기본.
//!
//! 헤더:
//! - `X-BAPI-API-KEY: <key>`
//! - `X-BAPI-TIMESTAMP: <ms>`
//! - `X-BAPI-RECV-WINDOW: <ms>`
//! - `X-BAPI-SIGN: <hex>`
//! - `Content-Type: application/json`
//!
//! ## OrderRequest → Bybit 매핑
//! | HFT | Bybit JSON | 규칙 |
//! |-----|-----------|------|
//! | symbol | `symbol` | `BTC_USDT` → `BTCUSDT` |
//! | category | 상수 `"linear"` | USDT perpetual |
//! | side | `side` | `Buy` / `Sell` (first letter capital) |
//! | order_type Market | `orderType: "Market"` | price 생략 |
//! | order_type Limit | `orderType: "Limit"` + `price` | |
//! | qty | `qty` (문자열) | |
//! | tif | `timeInForce` | `GTC` / `IOC` / `FOK` |
//! | client_id | `orderLinkId` | 36자 이하 |
//!
//! ## Bybit error handling
//!
//! 응답이 항상 `retCode: 0 = success`. 그 외는 reject.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Method;
use serde::Deserialize;
use tracing::debug;

use hft_exchange_api::{
    ApiError, ExchangeExecutor, OrderAck, OrderRequest, OrderSide, OrderType, TimeInForce,
};
use hft_exchange_rest::{
    headers_from_pairs, hmac_sha256_hex, now_epoch_ms, Credentials, RestClient,
};
use hft_types::ExchangeId;

#[derive(Debug, Clone)]
pub struct BybitExecutorConfig {
    pub rest_base: String,
    pub place_path: String,
    pub cancel_path: String,
    pub recv_window_ms: u64,
    /// `linear`, `inverse`, `spot`, `option`.
    pub category: String,
}

impl Default for BybitExecutorConfig {
    fn default() -> Self {
        Self {
            rest_base: "https://api.bybit.com".into(),
            place_path: "/v5/order/create".into(),
            cancel_path: "/v5/order/cancel".into(),
            recv_window_ms: 5_000,
            category: "linear".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BybitExecutor {
    pub cfg: BybitExecutorConfig,
    pub creds: Credentials,
    pub http: RestClient,
}

impl BybitExecutor {
    pub fn new(creds: Credentials) -> Result<Self, ApiError> {
        creds.validate()?;
        Ok(Self {
            cfg: BybitExecutorConfig::default(),
            creds,
            http: RestClient::new()?,
        })
    }

    pub fn with_config(mut self, cfg: BybitExecutorConfig) -> Self {
        self.cfg = cfg;
        self
    }

    pub fn with_http(mut self, http: RestClient) -> Self {
        self.http = http;
        self
    }

    /// `param_str` 조립 (POST 는 JSON body 그대로).
    fn build_place_body(&self, req: &OrderRequest) -> Result<String, ApiError> {
        req.basic_validate()?;
        let symbol = req.symbol.as_str().replace('_', "").to_ascii_uppercase();
        let side = match req.side {
            OrderSide::Buy => "Buy",
            OrderSide::Sell => "Sell",
        };
        let client_id = sanitize_link_id(&req.client_id);

        let mut body = serde_json::Map::new();
        body.insert("category".into(), self.cfg.category.clone().into());
        body.insert("symbol".into(), symbol.into());
        body.insert("side".into(), side.into());
        body.insert("qty".into(), format_num(req.qty).into());
        body.insert("orderLinkId".into(), client_id.into());

        match req.order_type {
            OrderType::Market => {
                body.insert("orderType".into(), "Market".into());
            }
            OrderType::Limit => {
                let p = req.price.ok_or_else(|| {
                    ApiError::InvalidOrder("limit without price".into())
                })?;
                if !p.is_finite() || p <= 0.0 {
                    return Err(ApiError::InvalidOrder(format!("bad price: {p}")));
                }
                body.insert("orderType".into(), "Limit".into());
                body.insert("price".into(), format_num(p).into());
                body.insert("timeInForce".into(), tif_string(req.tif).into());
            }
        }
        serde_json::to_string(&serde_json::Value::Object(body))
            .map_err(|e| ApiError::Decode(format!("bybit body: {e}")))
    }

    fn sign(&self, timestamp_ms: i64, recv_window: u64, param_str: &str) -> String {
        let pre = format!(
            "{}{}{}{}",
            timestamp_ms, self.creds.api_key, recv_window, param_str
        );
        hmac_sha256_hex(self.creds.api_secret.as_bytes(), pre.as_bytes())
    }

    async fn do_place(&self, req: OrderRequest) -> Result<OrderAck, ApiError> {
        self.creds.validate()?;
        let body = self.build_place_body(&req)?;
        let ts = now_epoch_ms();
        let sign = self.sign(ts, self.cfg.recv_window_ms, &body);

        let url = format!("{}{}", self.cfg.rest_base, self.cfg.place_path);
        let headers = headers_from_pairs([
            ("X-BAPI-API-KEY", self.creds.api_key.as_ref()),
            ("X-BAPI-TIMESTAMP", ts.to_string().as_str()),
            (
                "X-BAPI-RECV-WINDOW",
                self.cfg.recv_window_ms.to_string().as_str(),
            ),
            ("X-BAPI-SIGN", sign.as_str()),
        ])?;

        debug!(
            target: "bybit::executor",
            symbol = req.symbol.as_str(),
            side = ?req.side,
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
            "category": self.cfg.category,
            "symbol": symbol,
            "orderId": order_id,
        });
        let body = serde_json::to_string(&body_json)
            .map_err(|e| ApiError::Decode(format!("bybit cancel body: {e}")))?;
        let ts = now_epoch_ms();
        let sign = self.sign(ts, self.cfg.recv_window_ms, &body);

        let url = format!("{}{}", self.cfg.rest_base, self.cfg.cancel_path);
        let headers = headers_from_pairs([
            ("X-BAPI-API-KEY", self.creds.api_key.as_ref()),
            ("X-BAPI-TIMESTAMP", ts.to_string().as_str()),
            (
                "X-BAPI-RECV-WINDOW",
                self.cfg.recv_window_ms.to_string().as_str(),
            ),
            ("X-BAPI-SIGN", sign.as_str()),
        ])?;

        let resp = self.http.send(Method::POST, &url, &headers, Some(body)).await?;
        let resp_body = resp.into_ok_body()?;
        // Bybit 의 "not found" 는 retCode 170213 등 — reject 로 돌아오므로 에러 전파.
        parse_ack_only(&resp_body)
    }
}

#[async_trait]
impl ExchangeExecutor for BybitExecutor {
    fn id(&self) -> ExchangeId {
        ExchangeId::Bybit
    }

    async fn place_order(&self, req: OrderRequest) -> anyhow::Result<OrderAck> {
        Ok(self.do_place(req).await?)
    }

    async fn cancel(&self, exchange_order_id: &str) -> anyhow::Result<()> {
        Ok(self.do_cancel(exchange_order_id).await?)
    }

    fn label(&self) -> String {
        "bybit:v5_linear".into()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 응답
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BybitEnvelope {
    #[serde(rename = "retCode")]
    ret_code: i64,
    #[serde(rename = "retMsg", default)]
    ret_msg: String,
    #[serde(default)]
    result: Option<BybitResult>,
    #[serde(default)]
    time: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct BybitResult {
    #[serde(rename = "orderId", default)]
    order_id: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "orderLinkId", default)]
    order_link_id: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
}

fn parse_place_response(
    body: &str,
    exchange: ExchangeId,
    client_id: Arc<str>,
) -> Result<OrderAck, ApiError> {
    let env: BybitEnvelope = serde_json::from_str(body)
        .map_err(|e| ApiError::Decode(format!("bybit resp: {e}")))?;
    if env.ret_code != 0 {
        return Err(ApiError::Rejected(format!(
            "bybit {}: {}",
            env.ret_code, env.ret_msg
        )));
    }
    let result = env
        .result
        .ok_or_else(|| ApiError::Decode("missing result".into()))?;
    let order_id = result
        .order_id
        .ok_or_else(|| ApiError::Decode("missing orderId".into()))?;
    let symbol = result.symbol.unwrap_or_else(|| "?".to_string());
    let ts_ms = env.time.unwrap_or_else(now_epoch_ms);
    Ok(OrderAck {
        exchange,
        exchange_order_id: format!("{symbol}:{order_id}"),
        client_id,
        ts_ms,
    })
}

fn parse_ack_only(body: &str) -> Result<(), ApiError> {
    let env: BybitEnvelope = serde_json::from_str(body)
        .map_err(|e| ApiError::Decode(format!("bybit resp: {e}")))?;
    if env.ret_code != 0 {
        return Err(ApiError::Rejected(format!(
            "bybit {}: {}",
            env.ret_code, env.ret_msg
        )));
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// 보조
// ────────────────────────────────────────────────────────────────────────────

fn tif_string(t: TimeInForce) -> &'static str {
    match t {
        TimeInForce::Gtc => "GTC",
        TimeInForce::Ioc => "IOC",
        TimeInForce::Fok => "FOK",
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

fn sanitize_link_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(36));
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
            if out.len() >= 36 {
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

    fn mk_exec() -> BybitExecutor {
        BybitExecutor {
            cfg: BybitExecutorConfig::default(),
            creds: Credentials::new("key", "secret"),
            http: RestClient::new().unwrap(),
        }
    }

    #[test]
    fn sign_preserves_order_of_components() {
        let exec = mk_exec();
        // pre = "1700000000000" + "key" + "5000" + "{}"
        let s = exec.sign(1_700_000_000_000, 5_000, "{}");
        assert_eq!(s.len(), 64);
        // 동일 입력이면 결과 stable.
        let s2 = exec.sign(1_700_000_000_000, 5_000, "{}");
        assert_eq!(s, s2);
        // 다른 입력이면 다르다.
        let s3 = exec.sign(1_700_000_000_001, 5_000, "{}");
        assert_ne!(s, s3);
    }

    #[test]
    fn build_place_body_market() {
        let exec = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Bybit,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            qty: 0.1,
            price: None,
            tif: TimeInForce::Ioc,
            client_id: Arc::from("link1"),
        };
        let body = exec.build_place_body(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["category"], "linear");
        assert_eq!(v["symbol"], "BTCUSDT");
        assert_eq!(v["side"], "Buy");
        assert_eq!(v["orderType"], "Market");
        assert_eq!(v["qty"], "0.1");
        assert_eq!(v["orderLinkId"], "link1");
        assert!(v.get("price").is_none());
        // Market 이면 timeInForce 는 불필요 — Bybit 는 허용.
        assert!(v.get("timeInForce").is_none());
    }

    #[test]
    fn build_place_body_limit_includes_price_and_tif() {
        let exec = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Bybit,
            symbol: Symbol::new("ETH_USDT"),
            side: OrderSide::Sell,
            order_type: OrderType::Limit,
            qty: 1.25,
            price: Some(3_500.0),
            tif: TimeInForce::Fok,
            client_id: Arc::from("c2"),
        };
        let body = exec.build_place_body(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["price"], "3500");
        assert_eq!(v["timeInForce"], "FOK");
        assert_eq!(v["side"], "Sell");
    }

    #[test]
    fn parse_place_response_happy() {
        let body = r#"{"retCode":0,"retMsg":"OK","result":{"orderId":"ord-1","orderLinkId":"link1","symbol":"BTCUSDT"},"time":1700000000000}"#;
        let ack =
            parse_place_response(body, ExchangeId::Bybit, Arc::from("link1")).unwrap();
        assert_eq!(ack.exchange_order_id, "BTCUSDT:ord-1");
        assert_eq!(ack.ts_ms, 1_700_000_000_000);
    }

    #[test]
    fn parse_place_response_reject_on_retcode() {
        let body = r#"{"retCode":10001,"retMsg":"param error","result":null}"#;
        let err =
            parse_place_response(body, ExchangeId::Bybit, Arc::from("c")).unwrap_err();
        match err {
            ApiError::Rejected(m) => assert!(m.contains("10001")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_link_id_limits_length_and_fills_empty() {
        let s = sanitize_link_id(" bad id!!!! this is very very very very long abcdef");
        assert!(s.len() <= 36);
        assert!(!s.contains(' '));
        assert_eq!(sanitize_link_id(""), "x");
    }

    #[test]
    fn split_symbol_id_validates() {
        assert!(split_symbol_id("NOMATCH").is_err());
        let (s, i) = split_symbol_id("BTCUSDT:abc").unwrap();
        assert_eq!(s, "BTCUSDT");
        assert_eq!(i, "abc");
    }
}

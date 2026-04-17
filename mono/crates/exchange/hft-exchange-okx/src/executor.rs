//! OKX v5 (SWAP / Futures) 주문 executor.
//!
//! ## 엔드포인트
//! - Base: `https://www.okx.com`
//! - Place: `POST /api/v5/trade/order`
//! - Cancel: `POST /api/v5/trade/cancel-order`
//!
//! ## 서명 스펙 (OKX v5)
//!
//! ```text
//! pre_sign = timestamp + method + path + body
//! sign     = Base64( HMAC_SHA256( api_secret, pre_sign ) )
//! ```
//!
//! - `timestamp` = ISO 8601 UTC **ms** 해상도: `2024-01-01T12:34:56.789Z`.
//! - `method` = uppercase.
//! - `path` 는 query 포함 (`/api/v5/...?a=1`). POST 는 query 없음.
//! - `body` = POST 는 그대로, GET 은 빈 문자열.
//!
//! 헤더:
//! - `OK-ACCESS-KEY: <api_key>`
//! - `OK-ACCESS-SIGN: <base64>`
//! - `OK-ACCESS-TIMESTAMP: <iso 8601>`
//! - `OK-ACCESS-PASSPHRASE: <passphrase>`
//! - `Content-Type: application/json`
//!
//! ## OrderRequest → OKX 매핑
//! OKX 는 instrument 를 `BTC-USDT-SWAP` 처럼 3-part 로 부른다. 이 구현은
//! 심볼을 "BTC_USDT" 형태로 받아 `BTC-USDT-SWAP` 으로 변환한다 (SWAP 이
//! USDT-margined perpetual 에 해당).
//!
//! | HFT | OKX JSON | 규칙 |
//! |-----|----------|------|
//! | symbol "BTC_USDT" | `instId` | "BTC-USDT-SWAP" |
//! | tdMode 상수 `"cross"` | `tdMode` | |
//! | side | `side` | `buy`/`sell` |
//! | order_type Market | `ordType`: `"market"` | price 생략 |
//! | order_type Limit | `ordType`: `"limit"` + `px` + timeInForce 변형 |
//! | tif | OKX 는 ordType 자체로 구분 (`"limit"`, `"ioc"`, `"fok"`, `"post_only"`) — |
//! |     |   GTC → "limit", IOC → "ioc", FOK → "fok" |
//! | qty | `sz` | 문자열. OKX 에서는 contract count 또는 base 수량 (tdMode 에 따라) |
//! | client_id | `clOrdId` | |

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
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
pub struct OkxExecutorConfig {
    pub rest_base: String,
    pub place_path: String,
    pub cancel_path: String,
    /// `cross`, `isolated`, `cash`.
    pub td_mode: String,
    /// `SWAP`, `FUTURES`, `SPOT`, `MARGIN`. SWAP = USDT perpetual.
    pub instrument_suffix: String,
}

impl Default for OkxExecutorConfig {
    fn default() -> Self {
        Self {
            rest_base: "https://www.okx.com".into(),
            place_path: "/api/v5/trade/order".into(),
            cancel_path: "/api/v5/trade/cancel-order".into(),
            td_mode: "cross".into(),
            instrument_suffix: "SWAP".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OkxExecutor {
    pub cfg: OkxExecutorConfig,
    pub creds: Credentials,
    pub http: RestClient,
}

impl OkxExecutor {
    pub fn new(creds: Credentials) -> Result<Self, ApiError> {
        creds.validate()?;
        if creds.passphrase.is_none() {
            return Err(ApiError::Auth("okx requires passphrase".into()));
        }
        Ok(Self {
            cfg: OkxExecutorConfig::default(),
            creds,
            http: RestClient::new()?,
        })
    }

    pub fn with_config(mut self, cfg: OkxExecutorConfig) -> Self {
        self.cfg = cfg;
        self
    }

    pub fn with_http(mut self, http: RestClient) -> Self {
        self.http = http;
        self
    }

    /// "BTC_USDT" → "BTC-USDT-SWAP"
    fn to_inst_id(&self, canonical: &str) -> String {
        let dash = canonical.replace('_', "-").to_ascii_uppercase();
        format!("{}-{}", dash, self.cfg.instrument_suffix)
    }

    /// OKX ordType 매핑. GTC=limit, IOC/FOK 는 ordType 자체가 바뀐다.
    /// Market 은 항상 `market`.
    fn ord_type_for(req: &OrderRequest) -> &'static str {
        match req.order_type {
            OrderType::Market => "market",
            OrderType::Limit => match req.tif {
                TimeInForce::Gtc => "limit",
                TimeInForce::Ioc => "ioc",
                TimeInForce::Fok => "fok",
            },
        }
    }

    fn build_place_body(&self, req: &OrderRequest) -> Result<String, ApiError> {
        req.basic_validate()?;
        let inst_id = self.to_inst_id(req.symbol.as_str());
        let side = match req.side {
            OrderSide::Buy => "buy",
            OrderSide::Sell => "sell",
        };
        let cl_ord_id = sanitize_cl_ord_id(&req.client_id);

        let mut body = serde_json::Map::new();
        body.insert("instId".into(), inst_id.into());
        body.insert("tdMode".into(), self.cfg.td_mode.clone().into());
        body.insert("side".into(), side.into());
        body.insert("ordType".into(), Self::ord_type_for(req).into());
        body.insert("sz".into(), format_num(req.qty).into());
        body.insert("clOrdId".into(), cl_ord_id.into());
        body.insert("reduceOnly".into(), req.reduce_only.into());

        if matches!(req.order_type, OrderType::Limit) {
            let p = req
                .price
                .ok_or_else(|| ApiError::InvalidOrder("limit without price".into()))?;
            if !p.is_finite() || p <= 0.0 {
                return Err(ApiError::InvalidOrder(format!("bad price: {p}")));
            }
            body.insert("px".into(), format_num(p).into());
        }
        serde_json::to_string(&serde_json::Value::Object(body))
            .map_err(|e| ApiError::Decode(format!("okx body: {e}")))
    }

    /// OKX 서명에 쓰이는 ISO 8601 ms 해상도 timestamp. 테스트에서 주입할 수
    /// 있도록 순수 함수로 노출.
    pub fn iso_timestamp_now() -> String {
        Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    fn sign(&self, timestamp: &str, method: &str, path: &str, body: &str) -> String {
        let pre = format!("{}{}{}{}", timestamp, method, path, body);
        hmac_sha256_base64(self.creds.api_secret.as_bytes(), pre.as_bytes())
    }

    fn auth_headers(
        &self,
        timestamp: &str,
        sign: &str,
    ) -> Result<reqwest::header::HeaderMap, ApiError> {
        let pass = self
            .creds
            .passphrase
            .as_ref()
            .ok_or_else(|| ApiError::Auth("passphrase missing".into()))?;
        headers_from_pairs([
            ("OK-ACCESS-KEY", self.creds.api_key.as_ref()),
            ("OK-ACCESS-SIGN", sign),
            ("OK-ACCESS-TIMESTAMP", timestamp),
            ("OK-ACCESS-PASSPHRASE", pass.as_ref()),
        ])
    }

    async fn do_place(&self, req: OrderRequest) -> Result<OrderAck, ApiError> {
        self.creds.validate()?;
        let body = self.build_place_body(&req)?;
        let ts = Self::iso_timestamp_now();
        let sign = self.sign(&ts, "POST", &self.cfg.place_path, &body);
        let url = format!("{}{}", self.cfg.rest_base, self.cfg.place_path);
        let headers = self.auth_headers(&ts, &sign)?;

        debug!(
            target: "okx::executor",
            inst = %self.to_inst_id(req.symbol.as_str()),
            "placing order"
        );

        let resp = self
            .http
            .send(Method::POST, &url, &headers, Some(body))
            .await?;
        let resp_body = resp.into_ok_body()?;
        parse_place_response(
            &resp_body,
            req.exchange,
            req.client_id.clone(),
            &self.to_inst_id(req.symbol.as_str()),
        )
    }

    async fn do_cancel(&self, exchange_order_id: &str) -> Result<(), ApiError> {
        self.creds.validate()?;
        let (inst_id, order_id) = split_inst_id(exchange_order_id)?;
        let body_json = serde_json::json!({
            "instId": inst_id,
            "ordId": order_id,
        });
        let body = serde_json::to_string(&body_json)
            .map_err(|e| ApiError::Decode(format!("okx cancel body: {e}")))?;
        let ts = Self::iso_timestamp_now();
        let sign = self.sign(&ts, "POST", &self.cfg.cancel_path, &body);
        let url = format!("{}{}", self.cfg.rest_base, self.cfg.cancel_path);
        let headers = self.auth_headers(&ts, &sign)?;

        let resp = self
            .http
            .send(Method::POST, &url, &headers, Some(body))
            .await?;
        let resp_body = resp.into_ok_body()?;
        parse_ack_only(&resp_body)
    }
}

#[async_trait]
impl ExchangeExecutor for OkxExecutor {
    fn id(&self) -> ExchangeId {
        ExchangeId::Okx
    }

    async fn place_order(&self, req: OrderRequest) -> anyhow::Result<OrderAck> {
        Ok(self.do_place(req).await?)
    }

    async fn cancel(&self, exchange_order_id: &str) -> anyhow::Result<()> {
        Ok(self.do_cancel(exchange_order_id).await?)
    }

    fn label(&self) -> String {
        "okx:v5_swap".into()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 응답
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OkxEnvelope {
    code: String,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Vec<OkxOrderResult>,
}

#[derive(Debug, Deserialize)]
struct OkxOrderResult {
    #[serde(rename = "ordId", default)]
    ord_id: Option<String>,
    #[allow(dead_code)]
    #[serde(rename = "clOrdId", default)]
    cl_ord_id: Option<String>,
    #[serde(rename = "sCode", default)]
    s_code: Option<String>,
    #[serde(rename = "sMsg", default)]
    s_msg: Option<String>,
}

fn parse_place_response(
    body: &str,
    exchange: ExchangeId,
    client_id: Arc<str>,
    inst_id_hint: &str,
) -> Result<OrderAck, ApiError> {
    let env: OkxEnvelope =
        serde_json::from_str(body).map_err(|e| ApiError::Decode(format!("okx resp: {e}")))?;
    // OKX 는 envelope code 0 이어도 data[0].sCode 가 0 이 아니면 reject.
    if env.code != "0" {
        return Err(ApiError::Rejected(format!(
            "okx envelope {}: {}",
            env.code, env.msg
        )));
    }
    let first = env
        .data
        .first()
        .ok_or_else(|| ApiError::Decode("empty data[]".into()))?;
    if let Some(sc) = first.s_code.as_deref() {
        if sc != "0" {
            return Err(ApiError::Rejected(format!(
                "okx sCode {}: {}",
                sc,
                first.s_msg.clone().unwrap_or_default()
            )));
        }
    }
    let ord_id = first
        .ord_id
        .clone()
        .ok_or_else(|| ApiError::Decode("missing ordId".into()))?;
    Ok(OrderAck {
        exchange,
        exchange_order_id: format!("{inst_id_hint}:{ord_id}"),
        client_id,
        ts_ms: now_epoch_ms(),
    })
}

fn parse_ack_only(body: &str) -> Result<(), ApiError> {
    let env: OkxEnvelope =
        serde_json::from_str(body).map_err(|e| ApiError::Decode(format!("okx resp: {e}")))?;
    if env.code != "0" {
        return Err(ApiError::Rejected(format!("okx {}: {}", env.code, env.msg)));
    }
    if let Some(first) = env.data.first() {
        if let Some(sc) = first.s_code.as_deref() {
            if sc != "0" {
                return Err(ApiError::Rejected(format!(
                    "okx sCode {}: {}",
                    sc,
                    first.s_msg.clone().unwrap_or_default()
                )));
            }
        }
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// 보조
// ────────────────────────────────────────────────────────────────────────────

fn format_num(x: f64) -> String {
    let raw = format!("{:.8}", x);
    let trimmed = raw.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

fn sanitize_cl_ord_id(s: &str) -> String {
    // OKX clOrdId: alphanumeric only, 1~32 chars.
    let mut out = String::with_capacity(32);
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            if out.len() >= 32 {
                break;
            }
        }
    }
    if out.is_empty() {
        out.push('x');
    }
    out
}

/// 기대 포맷: `BTC-USDT-SWAP:ord123`.
fn split_inst_id(s: &str) -> Result<(String, String), ApiError> {
    let (inst, id) = s
        .split_once(':')
        .ok_or_else(|| ApiError::InvalidOrder(format!("expected INSTID:ID, got '{s}'")))?;
    if inst.is_empty() || id.is_empty() {
        return Err(ApiError::InvalidOrder(format!("malformed: '{s}'")));
    }
    Ok((inst.to_string(), id.to_string()))
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_types::Symbol;

    fn mk_exec() -> OkxExecutor {
        let creds = Credentials::new("key", "secret").with_passphrase("pp");
        OkxExecutor {
            cfg: OkxExecutorConfig::default(),
            creds,
            http: RestClient::new().unwrap(),
        }
    }

    #[test]
    fn inst_id_conversion() {
        let e = mk_exec();
        assert_eq!(e.to_inst_id("BTC_USDT"), "BTC-USDT-SWAP");
        assert_eq!(e.to_inst_id("eth_usdt"), "ETH-USDT-SWAP");
    }

    #[test]
    fn ord_type_mapping() {
        use hft_types::Symbol;
        let base = OrderRequest {
            exchange: ExchangeId::Okx,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            qty: 1.0,
            price: Some(100.0),
            reduce_only: false,
            tif: TimeInForce::Gtc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("c"),
        };
        assert_eq!(OkxExecutor::ord_type_for(&base), "limit");

        let mut r = base.clone();
        r.tif = TimeInForce::Ioc;
        assert_eq!(OkxExecutor::ord_type_for(&r), "ioc");

        r.tif = TimeInForce::Fok;
        assert_eq!(OkxExecutor::ord_type_for(&r), "fok");

        r.order_type = OrderType::Market;
        assert_eq!(OkxExecutor::ord_type_for(&r), "market");
    }

    #[test]
    fn build_place_body_limit_gtc() {
        let e = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Okx,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            qty: 1.0,
            price: Some(60_000.0),
            reduce_only: false,
            tif: TimeInForce::Gtc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("cl01abc"),
        };
        let body = e.build_place_body(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["instId"], "BTC-USDT-SWAP");
        assert_eq!(v["tdMode"], "cross");
        assert_eq!(v["side"], "buy");
        assert_eq!(v["ordType"], "limit");
        assert_eq!(v["sz"], "1");
        assert_eq!(v["px"], "60000");
        assert_eq!(v["clOrdId"], "cl01abc");
        assert_eq!(v["reduceOnly"], false);
    }

    #[test]
    fn build_place_body_market_omits_px() {
        let e = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Okx,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Sell,
            order_type: OrderType::Market,
            qty: 2.0,
            price: None,
            reduce_only: false,
            tif: TimeInForce::Ioc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("m1"),
        };
        let body = e.build_place_body(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ordType"], "market");
        assert!(v.get("px").is_none());
        assert_eq!(v["reduceOnly"], false);
    }

    #[test]
    fn build_place_body_includes_reduce_only_when_true() {
        let e = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Okx,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Sell,
            order_type: OrderType::Limit,
            qty: 1.0,
            price: Some(58_000.0),
            reduce_only: true,
            tif: TimeInForce::Ioc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("reduce-okx"),
        };
        let body = e.build_place_body(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["reduceOnly"], true);
    }

    #[test]
    fn sign_is_base64_44_chars() {
        let e = mk_exec();
        let s = e.sign(
            "2024-01-01T00:00:00.000Z",
            "POST",
            "/api/v5/trade/order",
            "{}",
        );
        assert_eq!(s.len(), 44);
    }

    #[test]
    fn iso_timestamp_shape() {
        let t = OkxExecutor::iso_timestamp_now();
        // 2024-01-01T00:00:00.000Z — 길이 24, 마지막 'Z'
        assert!(t.ends_with('Z'));
        // ms 자리 반드시 포함
        assert!(t.contains('.'));
    }

    #[test]
    fn parse_place_response_happy() {
        let body = r#"{"code":"0","msg":"","data":[{"ordId":"ord123","clOrdId":"cl","sCode":"0","sMsg":""}]}"#;
        let ack =
            parse_place_response(body, ExchangeId::Okx, Arc::from("cl"), "BTC-USDT-SWAP").unwrap();
        assert_eq!(ack.exchange_order_id, "BTC-USDT-SWAP:ord123");
    }

    #[test]
    fn parse_place_response_envelope_reject() {
        let body = r#"{"code":"50000","msg":"bad","data":[]}"#;
        let err = parse_place_response(body, ExchangeId::Okx, Arc::from("cl"), "BTC-USDT-SWAP")
            .unwrap_err();
        match err {
            ApiError::Rejected(m) => assert!(m.contains("50000")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn parse_place_response_inner_reject() {
        let body = r#"{"code":"0","msg":"","data":[{"sCode":"51008","sMsg":"insufficient"}]}"#;
        let err = parse_place_response(body, ExchangeId::Okx, Arc::from("cl"), "BTC-USDT-SWAP")
            .unwrap_err();
        match err {
            ApiError::Rejected(m) => assert!(m.contains("51008")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_cl_ord_id_strips_non_alnum() {
        assert_eq!(sanitize_cl_ord_id("ab_cd-12.3"), "abcd123");
        assert_eq!(sanitize_cl_ord_id(""), "x");
        let long = "a".repeat(50);
        assert_eq!(sanitize_cl_ord_id(&long).len(), 32);
    }

    #[test]
    fn split_inst_id_valid() {
        let (inst, id) = split_inst_id("BTC-USDT-SWAP:ord123").unwrap();
        assert_eq!(inst, "BTC-USDT-SWAP");
        assert_eq!(id, "ord123");
        assert!(split_inst_id("nope").is_err());
    }
}

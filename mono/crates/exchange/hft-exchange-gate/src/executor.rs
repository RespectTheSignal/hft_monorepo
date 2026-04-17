//! Gate.io Futures (USDT-margined) 주문 executor.
//!
//! ## 엔드포인트
//! - Base: `https://api.gateio.ws` (prod) 또는 `https://fx-api-testnet.gateio.ws` (testnet).
//! - Place: `POST /api/v4/futures/usdt/orders`
//! - Cancel by id: `DELETE /api/v4/futures/usdt/orders/{order_id}`
//!
//! ## 서명 스펙 (APIv4)
//!
//! Gate v4 공식 문서 기반:
//!
//! ```text
//! SIGN_STRING = method + "\n" + url_path + "\n" + query_string + "\n"
//!             + SHA512(body_bytes_hex_lower) + "\n"
//!             + unix_timestamp_seconds
//! SIGN = HEX_LOWER( HMAC_SHA512( api_secret, SIGN_STRING ) )
//! ```
//!
//! 헤더:
//! - `KEY: <api_key>`
//! - `SIGN: <hex sign>`
//! - `Timestamp: <unix seconds>`
//! - `Content-Type: application/json`
//!
//! GET 에서 body 가 없으면 SHA512 hash 자리에 **empty string 의 SHA512 hex**
//! 가 들어간다 (= `cf83e1...`). 본 구현은 POST 전용이라 항상 JSON body 를
//! 가지므로 body 가 비어도 같은 규칙을 따른다.
//!
//! ## OrderRequest → Gate 매핑
//! | HFT                   | Gate field      | 값/규칙                                     |
//! |-----------------------|-----------------|---------------------------------------------|
//! | `symbol`              | `contract`      | 그대로 (예: `BTC_USDT`)                     |
//! | `side` + `qty`        | `size`          | **부호 있는 정수** — Buy=+, Sell=−.         |
//! | `order_type::Market`  | `price: "0"`, `tif: "ioc"` | Gate 의 market 주문 표현. |
//! | `order_type::Limit`   | `price: "<p>"`                                        |
//! | `tif::Gtc`            | `tif: "gtc"`                                          |
//! | `tif::Ioc`            | `tif: "ioc"`                                          |
//! | `tif::Fok`            | `tif: "fok"`                                          |
//! | `client_id`           | `text`          | `t-` prefix 자동 부여 (Gate 규칙).           |
//!
//! **size 주의**: Gate 는 integer 계약 수량을 쓴다. `f64::round` 후 i64 캐스트.
//! 0 은 금지 — basic_validate 에서 이미 걸러짐.
//!
//! ## 테스트 전략
//! - HTTP 없이 서명 문자열/헤더 조립 단위 테스트.
//! - `prepare_body` / `canonical_sign_string` / `sign_request` 는 pure function.
//! - integration 테스트는 `wiremock` 없이 mock URL 로 connection-error 만 확인.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Method;
use serde::Deserialize;
use tracing::{debug, warn};

use hft_exchange_api::{
    ApiError, ExchangeExecutor, OrderAck, OrderRequest, OrderSide, OrderType, TimeInForce,
};
use hft_exchange_rest::{
    headers_from_pairs, hmac_sha512_hex, now_epoch_ms, now_epoch_s, sha512_hex, Credentials,
    RestClient,
};
use hft_types::ExchangeId;

// ────────────────────────────────────────────────────────────────────────────
// Config
// ────────────────────────────────────────────────────────────────────────────

/// Gate executor 구성. REST base URL 과 client_id prefix 를 override 가능.
#[derive(Debug, Clone)]
pub struct GateExecutorConfig {
    /// REST base (e.g. `https://api.gateio.ws`). trailing slash 없음.
    pub rest_base: String,
    /// text prefix — Gate 는 `t-` 로 시작하는 사용자 custom id 를 허용.
    pub text_prefix: String,
    /// 주문 경로. 테스트에서 `/spot/orders` 로 override 가능하게 노출.
    pub orders_path: String,
}

impl Default for GateExecutorConfig {
    fn default() -> Self {
        Self {
            rest_base: "https://api.gateio.ws".into(),
            text_prefix: "t-".into(),
            orders_path: "/api/v4/futures/usdt/orders".into(),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Executor
// ────────────────────────────────────────────────────────────────────────────

/// Gate Futures USDT executor.
///
/// `Arc` 로 복제하기 좋게 내부 state 를 immutable field 로만 보유.
/// `RestClient` 는 이미 `Clone` (reqwest::Client 공유).
#[derive(Debug, Clone)]
pub struct GateExecutor {
    pub cfg: GateExecutorConfig,
    pub creds: Credentials,
    pub http: RestClient,
}

impl GateExecutor {
    pub fn new(creds: Credentials) -> Result<Self, ApiError> {
        creds.validate()?;
        Ok(Self {
            cfg: GateExecutorConfig::default(),
            creds,
            http: RestClient::new()?,
        })
    }

    pub fn with_config(mut self, cfg: GateExecutorConfig) -> Self {
        self.cfg = cfg;
        self
    }

    pub fn with_http(mut self, http: RestClient) -> Self {
        self.http = http;
        self
    }

    // ── 서명 ─────────────────────────────────────────────────────────────

    /// Gate v4 canonical sign string.
    ///
    /// 순수 함수로 분리해 테스트 가능하도록 했다.
    /// `method` 는 uppercase, `path` 는 prefix `/` 포함, `query` 는 leading `?` 없이.
    fn canonical_sign_string(
        method: &str,
        path: &str,
        query: &str,
        body: &[u8],
        ts_seconds: i64,
    ) -> String {
        let body_hash = sha512_hex(body);
        format!(
            "{method}\n{path}\n{query}\n{body_hash}\n{ts}",
            method = method,
            path = path,
            query = query,
            body_hash = body_hash,
            ts = ts_seconds,
        )
    }

    fn sign(&self, method: &str, path: &str, query: &str, body: &[u8], ts: i64) -> String {
        let s = Self::canonical_sign_string(method, path, query, body, ts);
        hmac_sha512_hex(self.creds.api_secret.as_bytes(), s.as_bytes())
    }

    // ── body 조립 ────────────────────────────────────────────────────────

    /// `OrderRequest` → Gate JSON body. Gate 의 size 부호 규칙을 적용.
    ///
    /// 반환값: (body_string, text_with_prefix)
    fn prepare_body(&self, req: &OrderRequest) -> Result<(String, String), ApiError> {
        req.basic_validate()?;

        // size: Gate 는 부호 있는 정수 계약 수. round 후 i64 cast.
        let raw_size = req.qty.round();
        if !raw_size.is_finite() || raw_size.abs() >= i64::MAX as f64 {
            return Err(ApiError::InvalidOrder(format!(
                "qty out of range: {}",
                req.qty
            )));
        }
        let magnitude = raw_size as i64;
        if magnitude == 0 {
            return Err(ApiError::InvalidOrder(
                "qty rounded to 0 contracts — too small".into(),
            ));
        }
        let size: i64 = match req.side {
            OrderSide::Buy => magnitude,
            OrderSide::Sell => -magnitude,
        };

        // price: Market → "0" + tif ioc, Limit → 숫자 그대로.
        let (price_str, tif_str) = match req.order_type {
            OrderType::Market => ("0".to_string(), "ioc".to_string()),
            OrderType::Limit => {
                let p = req
                    .price
                    .ok_or_else(|| ApiError::InvalidOrder("limit without price".into()))?;
                if !p.is_finite() || p <= 0.0 {
                    return Err(ApiError::InvalidOrder(format!("bad price: {p}")));
                }
                let tif = match req.tif {
                    TimeInForce::Gtc => "gtc",
                    TimeInForce::Ioc => "ioc",
                    TimeInForce::Fok => "fok",
                };
                (format_price(p), tif.to_string())
            }
        };

        // text: Gate 는 `t-` prefix 요구. client_id 를 그대로 넣기 전 sanitize.
        let safe_id = sanitize_text(&req.client_id);
        let text = format!("{}{}", self.cfg.text_prefix, safe_id);

        // JSON body — 필드 순서는 Gate 와 무관하지만 일관성 위해 고정.
        let body = serde_json::json!({
            "contract": req.symbol.as_str(),
            "size": size,
            "price": price_str,
            "reduce_only": req.reduce_only,
            "tif": tif_str,
            "text": text,
        });
        let body_str = serde_json::to_string(&body)
            .map_err(|e| ApiError::Decode(format!("serialize body: {e}")))?;
        Ok((body_str, text))
    }

    // ── place/cancel 내부 HTTP ───────────────────────────────────────────

    async fn do_place(&self, req: OrderRequest) -> Result<OrderAck, ApiError> {
        self.creds.validate()?;
        let (body_str, text) = self.prepare_body(&req)?;

        let ts = now_epoch_s();
        let sign = self.sign("POST", &self.cfg.orders_path, "", body_str.as_bytes(), ts);

        let url = format!("{}{}", self.cfg.rest_base, self.cfg.orders_path);
        let headers = headers_from_pairs([
            ("KEY", self.creds.api_key.as_ref()),
            ("SIGN", sign.as_str()),
            ("Timestamp", ts.to_string().as_str()),
        ])?;

        debug!(
            target: "gate::executor",
            url = %url,
            client_id = %text,
            "placing order"
        );

        let resp = self
            .http
            .send(Method::POST, &url, &headers, Some(body_str))
            .await?;
        let body = resp.into_ok_body()?;

        parse_place_response(&body, req.exchange, req.client_id.clone())
    }

    async fn do_cancel(&self, order_id: &str) -> Result<(), ApiError> {
        self.creds.validate()?;
        if order_id.trim().is_empty() {
            return Err(ApiError::InvalidOrder("empty order_id".into()));
        }
        let path = format!("{}/{}", self.cfg.orders_path, order_id);
        let ts = now_epoch_s();
        let sign = self.sign("DELETE", &path, "", &[], ts);
        let url = format!("{}{}", self.cfg.rest_base, path);
        let headers = headers_from_pairs([
            ("KEY", self.creds.api_key.as_ref()),
            ("SIGN", sign.as_str()),
            ("Timestamp", ts.to_string().as_str()),
        ])?;

        let resp = self.http.send(Method::DELETE, &url, &headers, None).await?;
        // 200 / 201 → OK. 404 는 "이미 취소/체결" 로 간주 → Ok(()).
        if resp.status == reqwest::StatusCode::NOT_FOUND {
            warn!(
                target: "gate::executor",
                order_id = %order_id,
                "cancel target not found — treating as already-cancelled"
            );
            return Ok(());
        }
        let _ = resp.into_ok_body()?;
        Ok(())
    }
}

#[async_trait]
impl ExchangeExecutor for GateExecutor {
    fn id(&self) -> ExchangeId {
        ExchangeId::Gate
    }

    async fn place_order(&self, req: OrderRequest) -> anyhow::Result<OrderAck> {
        Ok(self.do_place(req).await?)
    }

    async fn cancel(&self, exchange_order_id: &str) -> anyhow::Result<()> {
        Ok(self.do_cancel(exchange_order_id).await?)
    }

    fn label(&self) -> String {
        "gate:futures_usdt".into()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 응답 파싱
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GatePlaceResp {
    #[serde(default)]
    id: serde_json::Value, // 숫자/문자열 양쪽 가능 — 관용적으로 받는다.
    #[allow(dead_code)]
    #[serde(default)]
    finish_as: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    create_time: Option<f64>,
}

fn parse_place_response(
    body: &str,
    exchange: ExchangeId,
    client_id: Arc<str>,
) -> Result<OrderAck, ApiError> {
    // Gate 는 2xx 인데 에러를 반환할 일은 드물지만, `label` 이 존재하면 reject 처리.
    let parsed: GatePlaceResp = serde_json::from_str(body).map_err(|e| {
        ApiError::Decode(format!(
            "place resp json: {e} / raw={}",
            truncate(body, 256)
        ))
    })?;

    if let Some(label) = parsed.label.as_deref() {
        if !label.is_empty() {
            let msg = parsed.message.unwrap_or_default();
            return Err(ApiError::Rejected(format!("{label}: {msg}")));
        }
    }
    // finish_as 가 "cancelled" 면 "바로 취소된 IOC/FOK" — 가격이 안 맞아 전부 취소.
    // 이 경우도 reject 로 취급하는 게 투명하지만, 우선 accept (ack) 하고 호출자가 판단.
    let id_str = match parsed.id {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s,
        _ => {
            return Err(ApiError::Decode(format!(
                "missing order id in gate response: {}",
                truncate(body, 256)
            )));
        }
    };
    let ts_ms = match parsed.create_time {
        Some(t) if t.is_finite() && t > 0.0 => (t * 1_000.0) as i64,
        _ => now_epoch_ms(),
    };
    Ok(OrderAck {
        exchange,
        exchange_order_id: id_str,
        client_id,
        ts_ms,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// 보조 함수
// ────────────────────────────────────────────────────────────────────────────

/// Gate 의 text 필드는 영숫자/하이픈/언더스코어/점만 허용. 그 외는 `_` 로 치환.
fn sanitize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('x');
    }
    out
}

/// price → 문자열. 소수점 trailing zero 제거, 최대 8 자리.
fn format_price(p: f64) -> String {
    // Gate 는 문자열을 그대로 받는다. `{:.8}` 로 자른 뒤 trailing 0 제거.
    let raw = format!("{:.8}", p);
    // trailing zero 제거
    let trimmed = raw.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &s[..end]
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_types::Symbol;

    fn mk_exec() -> GateExecutor {
        GateExecutor {
            cfg: GateExecutorConfig::default(),
            creds: Credentials::new("my_key", "my_secret"),
            http: RestClient::new().unwrap(),
        }
    }

    #[test]
    fn canonical_sign_string_format_is_stable() {
        let s = GateExecutor::canonical_sign_string(
            "POST",
            "/api/v4/futures/usdt/orders",
            "",
            b"{\"a\":1}",
            1_700_000_000,
        );
        // 5 줄 (method / path / query / hash / ts)
        assert_eq!(s.matches('\n').count(), 4);
        // 첫 줄은 method.
        assert!(s.starts_with("POST\n"));
        // hash 는 SHA512 = 128 hex chars.
        let parts: Vec<&str> = s.split('\n').collect();
        assert_eq!(parts[3].len(), 128);
        assert_eq!(parts[4], "1700000000");
    }

    #[test]
    fn sign_empty_body_uses_empty_sha512() {
        // cancel 처럼 body 가 empty 인 경우. SHA512("") = cf83e1...a3e
        let exec = mk_exec();
        let sig = exec.sign(
            "DELETE",
            "/api/v4/futures/usdt/orders/123",
            "",
            &[],
            1_700_000_000,
        );
        // 길이 = 128 hex
        assert_eq!(sig.len(), 128);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn prepare_body_buy_limit_gtc() {
        let exec = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            qty: 3.0,
            price: Some(60_000.5),
            reduce_only: false,
            tif: TimeInForce::Gtc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("abc123"),
        };
        let (body, text) = exec.prepare_body(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["contract"], "BTC_USDT");
        assert_eq!(v["size"], 3);
        assert_eq!(v["price"], "60000.5");
        assert_eq!(v["tif"], "gtc");
        assert_eq!(v["text"], "t-abc123");
        assert_eq!(text, "t-abc123");
    }

    #[test]
    fn prepare_body_sell_uses_negative_size() {
        let exec = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Sell,
            order_type: OrderType::Limit,
            qty: 5.0,
            price: Some(100.0),
            reduce_only: false,
            tif: TimeInForce::Ioc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("sell1"),
        };
        let (body, _) = exec.prepare_body(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["size"], -5);
        assert_eq!(v["tif"], "ioc");
    }

    #[test]
    fn prepare_body_market_forces_ioc_and_zero_price() {
        let exec = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("ETH_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            qty: 1.0,
            price: None,
            reduce_only: false,
            tif: TimeInForce::Gtc, // 무시됨
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("m1"),
        };
        let (body, _) = exec.prepare_body(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["price"], "0");
        assert_eq!(v["tif"], "ioc");
        assert_eq!(v["reduce_only"], false);
    }

    #[test]
    fn prepare_body_includes_reduce_only_when_true() {
        let exec = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Sell,
            order_type: OrderType::Limit,
            qty: 2.0,
            price: Some(42_000.0),
            reduce_only: true,
            tif: TimeInForce::Ioc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("reduce-gate"),
        };
        let (body, _) = exec.prepare_body(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["reduce_only"], true);
    }

    #[test]
    fn prepare_body_rejects_fractional_contracts() {
        let exec = mk_exec();
        // 0.4 → round → 0 → reject
        let req = OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            qty: 0.4,
            price: None,
            reduce_only: false,
            tif: TimeInForce::Ioc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("q0"),
        };
        let err = exec.prepare_body(&req).unwrap_err();
        match err {
            ApiError::InvalidOrder(m) => assert!(m.contains("too small")),
            _ => panic!("expected InvalidOrder"),
        }
    }

    #[test]
    fn prepare_body_sanitizes_client_id_spaces() {
        let exec = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            qty: 1.0,
            price: None,
            reduce_only: false,
            tif: TimeInForce::Ioc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("hi world#1"),
        };
        let (_, text) = exec.prepare_body(&req).unwrap();
        // 공백/# 은 _ 로 치환
        assert_eq!(text, "t-hi_world_1");
    }

    #[test]
    fn parse_place_response_accepts_numeric_id() {
        let body = r#"{"id":12345,"create_time":1700000000.5}"#;
        let ack = parse_place_response(body, ExchangeId::Gate, Arc::from("c1")).unwrap();
        assert_eq!(ack.exchange_order_id, "12345");
        assert_eq!(ack.ts_ms, 1_700_000_000_500);
    }

    #[test]
    fn parse_place_response_accepts_string_id() {
        let body = r#"{"id":"abc-456","create_time":1700000000}"#;
        let ack = parse_place_response(body, ExchangeId::Gate, Arc::from("c2")).unwrap();
        assert_eq!(ack.exchange_order_id, "abc-456");
    }

    #[test]
    fn parse_place_response_rejects_when_label_nonempty() {
        let body = r#"{"label":"INSUFFICIENT_BALANCE","message":"no funds"}"#;
        let err = parse_place_response(body, ExchangeId::Gate, Arc::from("c3")).unwrap_err();
        match err {
            ApiError::Rejected(m) => {
                assert!(m.contains("INSUFFICIENT_BALANCE"));
                assert!(m.contains("no funds"));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn parse_place_response_decode_error_on_missing_id() {
        let body = r#"{"create_time":1700000000}"#;
        let err = parse_place_response(body, ExchangeId::Gate, Arc::from("c4")).unwrap_err();
        match err {
            ApiError::Decode(_) => {}
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn format_price_trims_trailing_zeros() {
        assert_eq!(format_price(60000.5), "60000.5");
        assert_eq!(format_price(100.0), "100");
        assert_eq!(format_price(0.0001), "0.0001");
    }

    #[test]
    fn executor_new_rejects_empty_creds() {
        let err = GateExecutor::new(Credentials::new("", "")).unwrap_err();
        match err {
            ApiError::Auth(_) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_empty_id_rejected() {
        let exec = mk_exec();
        let err = exec.do_cancel(" ").await.unwrap_err();
        match err {
            ApiError::InvalidOrder(_) => {}
            other => panic!("expected InvalidOrder, got {other:?}"),
        }
    }
}

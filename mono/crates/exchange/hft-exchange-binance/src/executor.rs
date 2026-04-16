//! Binance Futures (USDT-M) 주문 executor.
//!
//! ## 엔드포인트
//! - Base: `https://fapi.binance.com`
//! - Place: `POST /fapi/v1/order`
//! - Cancel: `DELETE /fapi/v1/order?symbol=X&origClientOrderId=Y` 또는 `orderId=Z`
//!
//! ## 서명 스펙 (SIGNED endpoints)
//!
//! Binance 는 모든 parameter 를 **query string** 으로 보낸다 (POST 도 body 가 아닌
//! `?a=1&b=2` 형태). 절차:
//!
//! 1. parameter 목록을 key=value 로 조립 (insertion order 보존).
//! 2. 맨 뒤에 `timestamp=<epoch ms>` 추가.
//! 3. 위 문자열에 HMAC-SHA256( api_secret, string ) 를 계산, hex 로 `signature=`
//!    뒤에 append.
//!
//! 헤더는 단 하나: `X-MBX-APIKEY: <api_key>`.
//!
//! recvWindow 는 optional — 5000ms default 를 명시 지정.
//!
//! ## OrderRequest → Binance 매핑
//! | HFT | Binance param | 규칙 |
//! |-----|---------------|------|
//! | symbol | `symbol` | Binance 는 "BTCUSDT" 대문자 연결 — `BTC_USDT` 의 `_` 제거 + upper |
//! | side Buy/Sell | `side=BUY`/`SELL` | |
//! | order_type Limit | `type=LIMIT` + `timeInForce=<tif>` + `price=<p>` + `quantity=<q>` |
//! | order_type Market | `type=MARKET` + `quantity=<q>` (TIF/price 금지) |
//! | client_id | `newClientOrderId` | 영숫자/언더스코어/하이픈만. 32자 제한 권장 |
//!
//! ## 주의
//! - Binance 의 숫자 타입은 문자열. `{:.8}` 사용 후 trailing zero 제거.
//! - USDⓈ-M Futures 는 positionSide 를 요구할 수 있음 — 기본 hedge-mode 해제
//!   (One-way) 가정. positionSide 파라미터 omit.
//! - Binance 는 `timestamp` 필수이므로 local clock 이 심하게 드리프트되면 reject.
//!   caller 가 NTP 동기 유지.

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
pub struct BinanceExecutorConfig {
    pub rest_base: String,
    pub place_path: String,
    pub cancel_path: String,
    pub recv_window_ms: u64,
}

impl Default for BinanceExecutorConfig {
    fn default() -> Self {
        Self {
            rest_base: "https://fapi.binance.com".into(),
            place_path: "/fapi/v1/order".into(),
            cancel_path: "/fapi/v1/order".into(),
            recv_window_ms: 5_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BinanceExecutor {
    pub cfg: BinanceExecutorConfig,
    pub creds: Credentials,
    pub http: RestClient,
}

impl BinanceExecutor {
    pub fn new(creds: Credentials) -> Result<Self, ApiError> {
        creds.validate()?;
        Ok(Self {
            cfg: BinanceExecutorConfig::default(),
            creds,
            http: RestClient::new()?,
        })
    }

    pub fn with_config(mut self, cfg: BinanceExecutorConfig) -> Self {
        self.cfg = cfg;
        self
    }

    pub fn with_http(mut self, http: RestClient) -> Self {
        self.http = http;
        self
    }

    /// `OrderRequest` → query-string parameter 쌍 (signature 전 단계).
    fn build_place_params(
        &self,
        req: &OrderRequest,
        timestamp_ms: i64,
    ) -> Result<Vec<(String, String)>, ApiError> {
        req.basic_validate()?;
        let symbol = to_binance_symbol(req.symbol.as_str());
        let side = match req.side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        };
        let client_id = sanitize_client_id(&req.client_id);

        let mut params: Vec<(String, String)> = Vec::with_capacity(10);
        params.push(("symbol".into(), symbol));
        params.push(("side".into(), side.into()));

        match req.order_type {
            OrderType::Market => {
                params.push(("type".into(), "MARKET".into()));
                params.push(("quantity".into(), format_num(req.qty)));
            }
            OrderType::Limit => {
                let price = req.price.ok_or_else(|| {
                    ApiError::InvalidOrder("limit without price".into())
                })?;
                if !price.is_finite() || price <= 0.0 {
                    return Err(ApiError::InvalidOrder(format!("bad price: {price}")));
                }
                params.push(("type".into(), "LIMIT".into()));
                params.push(("timeInForce".into(), tif_string(req.tif).into()));
                params.push(("quantity".into(), format_num(req.qty)));
                params.push(("price".into(), format_num(price)));
            }
        }
        params.push(("newClientOrderId".into(), client_id));
        params.push(("recvWindow".into(), self.cfg.recv_window_ms.to_string()));
        params.push(("timestamp".into(), timestamp_ms.to_string()));
        Ok(params)
    }

    /// params → `a=1&b=2` (Binance 는 URL-encode 필요 없음 — 영숫자만 사용).
    /// 그래도 안전하게 percent-encode 는 RFC3986 unreserved 기준.
    fn encode_query(params: &[(String, String)]) -> String {
        let mut s = String::new();
        for (i, (k, v)) in params.iter().enumerate() {
            if i > 0 {
                s.push('&');
            }
            s.push_str(k);
            s.push('=');
            s.push_str(&percent_encode_simple(v));
        }
        s
    }

    fn sign(&self, query: &str) -> String {
        hmac_sha256_hex(self.creds.api_secret.as_bytes(), query.as_bytes())
    }

    async fn do_place(&self, req: OrderRequest) -> Result<OrderAck, ApiError> {
        self.creds.validate()?;
        let ts = now_epoch_ms();
        let params = self.build_place_params(&req, ts)?;
        let query = Self::encode_query(&params);
        let sig = self.sign(&query);
        let full_query = format!("{query}&signature={sig}");

        let url = format!("{}{}?{}", self.cfg.rest_base, self.cfg.place_path, full_query);
        let headers =
            headers_from_pairs([("X-MBX-APIKEY", self.creds.api_key.as_ref())])?;

        debug!(
            target: "binance::executor",
            symbol = req.symbol.as_str(),
            side = ?req.side,
            "placing order"
        );

        let resp = self.http.send(Method::POST, &url, &headers, None).await?;
        let body = resp.into_ok_body()?;
        parse_place_response(&body, req.exchange, req.client_id.clone())
    }

    async fn do_cancel(&self, exchange_order_id: &str) -> Result<(), ApiError> {
        self.creds.validate()?;
        // cancel 에도 symbol 이 필요. exchange_order_id 를 `SYMBOL:ID` 형태로 인코딩
        // 했다고 가정. 그렇지 않으면 Rejected.
        let (symbol, order_id) = split_symbol_id(exchange_order_id)?;
        let ts = now_epoch_ms();
        let params: Vec<(String, String)> = vec![
            ("symbol".into(), symbol),
            ("orderId".into(), order_id),
            ("recvWindow".into(), self.cfg.recv_window_ms.to_string()),
            ("timestamp".into(), ts.to_string()),
        ];
        let query = Self::encode_query(&params);
        let sig = self.sign(&query);
        let full_query = format!("{query}&signature={sig}");

        let url = format!(
            "{}{}?{}",
            self.cfg.rest_base, self.cfg.cancel_path, full_query
        );
        let headers =
            headers_from_pairs([("X-MBX-APIKEY", self.creds.api_key.as_ref())])?;

        let resp = self.http.send(Method::DELETE, &url, &headers, None).await?;
        if resp.status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let _ = resp.into_ok_body()?;
        Ok(())
    }
}

#[async_trait]
impl ExchangeExecutor for BinanceExecutor {
    fn id(&self) -> ExchangeId {
        ExchangeId::Binance
    }

    async fn place_order(&self, req: OrderRequest) -> anyhow::Result<OrderAck> {
        Ok(self.do_place(req).await?)
    }

    async fn cancel(&self, exchange_order_id: &str) -> anyhow::Result<()> {
        Ok(self.do_cancel(exchange_order_id).await?)
    }

    fn label(&self) -> String {
        "binance:fapi".into()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 응답
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BinancePlaceResp {
    #[serde(default)]
    #[serde(rename = "orderId")]
    order_id: Option<i64>,
    // 응답 전문 보존용 필드다. 현재는 ack 조립에 직접 쓰지 않지만 추후 로그/디버깅에 필요하다.
    #[allow(dead_code)]
    #[serde(default)]
    #[serde(rename = "clientOrderId")]
    client_order_id: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    #[serde(rename = "updateTime")]
    update_time: Option<i64>,
    #[serde(default)]
    #[serde(rename = "transactTime")]
    transact_time: Option<i64>,
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    msg: Option<String>,
}

fn parse_place_response(
    body: &str,
    exchange: ExchangeId,
    client_id: Arc<str>,
) -> Result<OrderAck, ApiError> {
    let p: BinancePlaceResp = serde_json::from_str(body)
        .map_err(|e| ApiError::Decode(format!("binance resp: {e}")))?;
    if let Some(code) = p.code {
        // Binance 는 success 2xx 에도 { code: 200, msg: ... } 를 돌려주는 endpoint 가
        // 일부 있어 code > 0 을 무조건 error 로 보진 않는다. 음수 code 는 항상 에러.
        if code < 0 {
            return Err(ApiError::Rejected(format!(
                "binance error {}: {}",
                code,
                p.msg.unwrap_or_default()
            )));
        }
    }
    let order_id = p
        .order_id
        .ok_or_else(|| ApiError::Decode(format!("missing orderId in response: {}", body)))?;
    let symbol = p.symbol.unwrap_or_else(|| "?".to_string());
    let ts_ms = p.update_time.or(p.transact_time).unwrap_or_else(now_epoch_ms);
    Ok(OrderAck {
        exchange,
        // cancel 은 symbol 도 필요하므로 합쳐서 보관.
        exchange_order_id: format!("{symbol}:{order_id}"),
        client_id,
        ts_ms,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// 보조
// ────────────────────────────────────────────────────────────────────────────

/// `BTC_USDT` → `BTCUSDT` (Binance 규칙).
fn to_binance_symbol(canonical: &str) -> String {
    canonical.replace('_', "").to_ascii_uppercase()
}

fn tif_string(t: TimeInForce) -> &'static str {
    match t {
        TimeInForce::Gtc => "GTC",
        TimeInForce::Ioc => "IOC",
        TimeInForce::Fok => "FOK",
    }
}

fn format_num(x: f64) -> String {
    // Binance 는 문자열. 소수 8자리, trailing zero 제거.
    let raw = format!("{:.8}", x);
    let trimmed = raw.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 영숫자/-/_/. 만 허용. 길이 32 로 제한.
fn sanitize_client_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(32));
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
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

/// `SYMBOL:ID` 분리.
fn split_symbol_id(s: &str) -> Result<(String, String), ApiError> {
    let (sym, id) = s
        .split_once(':')
        .ok_or_else(|| ApiError::InvalidOrder(format!("expected SYMBOL:ID, got '{s}'")))?;
    if sym.is_empty() || id.is_empty() {
        return Err(ApiError::InvalidOrder(format!(
            "malformed order id '{s}'"
        )));
    }
    Ok((sym.to_string(), id.to_string()))
}

/// RFC3986 unreserved 이외를 `%XX` 로 인코드. Binance 는 영숫자/.-_~만 들어가므로
/// 실제로는 거의 통과한다. 안전망.
fn percent_encode_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_types::Symbol;

    fn mk_exec() -> BinanceExecutor {
        BinanceExecutor {
            cfg: BinanceExecutorConfig::default(),
            creds: Credentials::new("key", "secret"),
            http: RestClient::new().unwrap(),
        }
    }

    #[test]
    fn canonical_symbol_uppercase_and_no_underscore() {
        assert_eq!(to_binance_symbol("BTC_USDT"), "BTCUSDT");
        assert_eq!(to_binance_symbol("eth_usdt"), "ETHUSDT");
    }

    #[test]
    fn sign_matches_official_example_shape() {
        // Binance docs example: timestamp=1499827319559&recvWindow=5000&...
        // 서명은 HMAC-SHA256 결과 hex 64자.
        let exec = mk_exec();
        let sig = exec.sign("a=1&b=2");
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn build_place_params_limit_includes_price_and_tif() {
        let exec = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Binance,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            qty: 0.01,
            price: Some(60_000.0),
            reduce_only: false,
            tif: TimeInForce::Gtc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("abc123"),
        };
        let params = exec.build_place_params(&req, 1_700_000_000_000).unwrap();
        let map: std::collections::HashMap<_, _> = params.iter().cloned().collect();
        assert_eq!(map.get("symbol").unwrap(), "BTCUSDT");
        assert_eq!(map.get("side").unwrap(), "BUY");
        assert_eq!(map.get("type").unwrap(), "LIMIT");
        assert_eq!(map.get("timeInForce").unwrap(), "GTC");
        assert_eq!(map.get("quantity").unwrap(), "0.01");
        assert_eq!(map.get("price").unwrap(), "60000");
        assert_eq!(map.get("newClientOrderId").unwrap(), "abc123");
        assert_eq!(map.get("recvWindow").unwrap(), "5000");
        assert_eq!(map.get("timestamp").unwrap(), "1700000000000");
    }

    #[test]
    fn build_place_params_market_omits_tif_and_price() {
        let exec = mk_exec();
        let req = OrderRequest {
            exchange: ExchangeId::Binance,
            symbol: Symbol::new("ETH_USDT"),
            side: OrderSide::Sell,
            order_type: OrderType::Market,
            qty: 2.5,
            price: None,
            reduce_only: false,
            tif: TimeInForce::Ioc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::from("m2"),
        };
        let params = exec.build_place_params(&req, 1).unwrap();
        let keys: Vec<&str> = params.iter().map(|p| p.0.as_str()).collect();
        assert!(keys.contains(&"type"));
        assert!(!keys.contains(&"price"));
        assert!(!keys.contains(&"timeInForce"));
    }

    #[test]
    fn encode_query_preserves_order() {
        let q = BinanceExecutor::encode_query(&[
            ("a".into(), "1".into()),
            ("b".into(), "2".into()),
            ("c".into(), "3".into()),
        ]);
        assert_eq!(q, "a=1&b=2&c=3");
    }

    #[test]
    fn percent_encode_unreserved_passthrough() {
        assert_eq!(percent_encode_simple("abc-._~"), "abc-._~");
        assert_eq!(percent_encode_simple("a b"), "a%20b");
        assert_eq!(percent_encode_simple("a+b"), "a%2Bb");
    }

    #[test]
    fn sanitize_client_id_limits_length_and_filters() {
        let s = sanitize_client_id("hello world#!^&*()12345678901234567890");
        assert!(s.len() <= 32);
        assert!(!s.contains(' '));
        assert!(!s.contains('#'));
    }

    #[test]
    fn parse_place_response_happy() {
        let body = r#"{"orderId":12345,"clientOrderId":"c1","symbol":"BTCUSDT","updateTime":1700000000000}"#;
        let ack = parse_place_response(body, ExchangeId::Binance, Arc::from("c1")).unwrap();
        assert_eq!(ack.exchange_order_id, "BTCUSDT:12345");
        assert_eq!(ack.ts_ms, 1_700_000_000_000);
    }

    #[test]
    fn parse_place_response_rejects_negative_code() {
        let body = r#"{"code":-2019,"msg":"Margin insufficient."}"#;
        let err =
            parse_place_response(body, ExchangeId::Binance, Arc::from("c2")).unwrap_err();
        match err {
            ApiError::Rejected(m) => assert!(m.contains("-2019")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn split_symbol_id_rejects_malformed() {
        assert!(split_symbol_id("just_an_id").is_err());
        assert!(split_symbol_id(":12345").is_err());
        assert!(split_symbol_id("BTCUSDT:").is_err());
        let (s, i) = split_symbol_id("BTCUSDT:12345").unwrap();
        assert_eq!(s, "BTCUSDT");
        assert_eq!(i, "12345");
    }

    #[test]
    fn format_num_trims_trailing_zeros() {
        assert_eq!(format_num(1.0), "1");
        assert_eq!(format_num(0.5), "0.5");
        assert_eq!(format_num(0.00000001), "0.00000001");
    }
}

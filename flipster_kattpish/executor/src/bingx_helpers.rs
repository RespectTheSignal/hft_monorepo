//! BingX web-API client. Talks to
//! `https://api-swap.we-api.com/api/swap/v1/proxy/trade/order/place/post`
//! — the same backend the BingX web UI uses — with browser-equivalent
//! headers + a self-computed SHA-256 `sign` header.
//!
//! Auth: bearer JWT extracted from a logged-in browser session
//! (via `dump_cookies.py` → cookies.json["bingx"]).
//!
//! Sign algorithm (reverse-engineered from the BingX webapp bundle on
//! 2026-05-05; verified against 3 captured requests + 1 live shadow
//! trade with our own computed sign):
//!
//! ```text
//! SECRET = "95d65c73dc5c4370ae9018fb7f2eab69"   // baked into the JS bundle
//! body_canon = JSON.stringify(body, sort_keys=true, separators=',',':',
//!                             with all numbers/booleans coerced to strings)
//!                             OR "{}" if body is empty
//! payload = SECRET + timestamp + traceId + deviceId + platformId
//!         + appVersion + antiDeviceId + body_canon
//! sign    = sha256(payload).hex().to_uppercase()
//! ```
//!
//! Caveats:
//! - SECRET could rotate when BingX redeploys their JS bundle. Re-run
//!   the capture script to refresh.
//! - JWT lives ~7 days. dump_cookies.py needs to run periodically.
//! - The `/position/list` endpoint and others use additional sign rules
//!   we have NOT verified yet — only `/trade/order/place/post` is
//!   confirmed end-to-end.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const ORDER_PLACE_URL: &str =
    "https://api-swap.we-api.com/api/swap/v1/proxy/trade/order/place/post";
const ORDER_CANCEL_URL: &str =
    "https://api-swap.we-api.com/api/swap/v1/proxy/trade/order/cancel/post";
const POSITIONS_URL: &str =
    "https://api-swap.we-api.com/api/swap/v1/proxy/position/list";

/// Reverse-engineered constant. Concat of `ENCRYPTION_KEYS.{p1,p2,p3}` from
/// the BingX webapp bundle. Treated as a fixed app-secret. If BingX rotates,
/// extract again from `dump_cookies.py`-driven capture.
const SECRET: &str = "95d65c73dc5c4370ae9018fb7f2eab69";

/// Side per BingX web payload convention.
#[derive(Debug, Clone, Copy, Serialize)]
pub enum Side {
    /// Buy / long entry / short exit
    #[serde(rename = "Bid")]
    Bid,
    /// Sell / short entry / long exit
    #[serde(rename = "Ask")]
    Ask,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum Action {
    Open,
    Close,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum TradeType {
    Market,
    Limit,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub enum VolumeType {
    /// `entrustVolume` is USDT notional (open orders).
    Value,
    /// `entrustVolume` is fraction of position (close orders, "1" = 100%).
    Percentage,
}

/// Headers common to every BingX request — the device fingerprint extracted
/// from a real browser session. Only the JWT in `authorization` and the
/// per-request (`timestamp`, `traceId`, `sign`) values change.
#[derive(Debug, Clone)]
pub struct BingxIdentity {
    pub user_id: String,
    pub device_id: String,
    pub anti_device_id: String,
    pub platform_id: String, // "30"
    pub app_version: String, // "5.12.3"
    pub jwt: String,
    /// User-Agent must roughly match the browser the JWT was minted from.
    pub user_agent: String,
}

impl BingxIdentity {
    /// Defaults pulled from the captured Firefox/macOS session (2026-05-05).
    /// `user_id` and `jwt` MUST be replaced — they're per-account.
    pub fn defaults_with_jwt(user_id: String, jwt: String) -> Self {
        Self {
            user_id,
            device_id: "be3bcb3d632948ddab2aed0a3f1b45d4".into(),
            anti_device_id: "22A728646696180140C05C55_9ceda1316928".into(),
            platform_id: "30".into(),
            app_version: "5.12.3".into(),
            jwt,
            user_agent:
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:150.0) \
                 Gecko/20100101 Firefox/150.0"
                    .into(),
        }
    }
}

pub struct BingxClient {
    http: reqwest::Client,
    pub id: BingxIdentity,
    /// Last cookie bundle (we don't strictly need cookies — JWT alone is
    /// enough — but keeping them mirrors the browser more closely and may
    /// help avoid bot heuristics).
    pub cookies: HashMap<String, String>,
}

impl BingxClient {
    pub fn new(id: BingxIdentity, cookies: HashMap<String, String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .gzip(true)
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { http, id, cookies })
    }

    fn cookie_header(&self) -> Option<String> {
        if self.cookies.is_empty() {
            return None;
        }
        Some(
            self.cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn build_headers(&self, ts: &str, trace_id: &str, sign: &str) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        let set = |h: &mut HeaderMap, k: &'static str, v: String| -> Result<()> {
            h.insert(
                HeaderName::from_static(k),
                HeaderValue::from_str(&v).map_err(|e| anyhow!("hdr {k}: {e}"))?,
            );
            Ok(())
        };
        set(&mut h, "user-agent", self.id.user_agent.clone())?;
        set(
            &mut h,
            "accept",
            "application/json, text/plain, */*".to_string(),
        )?;
        set(
            &mut h,
            "content-type",
            "application/json".to_string(),
        )?;
        set(&mut h, "platformid", self.id.platform_id.clone())?;
        set(&mut h, "appsiteid", "0".to_string())?;
        set(&mut h, "channel", "official".to_string())?;
        set(&mut h, "reg_channel", "official".to_string())?;
        set(&mut h, "app_version", self.id.app_version.clone())?;
        set(&mut h, "device_id", self.id.device_id.clone())?;
        set(&mut h, "lang", "en".to_string())?;
        set(&mut h, "appid", "30004".to_string())?;
        set(&mut h, "mainappid", "10009".to_string())?;
        set(&mut h, "timezone", "9".to_string())?;
        set(&mut h, "antideviceid", self.id.anti_device_id.clone())?;
        set(
            &mut h,
            "authorization",
            format!("Bearer {}", self.id.jwt),
        )?;
        set(&mut h, "traceid", trace_id.to_string())?;
        set(&mut h, "timestamp", ts.to_string())?;
        set(&mut h, "sign", sign.to_string())?;
        set(&mut h, "origin", "https://bingx.com".to_string())?;
        set(&mut h, "referer", "https://bingx.com/".to_string())?;
        if let Some(c) = self.cookie_header() {
            set(&mut h, "cookie", c)?;
        }
        Ok(h)
    }

    /// Place a market order. `coin_qty` is the amount in BASE coin units
    /// (e.g. `0.0001` for 0.0001 BTC). The captured browser payload uses
    /// `volumeType="Value"` + `tradingUnit="COIN"`, which BingX treats as
    /// "this much of the base coin". Use [`coin_qty_for_usd`] to convert
    /// from a USD notional + last price.
    pub async fn place_market_open(
        &self,
        symbol: &str,
        side: Side,
        coin_qty: f64,
        last_price: f64,
    ) -> Result<Value> {
        let ts = Self::now_ms();
        let body = json!({
            "userId": self.id.user_id,
            "symbol": symbol,
            "entrustVolume": format_qty(coin_qty),
            "entrustPrice": 0,
            "side": side,
            "action": "Open",
            "tradeType": "Market",
            "volumeType": "Value",
            "tradingUnit": "COIN",
            "triggerCloseV2": {"feeType": 0},
            "isOpponentPrice": 0,
            "onlyReduce": false,
            "tradePrice": format!("{last_price}"),
            "tradeTs": ts - 50,
            "supportNonCryptoOneway": true,
        });
        self.send_place(body, ts).await
    }

    /// Close a position fully via market.
    ///
    /// `position_id`:
    /// - `None` → relies on one-way mode + `onlyReduce=true`. BingX will
    ///   close the only open position on this symbol.
    /// - `Some(id)` → explicit positionId, required for hedge mode or
    ///   when multiple positions exist.
    ///
    /// We don't get `positionId` from the open response (only `orderId`),
    /// and `/position/list` has different sign rules we haven't verified,
    /// so we default to `None` (one-way mode) for new opens. Future:
    /// query `/position/list` post-open and cache the id.
    pub async fn close_market_full(
        &self,
        symbol: &str,
        side: Side,
        position_id: Option<&str>,
        last_price: f64,
    ) -> Result<Value> {
        let ts = Self::now_ms();
        let mut body = json!({
            "userId": self.id.user_id,
            "symbol": symbol,
            "entrustVolume": 1,
            "entrustPrice": 0,
            "side": side,
            "action": "Close",
            "tradeType": "Market",
            "volumeType": "Percentage",
            "tradingUnit": "COIN",
            "isOpponentPrice": 0,
            "onlyReduce": true,
            "tradePrice": format!("{last_price}"),
            "tradeTs": ts - 50,
            "supportNonCryptoOneway": true,
        });
        if let Some(pid) = position_id {
            body["positionId"] = serde_json::Value::String(pid.to_string());
        }
        self.send_place(body, ts).await
    }

    /// Limit order open at a strict price (passive / maker side).
    ///
    /// `is_opponent` controls fill behavior:
    /// - `false` → use the entered price strictly. If we put the price at
    ///   our side of book (bid for long, ask for short), the order rests
    ///   and pays MAKER fees when matched. Set to `false` for the maker
    ///   path that earns the Banner fee rebate.
    /// - `true` → use the opponent's best price (cross the spread, taker).
    pub async fn place_limit_open(
        &self,
        symbol: &str,
        side: Side,
        coin_qty: f64,
        limit_price: f64,
        is_opponent: bool,
    ) -> Result<Value> {
        let ts = Self::now_ms();
        let body = json!({
            "userId": self.id.user_id,
            "symbol": symbol,
            "entrustVolume": format_qty(coin_qty),
            "entrustPrice": format!("{limit_price}"),
            "side": side,
            "action": "Open",
            "tradeType": "Limit",
            "volumeType": "Value",
            "tradingUnit": "COIN",
            "triggerCloseV2": {"feeType": 0},
            "isOpponentPrice": if is_opponent { 1 } else { 0 },
            "timeInForce": "GTC",
            "onlyReduce": false,
            "supportNonCryptoOneway": true,
        });
        self.send_place(body, ts).await
    }

    /// Limit close at a strict price. Maker if priced passively
    /// (e.g. for a long, post the close limit at our take-profit ask above
    /// current bid). 100% close via volumeType=Percentage.
    pub async fn close_limit_full(
        &self,
        symbol: &str,
        side: Side,
        position_id: Option<&str>,
        limit_price: f64,
    ) -> Result<Value> {
        let ts = Self::now_ms();
        let mut body = json!({
            "userId": self.id.user_id,
            "symbol": symbol,
            "entrustVolume": 1,
            "entrustPrice": format!("{limit_price}"),
            "side": side,
            "action": "Close",
            "tradeType": "Limit",
            "volumeType": "Percentage",
            "tradingUnit": "COIN",
            "isOpponentPrice": 0,
            "timeInForce": "GTC",
            "onlyReduce": true,
            "tradePrice": format!("{limit_price}"),
            "tradeTs": ts - 50,
            "supportNonCryptoOneway": true,
        });
        if let Some(pid) = position_id {
            body["positionId"] = serde_json::Value::String(pid.to_string());
        }
        self.send_place(body, ts).await
    }

    async fn send_place(&self, body: Value, ts: i64) -> Result<Value> {
        let trace_id = Uuid::new_v4().simple().to_string();
        let body_canon = canonicalize_body(&body);
        let sign = compute_sign(
            &self.id,
            &ts.to_string(),
            &trace_id,
            &body_canon,
        );
        let headers = self.build_headers(&ts.to_string(), &trace_id, &sign)?;

        // Send the original (insertion-ordered) body bytes — server doesn't
        // need them sorted, only the sign canon matters for verification.
        let body_bytes = serde_json::to_vec(&body)?;
        let resp = self
            .http
            .post(ORDER_PLACE_URL)
            .headers(headers)
            .body(body_bytes)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("BingX HTTP {}: {}", status, text));
        }
        let v: Value =
            serde_json::from_str(&text).map_err(|e| anyhow!("non-JSON ({e}): {text}"))?;
        if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
            if code != 0 {
                let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
                return Err(anyhow!("BingX code={} msg={} body={}", code, msg, text));
            }
        }
        Ok(v)
    }

    /// Cancel a resting order by its `orderId`. Used to retract a maker-side
    /// limit entry that didn't fill within our wait window.
    pub async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<Value> {
        let ts = Self::now_ms();
        let body = json!({
            "userId": self.id.user_id,
            "symbol": symbol,
            "orderId": order_id,
        });
        let trace_id = Uuid::new_v4().simple().to_string();
        let body_canon = canonicalize_body(&body);
        let sign = compute_sign(&self.id, &ts.to_string(), &trace_id, &body_canon);
        let headers = self.build_headers(&ts.to_string(), &trace_id, &sign)?;
        let body_bytes = serde_json::to_vec(&body)?;
        let resp = self
            .http
            .post(ORDER_CANCEL_URL)
            .headers(headers)
            .body(body_bytes)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("BingX cancel HTTP {}: {}", status, text));
        }
        let v: Value =
            serde_json::from_str(&text).map_err(|e| anyhow!("non-JSON ({e}): {text}"))?;
        if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
            if code != 0 {
                let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
                return Err(anyhow!("BingX cancel code={} msg={} body={}", code, msg, text));
            }
        }
        Ok(v)
    }

    pub async fn list_positions(&self) -> Result<Value> {
        // GET endpoint; sign rules differ from POST (we haven't fully
        // verified GET parameter signing yet). Empty body → c="{}".
        let ts = Self::now_ms();
        let trace_id = Uuid::new_v4().simple().to_string();
        let sign = compute_sign(&self.id, &ts.to_string(), &trace_id, "{}");
        let headers = self.build_headers(&ts.to_string(), &trace_id, &sign)?;
        let resp = self.http.get(POSITIONS_URL).headers(headers).send().await?;
        let v: Value = resp.json().await?;
        Ok(v)
    }
}

/// Compute `sign` per the BingX webapp algorithm.
///
/// `body_canon` is the result of [`canonicalize_body`] (or "{}" for empty).
pub fn compute_sign(
    id: &BingxIdentity,
    timestamp_ms: &str,
    trace_id: &str,
    body_canon: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(SECRET.as_bytes());
    h.update(timestamp_ms.as_bytes());
    h.update(trace_id.as_bytes());
    h.update(id.device_id.as_bytes());
    h.update(id.platform_id.as_bytes());
    h.update(id.app_version.as_bytes());
    h.update(id.anti_device_id.as_bytes());
    h.update(body_canon.as_bytes());
    let digest = h.finalize();
    hex::encode(digest).to_uppercase()
}

/// Canonicalize a JSON body for sign computation:
/// - empty object → `"{}"`
/// - sort all object keys alphabetically (recursively)
/// - coerce numbers / booleans to their JS-style string form
///   (`0` → `"0"`, `true` → `"true"`, `false` → `"false"`, etc.)
/// - no whitespace; `,` and `:` separators
pub fn canonicalize_body(v: &Value) -> String {
    if v.is_object() && v.as_object().map(|o| o.is_empty()).unwrap_or(false) {
        return "{}".to_string();
    }
    let coerced = coerce_for_sign(v);
    canonical_stringify(&coerced)
}

fn coerce_for_sign(v: &Value) -> Value {
    match v {
        Value::Bool(b) => Value::String(if *b { "true".into() } else { "false".into() }),
        Value::Number(n) => Value::String(format_number(n)),
        Value::Array(a) => Value::Array(a.iter().map(coerce_for_sign).collect()),
        Value::Object(o) => {
            let mut m = Map::new();
            for (k, val) in o {
                m.insert(k.clone(), coerce_for_sign(val));
            }
            Value::Object(m)
        }
        other => other.clone(),
    }
}

/// Mirror JS `Number.prototype.toString()`:
/// - integers: no decimal point (`0`, `1778002277730`)
/// - floats: shortest round-trip; if value has no fractional part it is still
///   printed without trailing `.0` if the original `serde_json::Number` was
///   parsed as integer. Floats keep their representation.
fn format_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    if let Some(f) = n.as_f64() {
        // Reuse serde_json's float printer (which uses ryu) for shortest
        // round-trip form. Strips trailing ".0" when value is integral.
        let s = serde_json::Number::from_f64(f)
            .map(|n| n.to_string())
            .unwrap_or_else(|| f.to_string());
        return s;
    }
    n.to_string()
}

/// Serialize a JSON value with sorted object keys, no whitespace, "," and ":"
/// separators. Mirrors JS `JSON.stringify(obj)` after a sort_keys pass.
fn canonical_stringify(v: &Value) -> String {
    let mut out = String::new();
    stringify_inner(v, &mut out);
    out
}

fn stringify_inner(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            // Use serde_json's escape/quote logic.
            out.push_str(&serde_json::to_string(s).expect("string"));
        }
        Value::Array(a) => {
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                stringify_inner(item, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            let mut first = true;
            for k in keys {
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(&serde_json::to_string(k).expect("k"));
                out.push(':');
                stringify_inner(&o[k], out);
            }
            out.push('}');
        }
    }
}

/// Render a base-coin quantity for `entrustVolume`. BingX's UI sends 4-8
/// decimal places depending on `qtyDigitNum`; 4 is correct for BTC and a
/// safe default for thin alts (will be truncated by BingX to its lot
/// size). We strip trailing zeros and the decimal point if integral so
/// e.g. `7.0` renders as `"7"`.
fn format_qty(qty: f64) -> String {
    let s = format!("{:.6}", qty);
    let t = s.trim_end_matches('0').trim_end_matches('.');
    if t.is_empty() { "0".into() } else { t.to_string() }
}

/// Convert a USD-notional target to a base-coin quantity at `last_price`.
/// Returned value should still be rounded to the symbol's lot size by BingX.
pub fn coin_qty_for_usd(size_usd: f64, last_price: f64) -> f64 {
    if last_price <= 0.0 {
        return 0.0;
    }
    size_usd / last_price
}

/// Best-effort extraction of the order's outcome.
#[derive(Debug, Clone, Deserialize)]
pub struct PlaceOrderFill {
    pub order_id: Option<String>,
    pub avg_filled_price: Option<f64>,
    pub cum_filled_volume: Option<f64>,
    pub state: Option<i64>,
    pub side: Option<String>,
}

pub fn extract_fill(resp: &Value) -> PlaceOrderFill {
    let d = resp.get("data");
    PlaceOrderFill {
        order_id: d
            .and_then(|d| d.get("orderId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        avg_filled_price: d.and_then(|d| d.get("avgFilledPrice")).and_then(|v| v.as_f64()),
        cum_filled_volume: d
            .and_then(|d| d.get("cumFilledVolume"))
            .and_then(|v| v.as_f64()),
        state: d.and_then(|d| d.get("state")).and_then(|v| v.as_i64()),
        side: d
            .and_then(|d| d.get("side"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> BingxIdentity {
        BingxIdentity {
            user_id: "1507939247354732548".into(),
            device_id: "be3bcb3d632948ddab2aed0a3f1b45d4".into(),
            anti_device_id: "22A728646696180140C05C55_9ceda1316928".into(),
            platform_id: "30".into(),
            app_version: "5.12.3".into(),
            jwt: "<unused-in-tests>".into(),
            user_agent: "test".into(),
        }
    }

    /// Captured Limit Bid Open BTC — verifies sign against the real one
    /// pulled from the browser's network panel.
    #[test]
    fn captured_limit_bid_open() {
        let body: Value = serde_json::from_str(
            r#"{"userId":"1507939247354732548","symbol":"BTC-USDT","entrustVolume":"0.0001","entrustPrice":"81361.6","side":"Bid","action":"Open","tradeType":"Limit","volumeType":"Value","tradingUnit":"COIN","triggerCloseV2":{"feeType":0},"isOpponentPrice":1,"timeInForce":"GTC","onlyReduce":false,"supportNonCryptoOneway":true}"#,
        ).unwrap();
        let canon = canonicalize_body(&body);
        let sig = compute_sign(
            &id(),
            "1778002233653",
            "039cf4f1aec249fe998644b160964199",
            &canon,
        );
        assert_eq!(
            sig,
            "0A434E8BF0B9CF6846534B0D110B1C9793D3103DA241D68593FE8F6E0BA41A08"
        );
    }

    /// Captured Market Close (with positionId).
    #[test]
    fn captured_market_close() {
        let body: Value = serde_json::from_str(
            r#"{"userId":"1507939247354732548","symbol":"BTC-USDT","entrustVolume":1,"entrustPrice":0,"side":"Ask","action":"Close","tradeType":"Market","volumeType":"Percentage","tradingUnit":"COIN","isOpponentPrice":0,"onlyReduce":true,"positionId":"2051716191651835905","tradePrice":"81331.0","tradeTs":1778002252570,"supportNonCryptoOneway":true}"#,
        ).unwrap();
        let canon = canonicalize_body(&body);
        let sig = compute_sign(
            &id(),
            "1778002252887",
            "c95593de13a24d908e9f6412e80f7b91",
            &canon,
        );
        assert_eq!(
            sig,
            "694D38119BAE259E7F597283DF2854E394DBF70E3AF66ED5344FF0C2926F1683"
        );
    }

    /// Captured Market Bid Open.
    #[test]
    fn captured_market_bid_open() {
        let body: Value = serde_json::from_str(
            r#"{"userId":"1507939247354732548","symbol":"BTC-USDT","entrustVolume":"0.0001","entrustPrice":0,"side":"Bid","action":"Open","tradeType":"Market","volumeType":"Value","tradingUnit":"COIN","triggerCloseV2":{"feeType":0},"isOpponentPrice":0,"onlyReduce":false,"tradePrice":"81308.3","tradeTs":1778002277730,"supportNonCryptoOneway":true}"#,
        ).unwrap();
        let canon = canonicalize_body(&body);
        let sig = compute_sign(
            &id(),
            "1778002278023",
            "1204b15ac16e431daa8d1822f4a04cff",
            &canon,
        );
        assert_eq!(
            sig,
            "A0F5D8431D27AA6B2558461006FB31B0E8D03B9E3F0C7264E3C3D4E9CC7BFF3D"
        );
    }
}

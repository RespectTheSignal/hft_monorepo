//! Official BingX swap API client — narrow scope: position queries + balance.
//!
//! Orders are placed through the web-API path (`bingx_helpers.rs`) because
//! that's where the Banner fee rebate lives. The official API is used here
//! only for:
//!
//! - `GET /openApi/swap/v2/user/positions` → resolve `positionId` after a
//!   web-API open. Without this, we can't close (web API close requires
//!   positionId in hedge mode; in one-way mode it's needed too per
//!   `code 104414` empirical test 2026-05-05).
//! - `GET /openApi/swap/v2/user/balance` → equity + available margin for
//!   the kill switch.
//! - (future) `POST /openApi/user/auth/userDataStream` + listenKey WS for
//!   push-based position updates.
//!
//! Auth: standard exchange HMAC-SHA256 of the URL query string with the
//! API secret. Signature appended as `&signature=<hex_lower>`. Header
//! `X-BX-APIKEY` carries the key.

use std::time::Duration;

use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;

const BASE_URL: &str = "https://open-api.bingx.com";

type HmacSha256 = Hmac<Sha256>;

pub struct BingxOfficialClient {
    http: reqwest::Client,
    api_key: String,
    api_secret: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Position {
    #[serde(rename = "positionId")]
    pub position_id: String,
    pub symbol: String,
    /// "LONG" | "SHORT"
    #[serde(rename = "positionSide")]
    pub position_side: String,
    /// Position size in base coin units. Comes back as a string in BingX
    /// JSON; deserialize as String then parse.
    #[serde(rename = "positionAmt", default)]
    pub position_amt: String,
    #[serde(rename = "avgPrice", default)]
    pub avg_price: String,
    #[serde(rename = "leverage", default)]
    pub leverage: serde_json::Value,
    #[serde(rename = "unrealizedProfit", default)]
    pub unrealized_profit: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BalanceData {
    pub balance: BalanceDetail,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BalanceDetail {
    #[serde(default)]
    pub balance: String,
    #[serde(default)]
    pub equity: String,
    #[serde(rename = "availableMargin", default)]
    pub available_margin: String,
    #[serde(rename = "usedMargin", default)]
    pub used_margin: String,
    #[serde(rename = "unrealizedProfit", default)]
    pub unrealized_profit: String,
    #[serde(rename = "realisedProfit", default)]
    pub realised_profit: String,
}

impl BingxOfficialClient {
    pub fn new(api_key: String, api_secret: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .gzip(true)
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            api_key,
            api_secret,
        })
    }

    fn sign(&self, params: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes())
            .expect("hmac key any size");
        mac.update(params.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    async fn signed_get(&self, path: &str, extra_params: &[(&str, &str)]) -> Result<Value> {
        let ts = Self::now_ms().to_string();
        // Query string ORDER MATTERS for the signature: BingX requires
        // exactly the same string we sign and the string we send. We build
        // both from the same source.
        let mut parts: Vec<(String, String)> = extra_params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        parts.push(("timestamp".into(), ts));
        let qs = parts
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let sig = self.sign(&qs);
        let url = format!("{}{}?{}&signature={}", BASE_URL, path, qs, sig);
        let resp = self
            .http
            .get(&url)
            .header("X-BX-APIKEY", &self.api_key)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("BingX official HTTP {} on {}: {}", status, path, text));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow!("non-JSON ({e}): {text}"))?;
        if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
            if code != 0 {
                let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
                return Err(anyhow!("BingX official code={} msg={} body={}", code, msg, text));
            }
        }
        Ok(v)
    }

    pub async fn get_positions(&self) -> Result<Vec<Position>> {
        let v = self.signed_get("/openApi/swap/v2/user/positions", &[]).await?;
        let data = v
            .get("data")
            .ok_or_else(|| anyhow!("positions: missing data"))?;
        let arr = data
            .as_array()
            .ok_or_else(|| anyhow!("positions data not array: {data:?}"))?;
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            match serde_json::from_value::<Position>(item.clone()) {
                Ok(p) => out.push(p),
                Err(e) => tracing::warn!(error = %e, item = %item, "[bingx-api] bad position row"),
            }
        }
        Ok(out)
    }

    /// List positions filtered by symbol (BingX accepts the `symbol` query
    /// param). For the close path we usually call this and pick the first
    /// non-zero match.
    pub async fn get_positions_for(&self, symbol: &str) -> Result<Vec<Position>> {
        let v = self
            .signed_get("/openApi/swap/v2/user/positions", &[("symbol", symbol)])
            .await?;
        let data = v
            .get("data")
            .ok_or_else(|| anyhow!("positions: missing data"))?;
        let arr = data
            .as_array()
            .ok_or_else(|| anyhow!("positions data not array: {data:?}"))?;
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            if let Ok(p) = serde_json::from_value::<Position>(item.clone()) {
                out.push(p);
            }
        }
        Ok(out)
    }

    pub async fn get_balance(&self) -> Result<BalanceDetail> {
        let v = self.signed_get("/openApi/swap/v2/user/balance", &[]).await?;
        let data: BalanceData = serde_json::from_value(
            v.get("data").cloned().ok_or_else(|| anyhow!("missing data"))?,
        )?;
        Ok(data.balance)
    }

    /// Cancel a resting order via the official API. Used when our maker
    /// limit hasn't filled in time and we want to avoid stale orders
    /// pile-up. Note: cancellation doesn't affect the Banner fee rebate
    /// (no fees on unfilled cancels), so using the official API path here
    /// is harmless for fee economics.
    pub async fn cancel_order(&self, symbol: &str, order_id: &str) -> Result<Value> {
        let ts = Self::now_ms().to_string();
        let qs = format!("symbol={symbol}&orderId={order_id}&timestamp={ts}");
        let sig = self.sign(&qs);
        let url = format!(
            "{}{}?{}&signature={}",
            BASE_URL, "/openApi/swap/v2/trade/order", qs, sig
        );
        let resp = self
            .http
            .delete(&url)
            .header("X-BX-APIKEY", &self.api_key)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!(
                "cancel HTTP {} symbol={} order={}: {}",
                status,
                symbol,
                order_id,
                text
            ));
        }
        let v: Value = serde_json::from_str(&text)?;
        Ok(v)
    }
}

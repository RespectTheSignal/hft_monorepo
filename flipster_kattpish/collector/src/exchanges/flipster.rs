use super::ExchangeCollector;
use crate::model::{BookTick, ExchangeName};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::TimeZone;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const HOST: &str = "https://trading-api.flipster.io";
const UA: &str = "flipster-research/0.1";

/// Flipster public market WebSocket.
///
/// Endpoint:  wss://trading-api.flipster.io/api/v1/stream
/// Auth:      HMAC-SHA256 over ("GET" + "/api/v1/stream" + expires) with the
///            API secret. Sent as three handshake headers:
///              api-key, api-expires, api-signature.
/// Subscribe: {"op":"subscribe","args":["ticker.BTCUSDT.PERP", ...]}
/// Frame:     {"topic":"ticker.BTCUSDT.PERP","ts":<nanos>,
///             "data":[{"actionType":"UPDATE","rows":[Ticker,...]}]}
///
/// Ticker schema (from REST get-tickers docs):
///   symbol, bidPrice, askPrice, lastPrice, markPrice, indexPrice,
///   fundingRate, nextFundingTime, ...
/// Decimals are encoded as strings for precision.
pub struct FlipsterCollector {
    pub api_key: String,
    pub api_secret: String,
}

const STREAM_PATH: &str = "/api/v1/stream";

impl FlipsterCollector {
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("FLIPSTER_API_KEY").ok()?;
        let secret = std::env::var("FLIPSTER_API_SECRET").ok()?;
        if key.is_empty() || secret.is_empty() {
            return None;
        }
        Some(Self { api_key: key, api_secret: secret })
    }

    fn sign(&self, expires: i64) -> String {
        let msg = format!("GET{STREAM_PATH}{expires}");
        let mut mac =
            HmacSha256::new_from_slice(self.api_secret.as_bytes()).expect("hmac key");
        mac.update(msg.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn sign_path(&self, path: &str, expires: i64) -> String {
        let mut mac =
            HmacSha256::new_from_slice(self.api_secret.as_bytes()).expect("hmac key");
        mac.update(format!("GET{path}{expires}").as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Fetch the FULL market-available symbol list. Uses
    /// `GET /api/v1/market/contract` — this returns *all* listed contracts
    /// (hundreds), not just the tiny set that is currently API-tradable via
    /// `/trade/symbol`. Response is an array of Contract objects; we keep
    /// only `.PERP` symbols.
    pub async fn fetch_perp_symbols(&self) -> Result<Vec<String>> {
        let path = "/api/v1/market/contract";
        let expires = chrono::Utc::now().timestamp() + 60;
        let sig = self.sign_path(path, expires);
        let client = reqwest::Client::builder().user_agent(UA).build()?;
        let resp = client
            .get(format!("{HOST}{path}"))
            .header("api-key", &self.api_key)
            .header("api-expires", expires.to_string())
            .header("api-signature", sig)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "flipster contract list HTTP {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            ));
        }
        let v: Value = resp.json().await?;
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow!("contract list not an array"))?;
        let mut out: Vec<String> = arr
            .iter()
            .filter_map(|c| c.get("symbol").and_then(|s| s.as_str()))
            .filter(|s| s.ends_with(".PERP"))
            .map(|s| s.to_string())
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }
}

#[async_trait]
impl ExchangeCollector for FlipsterCollector {
    fn name(&self) -> ExchangeName {
        ExchangeName::Flipster
    }
    fn ws_url(&self) -> String {
        format!("wss://trading-api.flipster.io{STREAM_PATH}")
    }
    fn handshake_headers(&self) -> Vec<(String, String)> {
        let expires = chrono::Utc::now().timestamp() + 60;
        let sig = self.sign(expires);
        vec![
            ("api-key".into(), self.api_key.clone()),
            ("api-expires".into(), expires.to_string()),
            ("api-signature".into(), sig),
        ]
    }
    fn subscribe_msgs(&self, symbols: &[String]) -> Vec<String> {
        // 500+ symbols would blow up a single subscribe frame. Batch in
        // chunks of 50 to stay well under any frame size limit.
        symbols
            .chunks(50)
            .map(|chunk| {
                let args: Vec<String> =
                    chunk.iter().map(|s| format!("ticker.{s}")).collect();
                serde_json::json!({"op":"subscribe","args":args}).to_string()
            })
            .collect()
    }
    fn parse_frame(&self, frame: &str) -> Vec<BookTick> {
        let Ok(v): Result<Value, _> = serde_json::from_str(frame) else {
            return vec![];
        };
        let Some(topic) = v.get("topic").and_then(|x| x.as_str()) else {
            return vec![];
        };
        if !topic.starts_with("ticker.") {
            return vec![];
        }
        let Some(data_arr) = v.get("data").and_then(|x| x.as_array()) else {
            return vec![];
        };

        // Envelope ts is nanosecond integer (as number or string).
        let env_ts_ns = v
            .get("ts")
            .and_then(|x| {
                x.as_i64()
                    .or_else(|| x.as_str().and_then(|s| s.parse::<i64>().ok()))
            })
            .unwrap_or(0);
        let env_ts = if env_ts_ns > 0 {
            chrono::Utc
                .timestamp_nanos(env_ts_ns)
        } else {
            chrono::Utc::now()
        };

        let dec = |row: &Value, k: &str| -> Option<f64> {
            row.get(k).and_then(|x| {
                x.as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .or_else(|| x.as_f64())
            })
        };

        let mut out = Vec::new();
        for entry in data_arr {
            let Some(rows) = entry.get("rows").and_then(|x| x.as_array()) else {
                continue;
            };
            for row in rows {
                let Some(sym) = row.get("symbol").and_then(|x| x.as_str()) else {
                    continue;
                };
                let bid = dec(row, "bidPrice").unwrap_or(0.0);
                let ask = dec(row, "askPrice").unwrap_or(0.0);
                // Flipster sometimes publishes one side as 0 when the book
                // is empty or stale. If we accept those, mid collapses to
                // half of the populated side and strategies see fake
                // extreme spreads. Require both sides > 0.
                if bid <= 0.0 || ask <= 0.0 {
                    continue;
                }
                out.push(BookTick {
                    exchange: ExchangeName::Flipster,
                    symbol: sym.to_string(),
                    bid_price: bid,
                    ask_price: ask,
                    bid_size: 0.0, // not present in ticker stream
                    ask_size: 0.0,
                    last_price: dec(row, "lastPrice"),
                    mark_price: dec(row, "markPrice"),
                    index_price: dec(row, "indexPrice"),
                    timestamp: env_ts,
                });
            }
        }
        out
    }
}

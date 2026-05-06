//! Variational Omni — perp DEX, custom WS protocol.
//!
//! Endpoint: `wss://omni-ws-server.prod.ap-northeast-1.variational.io/prices`
//!
//! Reverse-engineered subscribe format (from `omni.variational.io` JS bundle
//! 2026-05-06; chunk `CDGZZqOc.js` calls `M.send(JSON.stringify({action,
//! instruments: e.map(A)}))` where `A` returns `{underlying, instrument_type:
//! "perpetual_future", settlement_asset:"USDC", funding_interval_s:3600,
//! dex_token_details}`):
//!
//! ```json
//! {"action":"subscribe","instruments":[
//!   {"underlying":"BTC","instrument_type":"perpetual_future",
//!    "settlement_asset":"USDC","funding_interval_s":3600}
//! ]}
//! ```
//!
//! Frame:
//! ```json
//! {"channel":"instrument_price:P-BTC-USDC-3600",
//!  "pricing":{"price":"81265.77","native_price":"0.9995",
//!             "delta":"1","gamma":"0","theta":"0","vega":"0","rho":"0",
//!             "iv":"0","underlying_price":"81306.29",
//!             "interest_rate":"-0.00005...","timestamp":"2026-05-06T..."}}
//! ```
//!
//! We use **`underlying_price`** (the spot/index ref) as our mid, not `price`
//! (the perp mark which carries premium/funding offset). For lead-lag vs
//! Binance, underlying_price tracks the real fair value.
//!
//! Universe: We default to a curated set of 30+ majors. Override with
//! `VARIATIONAL_UNDERLYINGS` env (comma-separated bases).

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use serde_json::Value;

use super::ExchangeCollector;
use crate::model::{BookTick, ExchangeName};

const DEFAULT_UNDERLYINGS: &[&str] = &[
    "BTC", "ETH", "SOL", "HYPE", "BNB", "XRP", "DOGE", "ADA", "AVAX", "LINK",
    "TRX", "DOT", "MATIC", "POL", "LTC", "BCH", "NEAR", "ATOM", "TON", "SUI",
    "APT", "ARB", "OP", "INJ", "TIA", "PYTH", "JUP", "WIF", "FARTCOIN", "ENA",
    "PENDLE", "AAVE", "MNT", "SEI", "STRK", "TAO",
];

pub struct VariationalCollector {
    pub underlyings: Arc<StdMutex<Vec<String>>>,
}

impl VariationalCollector {
    pub fn new(underlyings: Vec<String>) -> Self {
        let list = if underlyings.is_empty() {
            DEFAULT_UNDERLYINGS.iter().map(|s| s.to_string()).collect()
        } else {
            underlyings
        };
        Self {
            underlyings: Arc::new(StdMutex::new(list)),
        }
    }
}

#[async_trait]
impl ExchangeCollector for VariationalCollector {
    fn name(&self) -> ExchangeName {
        ExchangeName::Variational
    }
    fn ws_url(&self) -> String {
        "wss://omni-ws-server.prod.ap-northeast-1.variational.io/prices".to_string()
    }
    fn subscribe_msgs(&self, _symbols: &[String]) -> Vec<String> {
        let list = self.underlyings.lock().unwrap();
        let instruments: Vec<Value> = list
            .iter()
            .map(|u| {
                serde_json::json!({
                    "underlying": u,
                    "instrument_type": "perpetual_future",
                    "settlement_asset": "USDC",
                    "funding_interval_s": 3600
                })
            })
            .collect();
        let payload = serde_json::json!({
            "action": "subscribe",
            "instruments": instruments
        });
        vec![payload.to_string()]
    }
    fn parse_frame(&self, frame: &str) -> Vec<BookTick> {
        let Ok(v): Result<Value, _> = serde_json::from_str(frame) else {
            return vec![];
        };
        // Skip heartbeats + non-pricing events.
        let Some(channel) = v.get("channel").and_then(|x| x.as_str()) else {
            return vec![];
        };
        // Channel format: "instrument_price:P-<UNDERLYING>-USDC-<INTERVAL>"
        // Strip prefix, split on '-'. The underlying may itself contain a
        // hyphen (uncommon) — for safety, take the second component which is
        // always the underlying for perpetual_future.
        let Some(rest) = channel.strip_prefix("instrument_price:") else {
            return vec![];
        };
        let parts: Vec<&str> = rest.split('-').collect();
        // Expected: ["P", "<UNDERLYING>", "USDC", "3600"]. Reject anything else.
        if parts.len() < 4 || parts[0] != "P" {
            return vec![];
        }
        let underlying = parts[1].to_string();
        let Some(pricing) = v.get("pricing") else {
            return vec![];
        };
        // Prefer underlying_price (spot index) for lead-lag. Fall back to
        // perp `price` if missing.
        let pick = |k: &str| {
            pricing
                .get(k)
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|p| *p > 0.0)
        };
        let mid = pick("underlying_price").or_else(|| pick("price"));
        let Some(mid) = mid else {
            return vec![];
        };
        let ts = pricing
            .get("timestamp")
            .and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);
        vec![BookTick {
            exchange: ExchangeName::Variational,
            symbol: underlying,
            bid_price: mid,
            ask_price: mid,
            bid_size: 0.0,
            ask_size: 0.0,
            last_price: pick("price"),
            mark_price: pick("price"),
            index_price: pick("underlying_price"),
            timestamp: ts,
        }]
    }
}

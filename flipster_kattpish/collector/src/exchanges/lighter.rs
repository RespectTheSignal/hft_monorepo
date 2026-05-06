//! Lighter (zkLighter) — perp DEX, msgpack-or-JSON WS.
//!
//! Endpoint: `wss://mainnet.zklighter.elliot.ai/stream?encoding=json&readonly=true`
//! Subscribe: `{"type":"subscribe","channel":"market_stats/all"}`
//!
//! Frame shape (after subscribe + initial snapshot):
//! ```json
//! {"channel":"market_stats:all",
//!  "market_stats":{"0":{"symbol":"ETH","market_id":0,"index_price":"...",
//!                       "mark_price":"...","mid_price":"...",
//!                       "last_trade_price":"...",...}}}
//! ```
//!
//! No public bookTicker stream is used — we map `mid_price` to both bid
//! and ask so downstream `mid()` math works (no real BBO available without
//! also subscribing to `order_book/<market_id>` per market). For lead-lag
//! research vs Binance, mid_price is good enough — typically tracks the
//! true mid within ~1 bp on majors.

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use chrono::TimeZone;
use serde_json::Value;

use super::ExchangeCollector;
use crate::model::{BookTick, ExchangeName};

pub struct LighterCollector {
    pub symbols: Arc<StdMutex<Vec<String>>>,
}

impl LighterCollector {
    pub fn new(symbols: Vec<String>) -> Self {
        Self {
            symbols: Arc::new(StdMutex::new(symbols)),
        }
    }
}

#[async_trait]
impl ExchangeCollector for LighterCollector {
    fn name(&self) -> ExchangeName {
        ExchangeName::Lighter
    }
    fn ws_url(&self) -> String {
        // JSON encoding (msgpack also works but we'd need a parser).
        "wss://mainnet.zklighter.elliot.ai/stream?encoding=json&readonly=true".to_string()
    }
    fn subscribe_msgs(&self, _symbols: &[String]) -> Vec<String> {
        // One firehose covers every market.
        vec![r#"{"type":"subscribe","channel":"market_stats/all"}"#.to_string()]
    }
    fn parse_frame(&self, frame: &str) -> Vec<BookTick> {
        let Ok(v): Result<Value, _> = serde_json::from_str(frame) else {
            return vec![];
        };
        // Skip non-stats events ({"type":"connected",...}).
        let Some(stats) = v.get("market_stats").and_then(|x| x.as_object()) else {
            return vec![];
        };
        let now = chrono::Utc::now();
        let ts_ms = v.get("timestamp").and_then(|x| x.as_i64()).unwrap_or(0);
        let ts = if ts_ms > 0 {
            chrono::Utc
                .timestamp_millis_opt(ts_ms)
                .single()
                .unwrap_or(now)
        } else {
            now
        };
        let mut out = Vec::with_capacity(stats.len());
        for (_id, item) in stats {
            let Some(sym) = item.get("symbol").and_then(|x| x.as_str()) else {
                continue;
            };
            // Prefer mid_price (computed BBO mid). Fall back to mark_price,
            // then last_trade_price. Skip if all missing/non-numeric.
            let pick = |k: &str| {
                item.get(k)
                    .and_then(|x| x.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|p| *p > 0.0)
            };
            let mid = pick("mid_price").or_else(|| pick("mark_price")).or_else(|| pick("last_trade_price"));
            let Some(mid) = mid else {
                continue;
            };
            out.push(BookTick {
                exchange: ExchangeName::Lighter,
                symbol: sym.to_string(),
                bid_price: mid,
                ask_price: mid,
                bid_size: 0.0,
                ask_size: 0.0,
                last_price: pick("last_trade_price"),
                mark_price: pick("mark_price"),
                index_price: pick("index_price"),
                timestamp: ts,
            });
        }
        out
    }
}

//! AsterDex perp (Binance fork). All-symbol bookTicker firehose.
//!
//! Endpoint: `wss://fstream5.asterdex.com/plain/stream?streams=!bookTicker`
//! Frames: `{"stream":"!bookTicker","data":{"e":"bookTicker","u":...,"s":"BTCUSDT","b":"...","B":"...","a":"...","A":"...","T":...,"E":...}}`
//!
//! Format identical to Binance bookTicker — only the host differs. Browser
//! origin/cookies aren't required for the public stream (verified probe
//! 2026-05-06).

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use chrono::TimeZone;
use serde_json::Value;

use super::ExchangeCollector;
use crate::model::{BookTick, ExchangeName};

pub struct AsterCollector {
    pub symbols: Arc<StdMutex<Vec<String>>>,
}

impl AsterCollector {
    pub fn new(symbols: Vec<String>) -> Self {
        Self {
            symbols: Arc::new(StdMutex::new(symbols)),
        }
    }
}

#[async_trait]
impl ExchangeCollector for AsterCollector {
    fn name(&self) -> ExchangeName {
        ExchangeName::Aster
    }
    fn ws_url(&self) -> String {
        // !bookTicker is the all-symbols firehose. No need to enumerate
        // symbols ourselves — saves a /exchangeInfo round trip and the
        // collector immediately sees new listings.
        "wss://fstream5.asterdex.com/plain/stream?streams=!bookTicker".to_string()
    }
    fn subscribe_msgs(&self, _symbols: &[String]) -> Vec<String> {
        Vec::new()
    }
    fn parse_frame(&self, frame: &str) -> Vec<BookTick> {
        let Ok(v): Result<Value, _> = serde_json::from_str(frame) else {
            return vec![];
        };
        let v = v.get("data").cloned().unwrap_or(v);
        let Some(sym) = v.get("s").and_then(|x| x.as_str()) else {
            return vec![];
        };
        let Some(b) = v.get("b").and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok()) else {
            return vec![];
        };
        let Some(a) = v.get("a").and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok()) else {
            return vec![];
        };
        if b <= 0.0 || a <= 0.0 {
            return vec![];
        }
        let bsz = v.get("B").and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let asz = v.get("A").and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let ts_ms = v.get("T").and_then(|x| x.as_i64()).unwrap_or_else(|| {
            v.get("E").and_then(|x| x.as_i64()).unwrap_or(0)
        });
        let ts = if ts_ms > 0 {
            chrono::Utc.timestamp_millis_opt(ts_ms).single().unwrap_or_else(chrono::Utc::now)
        } else {
            chrono::Utc::now()
        };
        vec![BookTick {
            exchange: ExchangeName::Aster,
            symbol: sym.to_string(),
            bid_price: b,
            ask_price: a,
            bid_size: bsz,
            ask_size: asz,
            last_price: None,
            mark_price: None,
            index_price: None,
            timestamp: ts,
        }]
    }
}

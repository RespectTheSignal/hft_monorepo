//! PancakeSwap perp (Binance fork hosted on BSC). All-symbols bookTicker.
//!
//! Endpoint: `wss://perp-fstream.pancakeswap.finance/plain/stream?streams=!bookTicker`
//! Frame: `{"stream":"!bookTicker","data":{"e":"bookTicker","u":...,
//!         "s":"BTCUSDT","b":"<bid>","B":"<bid_sz>","a":"<ask>","A":"<ask_sz>",
//!         "T":<ts_ms>,"E":<event_ms>}}`
//!
//! 2026-05-06: discovered that PancakeSwap exposes the full Binance-style
//! suite (`!bookTicker`, `@depth`, `@depth5@100ms`, `@trade`, `@kline_1m`,
//! `@markPriceTicker@arr`) — initial collector wrongly used markPriceTicker
//! because that's what the JS bundle most prominently subscribed for the
//! ticker tape. bookTicker gives proper bid/ask/size, identical to Binance.

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use chrono::TimeZone;
use serde_json::Value;

use super::ExchangeCollector;
use crate::model::{BookTick, ExchangeName};

pub struct PancakeCollector {
    pub symbols: Arc<StdMutex<Vec<String>>>,
}

impl PancakeCollector {
    pub fn new(symbols: Vec<String>) -> Self {
        Self {
            symbols: Arc::new(StdMutex::new(symbols)),
        }
    }
}

#[async_trait]
impl ExchangeCollector for PancakeCollector {
    fn name(&self) -> ExchangeName {
        ExchangeName::Pancake
    }
    fn ws_url(&self) -> String {
        "wss://perp-fstream.pancakeswap.finance/plain/stream?streams=!bookTicker".to_string()
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
        let Some(b) = v
            .get("b")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<f64>().ok())
        else {
            return vec![];
        };
        let Some(a) = v
            .get("a")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<f64>().ok())
        else {
            return vec![];
        };
        if b <= 0.0 || a <= 0.0 {
            return vec![];
        }
        let bsz = v
            .get("B")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let asz = v
            .get("A")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let ts_ms = v
            .get("T")
            .and_then(|x| x.as_i64())
            .unwrap_or_else(|| v.get("E").and_then(|x| x.as_i64()).unwrap_or(0));
        let ts = if ts_ms > 0 {
            chrono::Utc
                .timestamp_millis_opt(ts_ms)
                .single()
                .unwrap_or_else(chrono::Utc::now)
        } else {
            chrono::Utc::now()
        };
        vec![BookTick {
            exchange: ExchangeName::Pancake,
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

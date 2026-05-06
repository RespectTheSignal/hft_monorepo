//! BingX swap (USDT-margined perp) public bookTicker WS.
//!
//! Endpoint: `wss://open-api-swap.bingx.com/swap-market`
//! Frames: GZIP-compressed JSON sent as binary.
//! Heartbeat: server sends a text "Ping"; client must reply "Pong".
//!
//! Subscribe payload per symbol:
//! ```json
//! {"id":"<uuid>", "reqType":"sub", "dataType":"BTC-USDT@bookTicker"}
//! ```
//!
//! Frame payload (after gunzip):
//! ```json
//! {"code":0,"dataType":"BTC-USDT@bookTicker",
//!  "data":{"e":"bookTicker","u":...,"s":"BTC-USDT",
//!          "b":"81000.0","B":"1.5","a":"81001.0","A":"2.0","T":171...}}
//! ```

use std::io::Read;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use chrono::TimeZone;
use flate2::read::GzDecoder;
use serde_json::Value;

use super::ExchangeCollector;
use crate::model::{BookTick, ExchangeName};

pub struct BingxCollector {
    pub symbols: Arc<StdMutex<Vec<String>>>,
}

impl BingxCollector {
    pub fn new(symbols: Vec<String>) -> Self {
        Self {
            symbols: Arc::new(StdMutex::new(symbols)),
        }
    }
}

fn gunzip(bytes: &[u8]) -> Option<String> {
    let mut decoder = GzDecoder::new(bytes);
    let mut out = String::new();
    decoder.read_to_string(&mut out).ok()?;
    Some(out)
}

fn parse_book_ticker(text: &str) -> Vec<BookTick> {
    let Ok(v): Result<Value, _> = serde_json::from_str(text) else {
        return Vec::new();
    };
    let Some(data) = v.get("data") else {
        return Vec::new();
    };
    let sym = data
        .get("s")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if sym.is_empty() {
        return Vec::new();
    }
    let bid = data
        .get("b")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let ask = data
        .get("a")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    if bid <= 0.0 || ask <= 0.0 {
        return Vec::new();
    }
    let bsz = data
        .get("B")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let asz = data
        .get("A")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let ts_ms = data.get("T").and_then(|x| x.as_i64()).unwrap_or(0);
    let timestamp = if ts_ms > 0 {
        chrono::Utc
            .timestamp_millis_opt(ts_ms)
            .single()
            .unwrap_or_else(chrono::Utc::now)
    } else {
        chrono::Utc::now()
    };
    vec![BookTick {
        exchange: ExchangeName::Bingx,
        symbol: sym,
        bid_price: bid,
        ask_price: ask,
        bid_size: bsz,
        ask_size: asz,
        last_price: None,
        mark_price: None,
        index_price: None,
        timestamp,
    }]
}

#[async_trait]
impl ExchangeCollector for BingxCollector {
    fn name(&self) -> ExchangeName {
        ExchangeName::Bingx
    }
    fn ws_url(&self) -> String {
        "wss://open-api-swap.bingx.com/swap-market".to_string()
    }
    fn subscribe_msgs(&self, symbols: &[String]) -> Vec<String> {
        symbols
            .iter()
            .enumerate()
            .map(|(i, s)| {
                format!(
                    r#"{{"id":"sub-{i}","reqType":"sub","dataType":"{}@bookTicker"}}"#,
                    s
                )
            })
            .collect()
    }
    fn parse_frame(&self, frame: &str) -> Vec<BookTick> {
        // Some text frames are subscription responses or status; only
        // payload-bearing frames have `dataType` ending in @bookTicker.
        if !frame.contains("bookTicker") {
            return Vec::new();
        }
        parse_book_ticker(frame)
    }
    fn parse_binary_frame(&self, bytes: &[u8]) -> Vec<BookTick> {
        let Some(text) = gunzip(bytes) else {
            return Vec::new();
        };
        if !text.contains("bookTicker") {
            return Vec::new();
        }
        parse_book_ticker(&text)
    }
    /// BingX server periodically sends a text "Ping" frame; reply with "Pong".
    fn pong_text(&self, frame: &str) -> Option<String> {
        if frame.trim() == "Ping" {
            Some("Pong".to_string())
        } else {
            None
        }
    }
}

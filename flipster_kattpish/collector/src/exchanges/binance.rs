use super::ExchangeCollector;
use crate::model::{BookTick, ExchangeName};
use anyhow::Result;
use async_trait::async_trait;
use chrono::TimeZone;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};

/// Fetch the full list of USDT-quoted PERPETUAL symbols currently trading
/// on Binance USDⓈ-M futures. Used at startup to auto-build the arb universe.
pub async fn fetch_usdt_perps() -> Result<HashSet<String>> {
    let client = reqwest::Client::builder()
        .user_agent("flipster-research/0.1")
        .build()?;
    let resp = client
        .get("https://fapi.binance.com/fapi/v1/exchangeInfo")
        .send()
        .await?;
    let v: Value = resp.json().await?;
    let Some(arr) = v.get("symbols").and_then(|x| x.as_array()) else {
        return Ok(HashSet::new());
    };
    let mut out = HashSet::new();
    for s in arr {
        if s.get("contractType").and_then(|x| x.as_str()) != Some("PERPETUAL") {
            continue;
        }
        if s.get("quoteAsset").and_then(|x| x.as_str()) != Some("USDT") {
            continue;
        }
        if s.get("status").and_then(|x| x.as_str()) != Some("TRADING") {
            continue;
        }
        if let Some(sym) = s.get("symbol").and_then(|x| x.as_str()) {
            out.insert(sym.to_string());
        }
    }
    Ok(out)
}

/// Binance collector uses the combined-stream URL form so we don't have to
/// send a SUBSCRIBE frame after connect. Streams are passed in the query
/// string and data frames come wrapped as {"stream": "...", "data": {...}}.
pub struct BinanceCollector {
    // Symbols are captured at construction time because the URL is built
    // from them. The ExchangeCollector trait's ws_url() doesn't take
    // arguments, so we stash them inside the struct.
    pub symbols: Arc<StdMutex<Vec<String>>>,
}

impl BinanceCollector {
    pub fn new(symbols: Vec<String>) -> Self {
        Self { symbols: Arc::new(StdMutex::new(symbols)) }
    }
}

#[async_trait]
impl ExchangeCollector for BinanceCollector {
    fn name(&self) -> ExchangeName {
        ExchangeName::Binance
    }
    fn ws_url(&self) -> String {
        let syms = self.symbols.lock().unwrap();
        let streams: Vec<String> = syms
            .iter()
            .map(|s| format!("{}@bookTicker", s.to_lowercase()))
            .collect();
        // Combined stream URL; data frames wrap payload under "data".
        format!("wss://fstream.binance.com/stream?streams={}", streams.join("/"))
    }
    fn subscribe_msgs(&self, _symbols: &[String]) -> Vec<String> {
        // Combined stream URL already includes the subscriptions — no
        // SUBSCRIBE message needed.
        Vec::new()
    }
    fn parse_frame(&self, frame: &str) -> Vec<BookTick> {
        let Ok(v): Result<Value, _> = serde_json::from_str(frame) else {
            return vec![];
        };
        // Combined stream wraps payload under "data". Non-combined streams
        // put fields at the top level. Handle both.
        let v = v.get("data").cloned().unwrap_or(v);
        // bookTicker stream payload: { u, s, b, B, a, A, T, E }
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
        let ts_ms = v.get("T").and_then(|x| x.as_i64()).unwrap_or_else(|| {
            v.get("E").and_then(|x| x.as_i64()).unwrap_or(0)
        });
        let ts = if ts_ms > 0 {
            chrono::Utc.timestamp_millis_opt(ts_ms).single().unwrap_or_else(chrono::Utc::now)
        } else {
            chrono::Utc::now()
        };
        vec![BookTick {
            exchange: ExchangeName::Binance,
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

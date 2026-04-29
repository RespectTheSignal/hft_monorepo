use super::ExchangeCollector;
use crate::model::{BookTick, ExchangeName};
use async_trait::async_trait;
use chrono::TimeZone;
use serde_json::Value;

pub struct GateCollector;

#[async_trait]
impl ExchangeCollector for GateCollector {
    fn name(&self) -> ExchangeName {
        ExchangeName::Gate
    }
    fn ws_url(&self) -> String {
        "wss://fx-ws.gateio.ws/v4/ws/usdt".into()
    }
    fn subscribe_msgs(&self, symbols: &[String]) -> Vec<String> {
        // Gate futures book_ticker: symbol format BTC_USDT.
        // Gate WS rejects very large single-subscribe payloads, so chunk
        // into batches of 50 symbols per subscribe frame.
        let normalized: Vec<String> = symbols
            .iter()
            .map(|s| {
                if s.contains('_') {
                    s.clone()
                } else if let Some(stripped) = s.strip_suffix("USDT") {
                    format!("{stripped}_USDT")
                } else {
                    s.clone()
                }
            })
            .collect();
        normalized
            .chunks(50)
            .map(|chunk| {
                serde_json::json!({
                    "time": chrono::Utc::now().timestamp(),
                    "channel": "futures.book_ticker",
                    "event": "subscribe",
                    "payload": chunk,
                })
                .to_string()
            })
            .collect()
    }
    fn parse_frame(&self, frame: &str) -> Vec<BookTick> {
        let Ok(v): Result<Value, _> = serde_json::from_str(frame) else {
            return vec![];
        };
        if v.get("channel").and_then(|x| x.as_str()) != Some("futures.book_ticker") {
            return vec![];
        }
        let Some(r) = v.get("result") else {
            return vec![];
        };
        let sym = r.get("s").and_then(|x| x.as_str()).unwrap_or("");
        if sym.is_empty() {
            return vec![];
        }
        let get_f = |k: &str| -> Option<f64> {
            r.get(k)
                .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()))
        };
        let bid = get_f("b").unwrap_or(0.0);
        let ask = get_f("a").unwrap_or(0.0);
        let bsz = r.get("B").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let asz = r.get("A").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let ts_ms = r.get("t").and_then(|x| x.as_i64()).unwrap_or(0);
        let ts = if ts_ms > 0 {
            chrono::Utc
                .timestamp_millis_opt(ts_ms)
                .single()
                .unwrap_or_else(chrono::Utc::now)
        } else {
            chrono::Utc::now()
        };
        vec![BookTick {
            exchange: ExchangeName::Gate,
            symbol: sym.replace('_', ""),
            bid_price: bid,
            ask_price: ask,
            bid_size: bsz,
            ask_size: asz,
            last_price: None,
            mark_price: None,
            index_price: None,
            timestamp: ts,
        }]
    }
}

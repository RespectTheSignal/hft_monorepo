use super::ExchangeCollector;
use crate::model::{BookTick, ExchangeName};
use async_trait::async_trait;
use chrono::TimeZone;
use serde_json::Value;

pub struct BybitCollector;

#[async_trait]
impl ExchangeCollector for BybitCollector {
    fn name(&self) -> ExchangeName {
        ExchangeName::Bybit
    }
    fn ws_url(&self) -> String {
        "wss://stream.bybit.com/v5/public/linear".into()
    }
    fn subscribe_msgs(&self, symbols: &[String]) -> Vec<String> {
        let args: Vec<String> = symbols.iter().map(|s| format!("tickers.{s}")).collect();
        vec![serde_json::json!({"op":"subscribe","args":args}).to_string()]
    }
    fn parse_frame(&self, frame: &str) -> Vec<BookTick> {
        let Ok(v): Result<Value, _> = serde_json::from_str(frame) else {
            return vec![];
        };
        let Some(topic) = v.get("topic").and_then(|x| x.as_str()) else {
            return vec![];
        };
        if !topic.starts_with("tickers.") {
            return vec![];
        }
        let Some(data) = v.get("data") else {
            return vec![];
        };
        let get_f = |k: &str| -> Option<f64> {
            data.get(k)
                .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()))
        };
        let sym = data.get("symbol").and_then(|x| x.as_str()).unwrap_or("");
        if sym.is_empty() {
            return vec![];
        }
        let bid = get_f("bid1Price").unwrap_or(0.0);
        let ask = get_f("ask1Price").unwrap_or(0.0);
        if bid == 0.0 && ask == 0.0 {
            return vec![];
        }
        let bsz = get_f("bid1Size").unwrap_or(0.0);
        let asz = get_f("ask1Size").unwrap_or(0.0);
        let mark = get_f("markPrice");
        let index = get_f("indexPrice");
        let last = get_f("lastPrice");
        let ts_ms = v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0);
        let ts = if ts_ms > 0 {
            chrono::Utc
                .timestamp_millis_opt(ts_ms)
                .single()
                .unwrap_or_else(chrono::Utc::now)
        } else {
            chrono::Utc::now()
        };
        vec![BookTick {
            exchange: ExchangeName::Bybit,
            symbol: sym.to_string(),
            bid_price: bid,
            ask_price: ask,
            bid_size: bsz,
            ask_size: asz,
            last_price: last,
            mark_price: mark,
            index_price: index,
            timestamp: ts,
        }]
    }
}

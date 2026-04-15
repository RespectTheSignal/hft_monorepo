use super::ExchangeCollector;
use crate::model::{BookTick, ExchangeName};
use async_trait::async_trait;
use chrono::TimeZone;
use serde_json::Value;

pub struct BitgetCollector;

#[async_trait]
impl ExchangeCollector for BitgetCollector {
    fn name(&self) -> ExchangeName {
        ExchangeName::Bitget
    }
    fn ws_url(&self) -> String {
        "wss://ws.bitget.com/v2/ws/public".into()
    }
    fn subscribe_msgs(&self, symbols: &[String]) -> Vec<String> {
        let args: Vec<Value> = symbols
            .iter()
            .map(|s| {
                serde_json::json!({
                    "instType": "USDT-FUTURES",
                    "channel": "ticker",
                    "instId": s,
                })
            })
            .collect();
        vec![serde_json::json!({"op":"subscribe","args":args}).to_string()]
    }
    fn parse_frame(&self, frame: &str) -> Vec<BookTick> {
        let Ok(v): Result<Value, _> = serde_json::from_str(frame) else {
            return vec![];
        };
        let Some(arr) = v.get("data").and_then(|d| d.as_array()) else {
            return vec![];
        };
        let mut out = Vec::new();
        for d in arr {
            let get_f = |k: &str| -> Option<f64> {
                d.get(k)
                    .and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()))
            };
            let sym = d.get("instId").and_then(|x| x.as_str()).unwrap_or("");
            if sym.is_empty() {
                continue;
            }
            let bid = get_f("bidPr").unwrap_or(0.0);
            let ask = get_f("askPr").unwrap_or(0.0);
            let bsz = get_f("bidSz").unwrap_or(0.0);
            let asz = get_f("askSz").unwrap_or(0.0);
            let mark = get_f("markPrice");
            let index = get_f("indexPrice");
            let last = get_f("lastPr");
            let ts_ms = d
                .get("ts")
                .and_then(|x| x.as_str().and_then(|s| s.parse::<i64>().ok()))
                .unwrap_or(0);
            let ts = if ts_ms > 0 {
                chrono::Utc
                    .timestamp_millis_opt(ts_ms)
                    .single()
                    .unwrap_or_else(chrono::Utc::now)
            } else {
                chrono::Utc::now()
            };
            out.push(BookTick {
                exchange: ExchangeName::Bitget,
                symbol: sym.to_string(),
                bid_price: bid,
                ask_price: ask,
                bid_size: bsz,
                ask_size: asz,
                last_price: last,
                mark_price: mark,
                index_price: index,
                timestamp: ts,
            });
        }
        out
    }
}

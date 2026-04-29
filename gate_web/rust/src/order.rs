use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Long,
    Short,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderType {
    Market,
    Limit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeInForce {
    /// Good till cancel
    Gtc,
    /// Immediate or cancel
    Ioc,
    /// Pending or cancel (post-only)
    Poc,
}

#[derive(Debug, Clone)]
pub struct OrderParams {
    pub side: Side,
    /// Number of contracts (unsigned; sign is derived from `side`).
    pub size: i64,
    pub order_type: OrderType,
    /// Required for limit orders; ignored for market.
    pub price: Option<f64>,
    pub tif: TimeInForce,
    pub reduce_only: bool,
    /// Optional custom label, e.g. "t-1234567890".
    pub text: Option<String>,
}

impl OrderParams {
    pub fn to_body(&self, contract: &str) -> serde_json::Value {
        let signed_size = match self.side {
            Side::Long => self.size,
            Side::Short => -self.size,
        };
        let price_str = self
            .price
            .map(|p| format_price(p))
            .unwrap_or_else(|| "0".to_string());
        let text = self
            .text
            .clone()
            .unwrap_or_else(|| format!("t-{}", now_ms()));

        serde_json::json!({
            "contract": contract,
            "size": signed_size.to_string(),
            "price": price_str,
            "order_type": tif_or_type_to_str(self.order_type),
            "tif": tif_to_str(self.tif),
            "text": text,
            "reduce_only": self.reduce_only,
        })
    }

    /// Build a reduce-only market IOC body to close `signed_size` contracts.
    /// Positive = close short (buy), negative = close long (sell).
    pub fn close_body(contract: &str, signed_size: i64, price: Option<f64>) -> serde_json::Value {
        let price_str = price
            .filter(|p| *p > 0.0)
            .map(format_price)
            .unwrap_or_else(|| "0".to_string());
        serde_json::json!({
            "contract": contract,
            "size": signed_size.to_string(),
            "price": price_str,
            "order_type": "market",
            "tif": "ioc",
            "text": format!("t-{}", now_ms()),
            "reduce_only": true,
        })
    }
}

fn tif_or_type_to_str(t: OrderType) -> &'static str {
    match t {
        OrderType::Market => "market",
        OrderType::Limit => "limit",
    }
}

fn tif_to_str(t: TimeInForce) -> &'static str {
    match t {
        TimeInForce::Gtc => "gtc",
        TimeInForce::Ioc => "ioc",
        TimeInForce::Poc => "poc",
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Format a price without trailing zeros or scientific notation.
/// Gate accepts decimal strings; matches Python's `str(float)` shape.
fn format_price(p: f64) -> String {
    let s = format!("{}", p);
    s
}

#[derive(Debug, Clone)]
pub struct OrderResponse {
    pub ok: bool,
    pub status: u16,
    pub data: serde_json::Value,
    pub elapsed_ms: f64,
}

impl OrderResponse {
    /// Inner data dict from `data.data` if present, else empty map.
    pub fn inner(&self) -> &serde_json::Value {
        self.data.get("data").unwrap_or(&serde_json::Value::Null)
    }

    pub fn message(&self) -> &str {
        self.data
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    pub fn label(&self) -> &str {
        self.data
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }
}

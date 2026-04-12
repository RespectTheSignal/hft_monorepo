use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Public: Server Time
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerTime {
    pub server_time: String,
}

// ---------------------------------------------------------------------------
// Market: Contract
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Contract {
    pub symbol: String,
    #[serde(default)]
    pub quote_currency: Option<String>,
    #[serde(default)]
    pub init_margin_rate: Option<String>,
    #[serde(default)]
    pub maint_margin_rate: Option<String>,
    #[serde(default)]
    pub max_leverage: Option<String>,
    #[serde(default)]
    pub base_interest_rate: Option<String>,
    #[serde(default)]
    pub funding_rate_cap: Option<String>,
    #[serde(default)]
    pub funding_interval_hours: Option<i32>,
    #[serde(default)]
    pub tick_size: Option<String>,
    #[serde(default)]
    pub unit_order_qty: Option<String>,
    #[serde(default)]
    pub notional_min_order_amount: Option<String>,
    #[serde(default)]
    pub notional_max_order_amount: Option<String>,
    #[serde(default)]
    pub notional_max_position_amount: Option<String>,
    #[serde(default)]
    pub max_open_interest: Option<String>,
}

// ---------------------------------------------------------------------------
// Market: Ticker (BookTicker)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ticker {
    pub symbol: String,
    #[serde(default)]
    pub bid_price: Option<String>,
    #[serde(default)]
    pub ask_price: Option<String>,
    #[serde(default)]
    pub last_price: Option<String>,
    #[serde(default)]
    pub mark_price: Option<String>,
    #[serde(default)]
    pub index_price: Option<String>,
    #[serde(default)]
    pub volume_24h: Option<String>,
    #[serde(default)]
    pub turnover_24h: Option<String>,
    #[serde(default)]
    pub price_change_24h: Option<String>,
    #[serde(default)]
    pub open_interest: Option<String>,
    #[serde(default)]
    pub funding_rate: Option<String>,
    #[serde(default)]
    pub next_funding_time: Option<String>,
    #[serde(default)]
    pub funding_interval_hours: Option<i32>,
    #[serde(default)]
    pub funding_rate_cap: Option<String>,
}

// ---------------------------------------------------------------------------
// WebSocket envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct WsMessage {
    pub topic: String,
    pub ts: String,
    pub data: Vec<WsData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsData {
    pub action_type: String,
    pub rows: Vec<serde_json::Value>,
}

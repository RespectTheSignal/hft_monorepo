use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExchangeName {
    Binance,
    Flipster,
    Bybit,
    Bitget,
    Gate,
}

impl ExchangeName {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Flipster => "flipster",
            Self::Bybit => "bybit",
            Self::Bitget => "bitget",
            Self::Gate => "gate",
        }
    }
    pub fn table(&self) -> &'static str {
        match self {
            Self::Binance => "binance_bookticker",
            Self::Flipster => "flipster_bookticker",
            Self::Bybit => "bybit_bookticker",
            Self::Bitget => "bitget_bookticker",
            Self::Gate => "gate_bookticker",
        }
    }
}

impl fmt::Display for ExchangeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct BookTick {
    pub exchange: ExchangeName,
    pub symbol: String,
    pub bid_price: f64,
    pub ask_price: f64,
    pub bid_size: f64,
    pub ask_size: f64,
    pub last_price: Option<f64>,
    pub mark_price: Option<f64>,
    pub index_price: Option<f64>,
    pub timestamp: DateTime<Utc>,
}

impl BookTick {
    pub fn mid(&self) -> f64 {
        (self.bid_price + self.ask_price) * 0.5
    }
}

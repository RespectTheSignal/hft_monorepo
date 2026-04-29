// Shared market data types for Flipster HFT.
// Modeled after gate_hft_rust/types.rs but simplified for Flipster's ticker+book feed.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BookTickerC {
    pub bid_price: f64,
    pub ask_price: f64,
    pub bid_size: f64,
    pub ask_size: f64,
    /// Exchange server timestamp (ms).
    pub server_time: i64,
}

impl BookTickerC {
    #[inline]
    pub fn mid(&self) -> f64 {
        0.5 * (self.bid_price + self.ask_price)
    }

    #[inline]
    pub fn spread_bp(&self) -> f64 {
        let m = self.mid();
        if m <= 0.0 {
            return 0.0;
        }
        (self.ask_price - self.bid_price) / m * 1.0e4
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TradeC {
    pub id: i64,
    pub price: f64,
    pub size: f64,
    pub create_time_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }

    /// Flipster one-way order API expects "Long" / "Short".
    pub fn flipster_side(&self) -> &'static str {
        match self {
            Side::Buy => "Long",
            Side::Sell => "Short",
        }
    }
}

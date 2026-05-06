//! Bookticker / quote types and the canonical symbol → base mapping.
//!
//! Previously duplicated in:
//! - `collector::model::{BookTick, ExchangeName}`
//! - `collector::strategies::Quote` (with `mid`, `spread_bp`, `buy_at`,
//!   `sell_at`)
//! - three near-identical `base_of(...)` functions in `strategies.rs`,
//!   `spread_revert.rs`, `gate_lead.rs`.

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
    Hyperliquid,
    Mexc,
    Bingx,
    Pancake,
    Aster,
    Lighter,
    Variational,
}

impl ExchangeName {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Flipster => "flipster",
            Self::Bybit => "bybit",
            Self::Bitget => "bitget",
            Self::Gate => "gate",
            Self::Hyperliquid => "hyperliquid",
            Self::Mexc => "mexc",
            Self::Bingx => "bingx",
            Self::Pancake => "pancake",
            Self::Aster => "aster",
            Self::Lighter => "lighter",
            Self::Variational => "variational",
        }
    }
    pub fn table(&self) -> &'static str {
        match self {
            Self::Binance => "binance_bookticker",
            Self::Flipster => "flipster_bookticker",
            Self::Bybit => "bybit_bookticker",
            Self::Bitget => "bitget_bookticker",
            Self::Gate => "gate_bookticker",
            Self::Hyperliquid => "hyperliquid_bookticker",
            Self::Mexc => "mexc_bookticker",
            Self::Bingx => "bingx_bookticker",
            Self::Pancake => "pancake_bookticker",
            Self::Aster => "aster_bookticker",
            Self::Lighter => "lighter_bookticker",
            Self::Variational => "variational_bookticker",
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
    /// Convenience: extract a [`Quote`] view of just the BBO with timestamp.
    pub fn quote(&self) -> Quote {
        Quote { bid: self.bid_price, ask: self.ask_price, ts: self.timestamp }
    }
}

/// Best bid / best ask snapshot at a single instant.
///
/// Mirrors the previous `collector::strategies::Quote` API so call sites can
/// migrate without behavioral change.
#[derive(Debug, Clone)]
pub struct Quote {
    pub bid: f64,
    pub ask: f64,
    pub ts: DateTime<Utc>,
}

impl Quote {
    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) * 0.5
    }
    /// Price at which a buyer would actually fill (crosses the spread up).
    pub fn buy_at(&self) -> f64 {
        self.ask
    }
    /// Price at which a seller would actually fill (crosses the spread down).
    pub fn sell_at(&self) -> f64 {
        self.bid
    }
    /// Relative spread width in bp. Returns 0 for a crossed/empty book.
    pub fn spread_bp(&self) -> f64 {
        let m = self.mid();
        if m <= 0.0 {
            return 0.0;
        }
        ((self.ask - self.bid) / m).max(0.0) * 10_000.0
    }
}

/// Map an exchange symbol to its uppercase USDT base (`BTCUSDT` / `BTC_USDT` /
/// `BTCUSDT.PERP` → `"BTC"`). Returns `None` for non-USDT or unsupported
/// exchanges.
///
/// Replaces three near-identical implementations:
/// - `collector::strategies::base_of` was strictest (only Flipster/Binance/Gate, only USDT).
/// - `collector::spread_revert::base_of` and `collector::gate_lead::base_of`
///   defensively replaced `_` (in case Gate symbols arrive un-normalized) and
///   used `trim_end_matches` (which would over-strip degenerate inputs).
///
/// This canonical version: defensive `_` replace, single-suffix `strip_suffix`,
/// uppercase, and rejects empty results. Currently restricted to the four
/// exchanges any pairs strategy reads (Binance, Flipster, Gate, Bybit, Bitget
/// are all expected to expose USDT-quoted perps with the same shape).
pub fn base_of(exchange: ExchangeName, symbol: &str) -> Option<String> {
    // Hyperliquid: symbol IS the base ("BTC", "PENGU", "kNEIRO" …). No quote
    // suffix to strip.
    if matches!(
        exchange,
        ExchangeName::Hyperliquid | ExchangeName::Lighter | ExchangeName::Variational
    ) {
        // Bare-base symbols ("BTC", "ETH"). Variational subscribes by
        // {underlying:"BTC",...} so we store symbol as the underlying name.
        let s = symbol.trim();
        return if s.is_empty() { None } else { Some(s.to_uppercase()) };
    }
    let cleaned = symbol.replace('_', "");
    let trimmed = match exchange {
        ExchangeName::Flipster => cleaned.trim_end_matches(".PERP").to_string(),
        ExchangeName::Binance
        | ExchangeName::Gate
        | ExchangeName::Bybit
        | ExchangeName::Bitget
        | ExchangeName::Mexc
        | ExchangeName::Pancake
        | ExchangeName::Aster => cleaned,
        // BingX uses "BTC-USDT" with a dash. Strip it before suffix match.
        ExchangeName::Bingx => cleaned.replace('-', ""),
        ExchangeName::Hyperliquid | ExchangeName::Lighter | ExchangeName::Variational => {
            unreachable!()
        }
    };
    trimmed
        .strip_suffix("USDT")
        .filter(|s| !s.is_empty())
        .map(str::to_uppercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_of_flipster() {
        assert_eq!(base_of(ExchangeName::Flipster, "BTCUSDT.PERP").as_deref(), Some("BTC"));
        assert_eq!(base_of(ExchangeName::Flipster, "ETHUSDT.PERP").as_deref(), Some("ETH"));
        // No USDT quote → None.
        assert_eq!(base_of(ExchangeName::Flipster, "USD1.PERP"), None);
    }

    #[test]
    fn base_of_gate_underscored() {
        assert_eq!(base_of(ExchangeName::Gate, "BTC_USDT").as_deref(), Some("BTC"));
        assert_eq!(base_of(ExchangeName::Gate, "BTCUSDT").as_deref(), Some("BTC"));
    }

    #[test]
    fn base_of_binance() {
        assert_eq!(base_of(ExchangeName::Binance, "BTCUSDT").as_deref(), Some("BTC"));
        assert_eq!(base_of(ExchangeName::Binance, "USDT"), None);
    }

    #[test]
    fn quote_helpers() {
        let q = Quote { bid: 100.0, ask: 100.2, ts: Utc::now() };
        assert!((q.mid() - 100.1).abs() < 1e-9);
        assert!((q.spread_bp() - ((0.2 / 100.1) * 10_000.0)).abs() < 1e-6);
        assert_eq!(q.buy_at(), 100.2);
        assert_eq!(q.sell_at(), 100.0);
    }
}

//! HL ↔ Binance pairs mean-reversion (paper).
//!
//! Mirrors Flipster pair strategy but with Hyperliquid as the trading leg
//! and Binance as the leader/hedge reference. 9bp round-trip taker fee.
//!
//! Data flow:
//!   QuestDB hyperliquid_bookticker  ─┐
//!                                    ├─►  paired by base symbol  ─►  EWMA(spread)
//!   QuestDB binance_bookticker      ─┘                              z-score → trade
//!
//! Trade semantics (single-leg paper, HL-only execution):
//!   spread = (HL_mid - BIN_mid) / BIN_mid * 1e4  (in bp)
//!   z      = (spread - ewma_mean) / ewma_std
//!   if z >  entry_sigma  →  HL_short  (HL is rich, expect mean-revert down)
//!   if z < -entry_sigma  →  HL_long   (HL is cheap, expect mean-revert up)
//!   if |z| < exit_sigma  →  close

pub mod config;
pub mod ewma;
pub mod exec;
pub mod ilp;
pub mod live_state;
pub mod reader;
pub mod strategy;

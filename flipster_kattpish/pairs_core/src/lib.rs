//! Shared core types for the Flipster pairs trading stack.
//!
//! Owned types and helpers that previously had 2–3 independent definitions
//! across `collector/src/{strategies,spread_revert,gate_lead}.rs` and
//! `executor/src/zmq_sub.rs`. Centralizing them here keeps the calculation
//! semantics consistent and makes it cheap to add new strategies / executors
//! that should agree on basis-points, mid prices, base symbol normalization,
//! and the wire format of trade signals.
//!
//! Modules:
//! - [`tick`]   — `BookTick`, `Quote`, `ExchangeName`, `base_of`
//! - [`window`] — `RollingWindow` (mean / std with O(1) eviction)
//! - [`position`] — exit-price selection + PnL helpers (single-leg and pair)
//! - [`signal`] — `TradeSignal` wire type for the ZMQ PUB/SUB contract

pub mod tick;
pub mod window;
pub mod position;
pub mod signal;

pub use tick::{base_of, BookTick, ExchangeName, Quote};
pub use window::RollingWindow;
pub use position::{flipster_entry_price, flipster_exit_price, pair_pnl_bp, single_leg_pnl_bp};
pub use signal::{SignalAction, SignalSide, TradeSignal};

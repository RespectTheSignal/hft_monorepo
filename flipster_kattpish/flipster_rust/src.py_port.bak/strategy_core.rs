// Strategy trait — mirrors gate_hft_rust::strategy_core::StrategyCore.
// Decouples signal/decision logic from the execution runner. Concrete strategies
// (e.g. `strategies::StaleMaker`) impl this; the runner holds `Arc<dyn StrategyCore>`.

use crate::data_cache::SymbolSnapshot;
use crate::params::StrategyParams;
use crate::signal::SignalResult;

pub trait StrategyCore: Send + Sync {
    fn name(&self) -> &str;

    fn params(&self) -> &StrategyParams;

    fn calculate_signal(&self, snap: &SymbolSnapshot, now_ms: i64) -> SignalResult;
}

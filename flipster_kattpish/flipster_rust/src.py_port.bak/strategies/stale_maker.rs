// StaleMaker — the Flipster MAKER_STALE cross-book strategy.
// Signal: api cross to web's opposite top-of-book.
// Entry: post_only LIMIT at web's stale price; cancel on convergence.

use crate::data_cache::SymbolSnapshot;
use crate::params::StrategyParams;
use crate::signal::{calculate_signal, SignalResult};
use crate::strategy_core::StrategyCore;

pub struct StaleMaker {
    params: StrategyParams,
}

impl StaleMaker {
    pub fn new(params: StrategyParams) -> Self {
        Self { params }
    }
}

impl StrategyCore for StaleMaker {
    fn name(&self) -> &str {
        "stale_maker"
    }

    fn params(&self) -> &StrategyParams {
        &self.params
    }

    fn calculate_signal(&self, snap: &SymbolSnapshot, now_ms: i64) -> SignalResult {
        calculate_signal(snap, now_ms, &self.params)
    }
}

// V6_0: Same as V6 but run with repeat_profitable_order: false in trade_settings (no repeat order after profitable fill).
use flipster_rust::runner;
use flipster_rust::strategies::v6_0::V6_0StrategyCore;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let leverage = runner::initialize()?;
    let core = Arc::new(V6_0StrategyCore::new(leverage));

    runner::start(core)
}

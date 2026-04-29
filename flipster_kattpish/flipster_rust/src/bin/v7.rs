use flipster_rust::runner;
use flipster_rust::strategies::v7::V7StrategyCore;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let leverage = runner::initialize()?;
    let core = Arc::new(V7StrategyCore::new(leverage));

    runner::start(core)
}

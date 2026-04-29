use flipster_rust::runner;
use flipster_rust::strategies::v6::V6StrategyCore;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let leverage = runner::initialize()?;
    let core = Arc::new(V6StrategyCore::new(leverage));

    runner::start(core)
}

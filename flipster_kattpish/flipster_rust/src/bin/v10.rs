use flipster_rust::runner;
use flipster_rust::strategies::v10::V10StrategyCore;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let leverage = runner::initialize()?;
    let core = Arc::new(V10StrategyCore::new(leverage));

    runner::start(core)
}

use flipster_rust::runner_ws;
use flipster_rust::strategies::v11_10::V11_10StrategyCore;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let leverage = runner_ws::initialize()?;
    let core = Arc::new(V11_10StrategyCore::new(leverage));

    runner_ws::start(core)
}

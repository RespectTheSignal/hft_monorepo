use flipster_rust::runner_ws;
use flipster_rust::strategies::v11::V11StrategyCore;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let leverage = runner_ws::initialize()?;
    let core = Arc::new(V11StrategyCore::new(leverage));

    runner_ws::start(core)
}

use flipster_rust::runner_close;
use flipster_rust::strategies::close_v1::CloseV1StrategyCore;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let leverage = runner_close::initialize()?;
    let core = Arc::new(CloseV1StrategyCore::new(leverage));

    runner_close::start(core)
}

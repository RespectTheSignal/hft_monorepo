use flipster_rust::runner;
use flipster_rust::strategies::v8::V8StrategyCore;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let leverage = runner::initialize()?;
    let core = Arc::new(V8StrategyCore::new(leverage));

    runner::start(core)
}

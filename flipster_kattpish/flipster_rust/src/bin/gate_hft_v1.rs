use flipster_rust::runner;
use flipster_rust::strategies::basic::BasicStrategyCore;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let leverage = runner::initialize()?;
    let core = Arc::new(BasicStrategyCore::new(leverage));
    
    runner::start(core)
}

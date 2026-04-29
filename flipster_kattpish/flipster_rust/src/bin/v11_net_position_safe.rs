use flipster_rust::runner_ws;
use flipster_rust::strategies::v11_net_position_safe::V11NetPositionSafeStrategyCore;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let leverage = runner_ws::initialize()?;
    let core = Arc::new(V11NetPositionSafeStrategyCore::new(leverage));

    runner_ws::start(core)
}

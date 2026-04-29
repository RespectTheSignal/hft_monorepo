// Monitoring binary: GateOrderManager only, no order placement.
// Usage: cargo run --bin monitoring -- --login_names v3_sb_001~v3_sb_010 [--interval 30]

fn main() -> anyhow::Result<()> {
    flipster_rust::runner_monitoring::run()
}

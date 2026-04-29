// Monitoring-only: IPC and gate_order_manager run (WS messages print). No order placement.

use crate::data_client::DataClient;
use crate::gate_order_manager::{
    GateOpenOrder, GateOrderManager, GatePosition, GateRateLimitState,
};
use log::{info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Start IPC (so data flows), then run monitoring loop: log positions, open orders, rate limit per manager.
/// If `print_orders` is false, order details are not logged (summary counts still are).
pub fn run(
    data_client: Arc<dyn DataClient>,
    gate_order_managers: HashMap<String, Arc<GateOrderManager>>,
    interval_secs: u64,
    print_orders: bool,
) -> anyhow::Result<()> {
    let interval = Duration::from_secs(interval_secs);
    info!(
        "[Monitoring] Starting IPC and loop for {} account(s), summary every {}s",
        gate_order_managers.len(),
        interval_secs
    );

    data_client.start()?;
    info!("[Monitoring] Data client started (gate_order_manager WS messages will print as normal)");

    loop {
        std::thread::sleep(interval);

        for (login_name, manager) in &gate_order_managers {
            log_manager_state(login_name, manager, print_orders);
        }
    }
}

fn log_manager_state(login_name: &str, manager: &GateOrderManager, print_orders: bool) {
    let positions = manager.positions.load();
    let open_orders = manager.open_orders.load();
    let rate_limit = manager.gate_rate_limit.load();
    let is_healthy = *manager.is_healthy.read();
    let num_positions = manager.num_positions();

    let positions_nonzero: Vec<&GatePosition> = positions
        .iter()
        .filter(|(_, p)| p.size != 0)
        .map(|(_, p)| p)
        .collect();
    let open_orders_vec: Vec<&GateOpenOrder> = open_orders.values().collect();
    let rl: &GateRateLimitState = &rate_limit;

    info!(
        "[Monitoring] {} | healthy={} positions_count={} open_orders={} rate_limit={}/{}",
        login_name,
        is_healthy,
        num_positions,
        open_orders_vec.len(),
        rl.requests_remain.unwrap_or(-1),
        rl.limit.unwrap_or(-1)
    );

    // for open_order in &open_orders_vec {
    //     let gate_contract = manager.get_contract(&open_order.contract);
    //     let open_order_usdt_size =
    //         manager.get_usdt_amount_from_size(&open_order.contract, open_order.left);

    //     if open_order_usdt_size.abs() > 100.0 {
    //         if print_orders {
    //             info!(
    //                 "[Monitoring] Cancelling order {} {} @ {} size={}",
    //                 login_name, open_order.contract, open_order.price, open_order.size
    //             );
    //         }
    //         if let Err(e) = manager.cancel_order(&open_order.id) {
    //             warn!(
    //                 "[Monitoring] Failed to cancel order {}: {}",
    //                 open_order.id, e
    //             );
    //         }
    //     }
    // }

    if print_orders {
        if !open_orders_vec.is_empty() {
            for o in open_orders_vec.iter().take(20) {
                info!(
                    "[Monitoring] {} order {} {} {} @ {} left={}",
                    login_name, o.id, o.contract, o.size, o.price, o.left
                );
            }
            if open_orders_vec.len() > 20 {
                info!(
                    "[Monitoring] {} ... and {} more open orders",
                    login_name,
                    open_orders_vec.len() - 20
                );
            }
        }
    }
}

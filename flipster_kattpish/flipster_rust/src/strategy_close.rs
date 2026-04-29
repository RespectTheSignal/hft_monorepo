// Main strategy structure and signal loop

use crate::{
    config::StrategyConfig,
    data_cache::DataCache,
    data_client::DataClient,
    gate_order_manager::GateOrderManager,
    order_manager_client::{OrderManagerClient, OrderRequest},
    strategy_core::StrategyCore,
};
use crossbeam_channel::{unbounded as cb_unbounded, Receiver as CbReceiver, Sender as CbSender};
use log::{error, info, warn};
use parking_lot::RwLock;
use rand::Rng;
use rayon::prelude::*;
use std::{cmp::max, collections::HashMap, env};
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const EPS: f64 = 1e-12;

/// Strategy instance
pub struct StrategyClose {
    configs: Arc<RwLock<Arc<Vec<StrategyConfig>>>>,
    all_symbols: Arc<RwLock<Vec<String>>>,
    cache: Arc<DataCache>,
    data_client: Arc<dyn DataClient>,
    order_manager_client: Arc<OrderManagerClient>,
    gate_order_managers: Arc<std::collections::HashMap<String, Arc<GateOrderManager>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
    latest_data_time_ms: Arc<RwLock<i64>>,
    core: Arc<dyn StrategyCore>,
    interval: u64,
    binance_last_book_ticker_latency_ms: u64,
    gate_last_book_ticker_latency_ms: u64,
    restart_interval_secs: u64,
    slippage: f64,
    size_threshold: f64,
    order_divider: u64,
}

impl StrategyClose {
    pub fn new(
        configs: Arc<RwLock<Arc<Vec<StrategyConfig>>>>,
        all_symbols: Arc<RwLock<Vec<String>>>,
        cache: Arc<DataCache>,
        data_client: Arc<dyn DataClient>,
        order_manager_client: Arc<OrderManagerClient>,
        gate_order_managers: std::collections::HashMap<String, Arc<GateOrderManager>>,
        core: Arc<dyn StrategyCore>,
        interval: u64,
        binance_last_book_ticker_latency_ms: u64,
        gate_last_book_ticker_latency_ms: u64,
        restart_interval_secs: u64,
        slippage: f64,
        size_threshold: f64,
        order_divider: u64,
    ) -> Self {
        Self {
            configs,
            all_symbols,
            cache,
            data_client,
            order_manager_client,
            gate_order_managers: Arc::new(gate_order_managers),
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            latest_data_time_ms: Arc::new(RwLock::new(0)),
            core,
            interval,
            binance_last_book_ticker_latency_ms,
            gate_last_book_ticker_latency_ms,
            restart_interval_secs,
            slippage: slippage.clone(),
            size_threshold: size_threshold.clone(),
            order_divider,
        }
    }

    /// Start the strategy (IPC client and signal loop)
    pub fn start(&self) -> anyhow::Result<()> {
        // Start the strategy initially
        self.start_internal()?;

        // Spawn restart manager thread that restarts every 10 minutes
        let running = self.running.clone();
        let data_client = self.data_client.clone();
        let cache = self.cache.clone();
        let configs = self.configs.clone();
        let all_symbols = self.all_symbols.clone();
        let gate_order_managers = self.gate_order_managers.clone();
        let order_manager_client = self.order_manager_client.clone();
        let latest_data_time_ms = self.latest_data_time_ms.clone();
        let core = self.core.clone();
        let interval = self.interval;
        let binance_last_book_ticker_latency_ms = self.binance_last_book_ticker_latency_ms;
        let gate_last_book_ticker_latency_ms = self.gate_last_book_ticker_latency_ms;
        let restart_interval_secs = self.restart_interval_secs;
        let slippage = self.slippage;
        let size_threshold = self.size_threshold.clone();
        let order_divider = self.order_divider;
        std::thread::spawn(move || {
            loop {
                // Wait restart_interval_secs seconds
                std::thread::sleep(Duration::from_secs(restart_interval_secs));

                info!(
                    "[Strategy] Restarting strategy after {} seconds",
                    restart_interval_secs
                );

                // Stop the current strategy
                running.store(false, std::sync::atomic::Ordering::SeqCst);
                data_client.stop();

                // Wait a bit for threads to clean up
                std::thread::sleep(Duration::from_secs(1));

                // Reset latest_data_time_ms
                {
                    let mut latest = latest_data_time_ms.write();
                    *latest = 0;
                }

                // Restart the strategy
                running.store(true, std::sync::atomic::Ordering::SeqCst);

                if let Err(e) = data_client.start() {
                    error!("[Strategy] Failed to restart data client: {}", e);
                    continue;
                }

                info!("[Strategy] IPC client restarted");

                // Create new order queue
                let (order_tx, order_rx): (CbSender<OrderRequest>, CbReceiver<OrderRequest>) =
                    cb_unbounded();

                // Start order worker threads
                let num_workers = std::cmp::min(16, gate_order_managers.len() * 2).max(1);
                let order_manager_client_clone = order_manager_client.clone();
                let gate_order_managers_clone = gate_order_managers.clone();
                let running_clone = running.clone();

                for i in 0..num_workers {
                    let order_rx = order_rx.clone();
                    let order_manager_client = order_manager_client_clone.clone();
                    let gate_order_managers = gate_order_managers_clone.clone();
                    let running = running_clone.clone();
                    std::thread::spawn(move || {
                        Self::order_worker_loop(
                            order_rx,
                            order_manager_client,
                            gate_order_managers,
                            running,
                            i,
                        );
                    });
                }

                // Start signal loop
                let cache = cache.clone();
                let configs = configs.clone();
                let all_symbols = all_symbols.clone();
                let order_tx = order_tx.clone();
                let gate_order_managers = gate_order_managers.clone();
                let running = running.clone();
                let latest_data_time_ms = latest_data_time_ms.clone();
                let core = core.clone();
                let interval = interval;
                let binance_last_book_ticker_latency_ms = binance_last_book_ticker_latency_ms;
                let gate_last_book_ticker_latency_ms = gate_last_book_ticker_latency_ms;
                // let restart_interval_secs = restart_interval_secs;
                let size_threshold = size_threshold.clone();
                let order_divider = order_divider.clone();

                let slippage = slippage.clone();
                std::thread::spawn(move || {
                    Self::signal_loop(
                        cache,
                        configs,
                        all_symbols,
                        order_tx,
                        gate_order_managers,
                        running,
                        latest_data_time_ms,
                        core,
                        interval,
                        binance_last_book_ticker_latency_ms,
                        gate_last_book_ticker_latency_ms,
                        size_threshold,
                        slippage,
                        order_divider,
                    );
                });

                info!("[Strategy] Strategy restarted");
            }
        });

        Ok(())
    }

    /// Internal method to start the strategy (called by start and restart loop)
    fn start_internal(&self) -> anyhow::Result<()> {
        info!("[Strategy] Starting strategy: {}", self.core.name());
        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        // Start IPC client
        self.data_client.start()?;

        info!("[Strategy] Data client started");

        // Create order queue (MPMC, non-blocking sends)
        let (order_tx, order_rx): (CbSender<OrderRequest>, CbReceiver<OrderRequest>) =
            cb_unbounded();

        // Start multiple order worker threads (same as Python ThreadPoolExecutor)
        // Python: max_workers = min(16, len(order_managers) * 2)
        let num_workers = std::cmp::min(16, self.gate_order_managers.len() * 2).max(1);
        let order_manager_client = self.order_manager_client.clone();
        let gate_order_managers = self.gate_order_managers.clone();
        let running = self.running.clone();

        // Clone receiver for each worker (crossbeam receiver is MPMC)
        for i in 0..num_workers {
            let order_rx = order_rx.clone();
            let order_manager_client = self.order_manager_client.clone();
            let gate_order_managers = gate_order_managers.clone();
            let running = running.clone();
            std::thread::spawn(move || {
                Self::order_worker_loop(
                    order_rx,
                    order_manager_client,
                    gate_order_managers,
                    running,
                    i,
                );
            });
        }

        // Start signal loop in separate thread
        let cache = self.cache.clone();
        let configs = self.configs.clone();
        let all_symbols = self.all_symbols.clone();
        let order_tx = order_tx.clone();
        let gate_order_managers = self.gate_order_managers.clone();
        let running = self.running.clone();
        let latest_data_time_ms = self.latest_data_time_ms.clone();
        let core = self.core.clone();
        let interval = self.interval;
        let binance_last_book_ticker_latency_ms = self.binance_last_book_ticker_latency_ms;
        let gate_last_book_ticker_latency_ms = self.gate_last_book_ticker_latency_ms;
        let slippage = self.slippage.clone();
        let size_threshold = self.size_threshold.clone();
        let order_divider = self.order_divider.clone();

        std::thread::spawn(move || {
            Self::signal_loop(
                cache,
                configs,
                all_symbols,
                order_tx,
                gate_order_managers,
                running,
                latest_data_time_ms,
                core,
                interval,
                binance_last_book_ticker_latency_ms,
                gate_last_book_ticker_latency_ms,
                size_threshold,
                slippage,
                order_divider,
            );
        });

        Ok(())
    }

    /// Order worker loop - processes orders from queue (non-blocking, same as Python _order_queue_worker)
    fn order_worker_loop(
        order_rx: CbReceiver<OrderRequest>,
        order_manager_client: Arc<OrderManagerClient>,
        gate_order_managers: Arc<std::collections::HashMap<String, Arc<GateOrderManager>>>,
        running: Arc<std::sync::atomic::AtomicBool>,
        worker_id: usize,
    ) {
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            match order_rx.recv_timeout(Duration::from_millis(5)) {
                Ok(order_request) => {
                    if let Some(gate_order_manager) =
                        gate_order_managers.get(&order_request.login_name)
                    {
                        // TODO: 임의로 주문 들어가게끔 해둔거 나중에 제거해야거
                        if gate_order_manager.is_healthy() == Some(false)
                            && rand::rng().random_bool(0.95)
                        {
                            error!("[OrderWorker {}] Account is not healthy, skipping order for {}: {}", worker_id, order_request.symbol, order_request.login_name);
                            continue;
                        }
                        if let Err(e) = gate_order_manager
                            .place_order(&order_manager_client, order_request.clone())
                        {
                            // if timeout, log as warning
                            if e.to_string().contains("timeout") {
                                warn!("[OrderWorker {}] Timeout sending order for: login_name={}, symbol={}", worker_id, order_request.login_name, order_request.symbol);
                            } else {
                                error!("[OrderWorker {}] Error sending order for: login_name={}, symbol={}, error={}", worker_id, order_request.login_name, order_request.symbol, e);
                            }
                        }
                        gate_order_manager.increment_order_count(&order_request.symbol);
                    } else {
                        // Fallback: no GateOrderManager found, send directly
                        if let Err(e) =
                            order_manager_client.send_order(order_request.clone(), false)
                        {
                            // if timeout, log as warning
                            if e.to_string().contains("timeout") {
                                warn!("[OrderWorker {}] Timeout sending order for: login_name={}, symbol={}", worker_id, order_request.login_name, order_request.symbol);
                            } else {
                                error!("[OrderWorker {}] Error sending order for: login_name={}, symbol={}, error={}", worker_id, order_request.login_name, order_request.symbol, e);
                            }
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Signal loop - main trading logic
    fn signal_loop(
        cache: Arc<DataCache>,
        configs: Arc<RwLock<Arc<Vec<StrategyConfig>>>>,
        all_symbols: Arc<RwLock<Vec<String>>>,
        order_tx: CbSender<OrderRequest>,
        gate_order_managers: Arc<std::collections::HashMap<String, Arc<GateOrderManager>>>,
        running: Arc<std::sync::atomic::AtomicBool>,
        latest_data_time_ms: Arc<RwLock<i64>>,
        core: Arc<dyn StrategyCore>,
        interval: u64,
        binance_last_book_ticker_latency_ms: u64,
        gate_last_book_ticker_latency_ms: u64,
        size_threshold: f64,
        slippage: f64,
        order_divider: u64,
    ) {
        // Silent warmup (same as Python - no print statements)
        // std::thread::sleep(Duration::from_secs(5));

        // Wait for IPC data to be available (same as Python)
        let wait_start = SystemTime::now();
        let mut has_data = false;
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            if wait_start.elapsed().unwrap().as_secs() > 20 {
                break;
            }
            let (_, gate_count) = cache.counts();

            if gate_count > 0 {
                has_data = true;
                info!("✅ IPC data available: {} gate booktickers", gate_count);
                break;
            }

            std::thread::sleep(Duration::from_secs(1));
        }

        if !has_data {
            warn!("\x1b[33m⚠️  Warning: No market data received after warmup period\x1b[0m");
        }

        // std::thread::sleep(Duration::from_secs(5)); // Additional warmup

        // Silent - no "START OF CPU SIGNAL LOOP" message (same as Python)

        let last_warn_time_map: Arc<RwLock<HashMap<String, i64>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let last_order_price_map: Arc<RwLock<HashMap<String, f64>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let warn_no_snapshot_interval_sec: i64 = env::var("WARN_NO_SNAPSHOT_INTERVAL_SEC")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let signal_loop_profile =
            env::var("SIGNAL_LOOP_PROFILE").as_ref().map(|s| s.as_str()) == Ok("1");
        let profile_interval_sec: u64 = env::var("SIGNAL_LOOP_PROFILE_INTERVAL_SEC")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        let (mut last_profile_log, mut n_iters, mut no_data_iters) = (Instant::now(), 0u64, 0u64);
        let (mut sum_total_ms, mut sum_counts_ms, mut sum_snapshots_ms, mut sum_process_ms) =
            (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
        let mut max_total_ms = 0.0_f64;

        // Main loop
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            let (mut t0, mut t1, mut t2, mut t3) = (None, None, None, None);
            if signal_loop_profile {
                t0 = Some(Instant::now());
            }

            // Check if IPC cache has data (one counts() = 2 read() calls)
            let (base_count, gate_count) = cache.counts();

            if base_count == 0 && gate_count == 0 {
                if signal_loop_profile {
                    no_data_iters += 1;
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            if signal_loop_profile {
                t1 = Some(Instant::now());
            }

            let current_time_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;

            let symbols_snapshot = { all_symbols.read().clone() };
            let configs_snapshot = configs.read().clone();
            if signal_loop_profile {
                t2 = Some(Instant::now());
            }

            // Process each symbol in parallel using rayon
            symbols_snapshot.par_iter().for_each(|symbol| {
                Self::process_symbol(
                    symbol.as_str(),
                    current_time_ms,
                    cache.clone(),
                    configs_snapshot.clone(),
                    order_tx.clone(),
                    gate_order_managers.clone(),
                    latest_data_time_ms.clone(),
                    core.clone(),
                    binance_last_book_ticker_latency_ms,
                    gate_last_book_ticker_latency_ms,
                    size_threshold,
                    slippage.clone(),
                    last_order_price_map.clone(),
                    order_divider,
                    last_warn_time_map.clone(),
                    warn_no_snapshot_interval_sec,
                );
            });
            if signal_loop_profile {
                t3 = Some(Instant::now());
            }

            if signal_loop_profile {
                if let (Some(a), Some(b), Some(c), Some(d)) = (t0, t1, t2, t3) {
                    let total_ms = d.duration_since(a).as_secs_f64() * 1000.0;
                    let counts_ms = b.duration_since(a).as_secs_f64() * 1000.0;
                    let snapshots_ms = c.duration_since(b).as_secs_f64() * 1000.0;
                    let process_ms = d.duration_since(c).as_secs_f64() * 1000.0;
                    n_iters += 1;
                    sum_total_ms += total_ms;
                    sum_counts_ms += counts_ms;
                    sum_snapshots_ms += snapshots_ms;
                    sum_process_ms += process_ms;
                    if total_ms > max_total_ms {
                        max_total_ms = total_ms;
                    }
                }
            }

            if signal_loop_profile && last_profile_log.elapsed().as_secs() >= profile_interval_sec {
                if n_iters > 0 {
                    info!(
                        "[SignalLoopClose] last {}s: iters={} no_data={} avg_total={:.2}ms avg_counts={:.2}ms avg_snapshots={:.2}ms avg_process={:.2}ms max_total={:.2}ms",
                        profile_interval_sec,
                        n_iters,
                        no_data_iters,
                        sum_total_ms / n_iters as f64,
                        sum_counts_ms / n_iters as f64,
                        sum_snapshots_ms / n_iters as f64,
                        sum_process_ms / n_iters as f64,
                        max_total_ms
                    );
                }
                last_profile_log = Instant::now();
                n_iters = 0;
                no_data_iters = 0;
                sum_total_ms = 0.0;
                sum_counts_ms = 0.0;
                sum_snapshots_ms = 0.0;
                sum_process_ms = 0.0;
                max_total_ms = 0.0;
            }

            // Sleep for 0.01 seconds (10ms) - same as Python version
            std::thread::sleep(Duration::from_millis(interval));
        }
    }

    /// Process a single symbol (thread-safe, can be called in parallel)
    fn process_symbol(
        symbol: &str,
        current_time_ms: i64,
        cache: Arc<DataCache>,
        configs: Arc<Vec<StrategyConfig>>,
        _order_tx: CbSender<OrderRequest>,
        gate_order_managers: Arc<std::collections::HashMap<String, Arc<GateOrderManager>>>,
        _latest_data_time_ms: Arc<RwLock<i64>>,
        _core: Arc<dyn StrategyCore>,
        _binance_last_book_ticker_latency_ms: u64,
        gate_last_book_ticker_latency_ms: u64,
        size_threshold: f64,
        slippage: f64,
        last_order_price_map: Arc<RwLock<HashMap<String, f64>>>,
        order_divider: u64,
        last_warn_time_map: Arc<RwLock<HashMap<String, i64>>>,
        warn_no_snapshot_interval_sec: i64,
    ) {
        // Get data from cache (one read() for current, previous, received_at)
        let (gate_bt, gate_previous_bt_opt, gate_bt_received_at_ms) =
            match cache.get_gate_bookticker_full(symbol) {
                Some(t) => t,
                None => {
                    if warn_no_snapshot_interval_sec > 0 {
                        let key = format!("{}_no_snapshot", symbol);
                        let mut map = last_warn_time_map.write();
                        let last = map.get(&key).copied().unwrap_or(0);
                        let interval_ms = warn_no_snapshot_interval_sec * 1000;
                        if current_time_ms - last >= interval_ms {
                            map.insert(key, current_time_ms);
                            error!(
                                "\x1b[31m[StrategyClose] No gate bookticker snapshot for {}\x1b[0m",
                                symbol
                            );
                        }
                    }
                    return;
                }
            };

        // Get server times
        let gate_bt_server_time = gate_bt.server_time;

        // Latency check - reject if data is too old (200ms threshold)
        // Print latency log if exceeds threshold (same as Python)

        // if let Some(binance_bt_server_time) = binance_bt_server_time {
        //     if (binance_bt_received_at_ms - binance_bt_server_time)
        //         > (binance_last_book_ticker_latency_ms as i64)
        //     {
        //         if binance_bt_received_at_ms - binance_bt_server_time > 300 {
        //             warn!(
        //                 "\x1b[33mBinance B.T.L {} is too old: {}ms\x1b[0m",
        //                 symbol,
        //                 binance_bt_received_at_ms - binance_bt_server_time
        //             );
        //         }
        //         return;
        //     }
        // }

        // // Update latest data time
        // {
        //     let mut latest = latest_data_time_ms.write();

        //     if let Some(binance_bt_server_time) = binance_bt_server_time {
        //         if binance_bt_server_time > 0 && binance_bt_server_time > *latest {
        //             *latest = binance_bt_server_time;
        //         }
        //     }

        //     let binance_data_lag = current_time_ms - *latest;
        //     if binance_data_lag > 2000 {
        //         warn!("\x1b[31mBinance data lag: {}ms\x1b[0m", binance_data_lag);
        //         return;
        //     }
        // }
        let latest_gate_bt = gate_bt;
        let previous_gate_bt = match gate_previous_bt_opt {
            Some(bt) => bt,
            None => return,
        };

        let spread = latest_gate_bt.ask_price - latest_gate_bt.bid_price;
        let spread_bp = spread / (latest_gate_bt.ask_price + EPS) * 10_000.0;

        let previous_spread = previous_gate_bt.ask_price - previous_gate_bt.bid_price;

        // Process for each config (account) that trades this symbol
        for config in configs.iter() {
            if !config.is_strategy_symbol(symbol) {
                continue;
            }

            // Get GateOrderManager for this login_name
            let gate_order_manager = match gate_order_managers.get(&config.login_name) {
                Some(mgr) => mgr,
                None => {
                    error!(
                        "[Strategy] No GateOrderManager found for {}",
                        config.login_name
                    );
                    continue;
                }
            };
            let mut should_cancel = false;

            if (gate_bt_received_at_ms - gate_bt_server_time)
                > (gate_last_book_ticker_latency_ms as i64)
            {
                let should_warn = {
                    let mut map = last_warn_time_map.write();
                    let last_warn = map.get(symbol).copied().unwrap_or(0);
                    if current_time_ms - last_warn >= 1000 {
                        map.insert(symbol.to_string(), current_time_ms);
                        true
                    } else {
                        false
                    }
                };
                if should_warn {
                    warn!(
                        "\x1b[33mGate B.T.L {} is too old: {}ms\x1b[0m",
                        symbol,
                        gate_bt_received_at_ms - gate_bt_server_time
                    );
                }
                should_cancel = true;
            }

            // Get contract info from GateOrderManager
            // let contract = match gate_order_manager.get_contract(symbol) {
            //     Some(contract) => contract,
            //     None => {
            //         error!(
            //             "[Strategy] Contract not found for {}: {}",
            //             config.login_name, symbol
            //         );
            //         continue;
            //     }
            // };

            let position_size = gate_order_manager.get_position_size(symbol);

            let usdt_position_size = gate_order_manager.get_usdt_position_size(symbol);

            let mut order_side: Option<&str> = None;
            let mut order_price: Option<f64> = None;
            let mut previous_order_price: Option<f64> = None;
            let mut actual_previous_order_price: Option<f64> = None;

            let last_order_price = last_order_price_map.read().get(symbol).cloned();
            if last_order_price.is_some() {
                actual_previous_order_price = Some(last_order_price.unwrap());
            }

            if usdt_position_size.abs() < size_threshold {
                continue;
            }
            if usdt_position_size < 0.0 {
                order_side = Some("buy");

                order_price = Some(latest_gate_bt.bid_price - spread * slippage);
                previous_order_price =
                    Some(previous_gate_bt.bid_price - previous_spread * slippage);

                if actual_previous_order_price.is_some() {
                    let price_change_bp = (order_price.unwrap()
                        - actual_previous_order_price.unwrap())
                        / actual_previous_order_price.unwrap()
                        * 10_000.0;
                    if (actual_previous_order_price.unwrap() - order_price.unwrap()).abs() < EPS {
                        continue;
                    }
                    if order_price.unwrap() > actual_previous_order_price.unwrap()
                        && price_change_bp.abs() < spread_bp.abs() * slippage
                    {
                        continue;
                    }
                    //  else if (order_price.unwrap() - actual_previous_order_price.unwrap())
                    //     / actual_previous_order_price.unwrap()
                    //     < 0.0002
                    // {
                    //     continue;
                    // }
                }

                if previous_order_price.is_none() || order_price.is_none() {
                    continue;
                }

                if (previous_order_price.unwrap() - order_price.unwrap()).abs() < EPS {
                    continue;
                }
                if (previous_gate_bt.bid_price - latest_gate_bt.bid_price).abs() < EPS
                    && previous_order_price < order_price
                {
                    continue;
                }
            } else {
                order_side = Some("sell");
                order_price = Some(latest_gate_bt.ask_price + spread * slippage);
                previous_order_price =
                    Some(previous_gate_bt.ask_price + previous_spread * slippage);
                if actual_previous_order_price.is_some() {
                    if (actual_previous_order_price.unwrap() - order_price.unwrap()).abs() < EPS {
                        continue;
                    }
                    let price_change_bp = (order_price.unwrap()
                        - actual_previous_order_price.unwrap())
                        / actual_previous_order_price.unwrap()
                        * 10_000.0;
                    if order_price.unwrap() < actual_previous_order_price.unwrap()
                        && price_change_bp.abs() < spread_bp.abs() * slippage
                    {
                        continue;
                    }
                    // else if (actual_previous_order_price.unwrap() - order_price.unwrap())
                    //     / actual_previous_order_price.unwrap()
                    //     < 0.0002
                    // {
                    //     continue;
                    // }
                }
                if previous_order_price.is_none() || order_price.is_none() {
                    continue;
                }
                if (previous_order_price.unwrap() - order_price.unwrap()).abs() < EPS {
                    continue;
                }
                if (previous_gate_bt.ask_price - latest_gate_bt.ask_price).abs() < EPS
                    && previous_order_price > order_price
                {
                    continue;
                }
            }
            if should_cancel {
                if let Err(e) =
                    gate_order_manager.cancel_websocket_orders_matched(Some(symbol), None)
                {
                    error!(
                        "[Strategy] Error canceling orders for: login_name={}, symbol={}, error={}",
                        config.login_name, symbol, e
                    );
                }
                if order_price.is_some() && previous_order_price.is_some() {
                    last_order_price_map
                        .write()
                        .insert(symbol.to_string(), order_price.unwrap());
                }

                continue;
            }

            if order_side.is_none() || order_price.is_none() || previous_order_price.is_none() {
                if let Err(e) =
                    gate_order_manager.cancel_websocket_orders_matched(Some(symbol), None)
                {
                    error!(
                        "[Strategy] Error canceling orders for: login_name={}, symbol={}, error={}",
                        config.login_name, symbol, e
                    );
                }
                continue;
            } else {
                // let order_side = order_side.unwrap();
                // let order_price = order_price.unwrap();
                // let previous_order_price = previous_order_price.unwrap();
            }

            if let Err(e) = gate_order_manager.cancel_websocket_orders_matched(Some(symbol), None) {
                error!(
                    "[Strategy] Error canceling orders for: login_name={}, symbol={}, error={}",
                    config.login_name, symbol, e
                );
            }
            let max_order_size = position_size.abs();
            let abs_max_order_size = (max_order_size.abs() / order_divider as i64) as i64;
            let min_order_size = gate_order_manager.get_min_order_size(symbol);

            if abs_max_order_size <= min_order_size {
                continue;
            }
            let order_size = max(abs_max_order_size, min_order_size);

            // let order_price = gate_order_manager.get_valid_order_price(
            //     symbol,
            //     order_price.unwrap(),
            //     Some(order_side.unwrap()),
            // );

            let signed_order_size = if order_side == Some("buy") {
                order_size
            } else {
                -order_size
            };
            if order_price.is_none() {
                continue;
            }
            last_order_price_map
                .write()
                .insert(symbol.to_string(), order_price.unwrap());

            if let Err(e) = gate_order_manager.place_websocket_order(
                symbol,
                order_price.unwrap(),
                signed_order_size,
                true,
                "poc",
                order_side.clone(),
                None,
            ) {
                error!(
                    "[Strategy] Error placing order for: login_name={}, symbol={}, error={}",
                    config.login_name, symbol, e
                );
                if e.to_string().contains("timeout") {
                    warn!(
                        "[Strategy] Timeout placing order for: login_name={}, symbol={}",
                        config.login_name, symbol
                    );
                } else {
                    error!(
                        "[Strategy] Error placing order for: login_name={}, symbol={}, error={}",
                        config.login_name, symbol, e
                    );
                }
            }
        }
    }

    pub fn stop(&self) {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.data_client.stop();
    }
}

// Main strategy structure and signal loop

use crate::{
    config::StrategyConfig,
    data_cache::DataCache,
    data_client::DataClient,
    gate_order_manager::GateOrderManager,
    handle_chance::Chance,
    market_watcher::SharedMarketGapState,
    order_manager_client::{OrderManagerClient, OrderRequest},
    strategy_core::StrategyCore,
    strategy_utils::get_valid_order_price,
};
use crossbeam_channel::{unbounded as cb_unbounded, Receiver as CbReceiver, Sender as CbSender};
use log::{error, info, warn};
use parking_lot::RwLock;
use rand::seq::SliceRandom;
use rand::Rng;
use rayon::prelude::*;
use std::collections::HashMap;
use std::env;
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const EPS: f64 = 1e-12;

/// Strategy instance
pub struct Strategy {
    configs: Arc<RwLock<Arc<Vec<StrategyConfig>>>>,
    all_symbols: Arc<RwLock<Vec<String>>>,
    cache: Arc<DataCache>,
    data_client: Arc<dyn DataClient>,
    order_manager_client: Arc<OrderManagerClient>,
    gate_order_managers: Arc<std::collections::HashMap<String, Arc<GateOrderManager>>>,
    running: Arc<std::sync::atomic::AtomicBool>,
    latest_data_time_ms: Arc<RwLock<i64>>,
    core: Arc<dyn StrategyCore>,
    market_gap_state: SharedMarketGapState,
    interval: u64,
    base_last_book_ticker_latency_ms: u64,
    gate_last_book_ticker_latency_ms: u64,
    restart_interval_secs: u64,
}

impl Strategy {
    pub fn new(
        configs: Arc<RwLock<Arc<Vec<StrategyConfig>>>>,
        all_symbols: Arc<RwLock<Vec<String>>>,
        cache: Arc<DataCache>,
        data_client: Arc<dyn DataClient>,
        order_manager_client: Arc<OrderManagerClient>,
        gate_order_managers: std::collections::HashMap<String, Arc<GateOrderManager>>,
        core: Arc<dyn StrategyCore>,
        market_gap_state: SharedMarketGapState,
        interval: u64,
        base_last_book_ticker_latency_ms: u64,
        gate_last_book_ticker_latency_ms: u64,
        restart_interval_secs: u64,
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
            market_gap_state,
            interval,
            base_last_book_ticker_latency_ms,
            gate_last_book_ticker_latency_ms,
            restart_interval_secs,
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
        let market_gap_state = self.market_gap_state.clone();
        let interval = self.interval;
        let base_last_book_ticker_latency_ms = self.base_last_book_ticker_latency_ms;
        let gate_last_book_ticker_latency_ms = self.gate_last_book_ticker_latency_ms;
        let restart_interval_secs = self.restart_interval_secs;

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
                let market_gap_state = market_gap_state.clone();
                let interval = interval;
                let base_last_book_ticker_latency_ms = base_last_book_ticker_latency_ms;
                let gate_last_book_ticker_latency_ms = gate_last_book_ticker_latency_ms;
                // let restart_interval_secs = restart_interval_secs;

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
                        market_gap_state,
                        interval,
                        base_last_book_ticker_latency_ms,
                        gate_last_book_ticker_latency_ms,
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
        let market_gap_state = self.market_gap_state.clone();
        let interval = self.interval;
        let base_last_book_ticker_latency_ms = self.base_last_book_ticker_latency_ms;
        let gate_last_book_ticker_latency_ms = self.gate_last_book_ticker_latency_ms;

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
                market_gap_state,
                interval,
                base_last_book_ticker_latency_ms,
                gate_last_book_ticker_latency_ms,
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
        market_gap_state: SharedMarketGapState,
        interval: u64,
        base_last_book_ticker_latency_ms: u64,
        gate_last_book_ticker_latency_ms: u64,
    ) {
        // Silent warmup (same as Python - no print statements)
        std::thread::sleep(Duration::from_secs(5));

        // Wait for IPC data to be available (same as Python)
        let wait_start = SystemTime::now();
        let mut has_data = false;
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            if wait_start.elapsed().unwrap().as_secs() > 20 {
                break;
            }

            // Check if we have data in cache (one counts() = 2 read() calls)
            let (base_count, gate_count) = cache.counts();

            if base_count > 0 || gate_count > 0 {
                has_data = true;
                info!(
                    "✅ IPC data available: {} {}, {} gate booktickers",
                    base_count,
                    cache.base_exchange(),
                    gate_count
                );
                break;
            }

            std::thread::sleep(Duration::from_secs(1));
        }

        if !has_data {
            warn!("\x1b[33m⚠️  Warning: No market data received after warmup period\x1b[0m");
        }

        std::thread::sleep(Duration::from_secs(5)); // Additional warmup

        // Silent - no "START OF CPU SIGNAL LOOP" message (same as Python)

        let last_warn_time_map: Arc<RwLock<HashMap<String, i64>>> =
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

            // shuffle symbols
            let symbols_snapshot = { all_symbols.read().clone() };
            let mut shuffled_symbols = symbols_snapshot.clone();
            shuffled_symbols.shuffle(&mut rand::rng());

            let configs_snapshot = configs.read().clone();
            if signal_loop_profile {
                t2 = Some(Instant::now());
            }

            let total_net_positions_usdt_size = gate_order_managers
                .values()
                .map(|m| m.get_net_positions_usdt_size())
                .sum::<f64>();
            if let Some(slot) = gate_order_managers
                .values()
                .find_map(|m| m.total_net_positions_usdt_size_all_accounts.as_ref())
            {
                slot.store(Arc::new(total_net_positions_usdt_size));
            }

            // Process each symbol in parallel using rayon (shuffled order)
            shuffled_symbols.par_iter().for_each(|symbol| {
                Self::process_symbol(
                    symbol.as_str(),
                    current_time_ms,
                    total_net_positions_usdt_size,
                    cache.clone(),
                    configs_snapshot.clone(),
                    order_tx.clone(),
                    gate_order_managers.clone(),
                    latest_data_time_ms.clone(),
                    core.clone(),
                    market_gap_state.clone(),
                    base_last_book_ticker_latency_ms,
                    gate_last_book_ticker_latency_ms,
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
                        "[SignalLoop] last {}s: iters={} no_data={} avg_total={:.2}ms avg_counts={:.2}ms avg_snapshots={:.2}ms avg_process={:.2}ms max_total={:.2}ms",
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
        total_net_positions_usdt_size: f64,
        cache: Arc<DataCache>,
        configs: Arc<Vec<StrategyConfig>>,
        order_tx: CbSender<OrderRequest>,
        gate_order_managers: Arc<std::collections::HashMap<String, Arc<GateOrderManager>>>,
        latest_data_time_ms: Arc<RwLock<i64>>,
        core: Arc<dyn StrategyCore>,
        market_gap_state: SharedMarketGapState,
        base_last_book_ticker_latency_ms: u64,
        gate_last_book_ticker_latency_ms: u64,
        last_warn_time_map: Arc<RwLock<HashMap<String, i64>>>,
        warn_no_snapshot_interval_sec: i64,
    ) {
        // Get data from cache (one snapshot = 3 read() calls instead of 6+)
        let book_snapshot = match cache.get_symbol_bookticker_snapshot(symbol, current_time_ms) {
            Some(s) => s,
            None => {
                if warn_no_snapshot_interval_sec > 0 {
                    let key = format!("{}_no_snapshot", symbol);
                    let mut map = last_warn_time_map.write();
                    let last = map.get(&key).copied().unwrap_or(0);
                    let interval_ms = warn_no_snapshot_interval_sec * 1000;
                    if current_time_ms - last >= interval_ms {
                        map.insert(key, current_time_ms);
                        error!(
                            "\x1b[31m[Strategy] No bookticker snapshot for {} (gate/base web missing)\x1b[0m",
                            symbol
                        );
                    }
                }
                return;
            }
        };

        let gate_bt = book_snapshot.gate_bt;
        let gate_web_bt = book_snapshot.gate_web_bt;
        let base_bt = book_snapshot.base_bt;
        let gate_bt_server_time = gate_bt.server_time;
        let base_bt_server_time = base_bt.as_ref().map(|bt| bt.server_time);
        let gate_web_bt_server_time = gate_web_bt.server_time;
        let gate_bt_received_at_ms = book_snapshot.gate_bt_received_at_ms;
        let base_bt_received_at_ms = book_snapshot.base_bt_received_at_ms;
        let _gate_web_bt_received_at_ms = book_snapshot.gate_web_bt_received_at_ms;

        // Latency check - reject if data is too old (200ms threshold)
        // Print latency log if exceeds threshold (same as Python)
        if (gate_bt_received_at_ms - gate_bt_server_time)
            > (gate_last_book_ticker_latency_ms as i64)
        {
            if gate_bt_received_at_ms - gate_bt_server_time > 200 {
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
            }
            return;
        }
        if let Some(base_bt_server_time) = base_bt_server_time {
            if (base_bt_received_at_ms - base_bt_server_time)
                > (base_last_book_ticker_latency_ms as i64)
            {
                if base_bt_received_at_ms - base_bt_server_time > 300 {
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
                            "\x1b[33mBase({}) B.T.L {} is too old: {}ms\x1b[0m",
                            cache.base_exchange(),
                            symbol,
                            base_bt_received_at_ms - base_bt_server_time
                        );
                    }
                }
                return;
            }
        }

        // Update latest data time
        {
            let mut latest = latest_data_time_ms.write();

            if let Some(base_bt_server_time) = base_bt_server_time {
                if base_bt_server_time > 0 && base_bt_server_time > *latest {
                    *latest = base_bt_server_time;
                }
            }

            let base_data_lag = current_time_ms - *latest;
            // Flipster mode: Binance ZMQ feed from 211.181.122.24 can have 2-5s publish lag;
            // skip the hardcoded 2000ms gate (the per-symbol `base_last_book_ticker_latency_ms`
            // CLI flag already governs per-tick staleness).
            let lag_threshold: i64 = if crate::flipster_cookie_store::global().is_some() {
                30_000
            } else {
                2_000
            };
            if base_data_lag > lag_threshold {
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
                        "\x1b[31mBase({}) data lag: {}ms\x1b[0m",
                        cache.base_exchange(),
                        base_data_lag
                    );
                }
                return;
            }
        }

        cache.push_bookticker_snapshot_history(symbol, current_time_ms, book_snapshot.clone());
        let previous_book_snapshot_1s = cache
            .get_bookticker_snapshot_before_wall_ms(symbol, current_time_ms.saturating_sub(1000));
        let previous_book_snapshot_5s = cache
            .get_bookticker_snapshot_before_wall_ms(symbol, current_time_ms.saturating_sub(5000));
        let previous_book_snapshot_10s = cache
            .get_bookticker_snapshot_before_wall_ms(symbol, current_time_ms.saturating_sub(10_000));
        let previous_book_snapshot_20s = cache
            .get_bookticker_snapshot_before_wall_ms(symbol, current_time_ms.saturating_sub(20_000));

        if previous_book_snapshot_1s.is_none() {
            let key = format!("{}_no_prev_book_snapshot_1s", symbol);
            let mut map = last_warn_time_map.write();
            let last = map.get(&key).copied().unwrap_or(0);
            if current_time_ms - last >= 10_000 {
                map.insert(key, current_time_ms);
                warn!(
                    "[Strategy] No ~1s bookticker snapshot history for {} (previous_snapshot_1s unavailable for decide_order)",
                    symbol
                );
            }
        }
        if previous_book_snapshot_5s.is_none() {
            let key = format!("{}_no_prev_book_snapshot_5s", symbol);
            let mut map = last_warn_time_map.write();
            let last = map.get(&key).copied().unwrap_or(0);
            if current_time_ms - last >= 10_000 {
                map.insert(key, current_time_ms);
                warn!(
                    "[Strategy] No ~5s bookticker snapshot history for {} (previous_snapshot_5s unavailable for decide_order)",
                    symbol
                );
            }
        }
        if previous_book_snapshot_10s.is_none() {
            let key = format!("{}_no_prev_book_snapshot_10s", symbol);
            let mut map = last_warn_time_map.write();
            let last = map.get(&key).copied().unwrap_or(0);
            if current_time_ms - last >= 10_000 {
                map.insert(key, current_time_ms);
                warn!(
                    "[Strategy] No ~10s bookticker snapshot history for {} (previous_snapshot_10s unavailable for decide_order)",
                    symbol
                );
            }
        }
        if previous_book_snapshot_20s.is_none() {
            let key = format!("{}_no_prev_book_snapshot_20s", symbol);
            let mut map = last_warn_time_map.write();
            let last = map.get(&key).copied().unwrap_or(0);
            if current_time_ms - last >= 10_000 {
                map.insert(key, current_time_ms);
                warn!(
                    "[Strategy] No ~20s bookticker snapshot history for {} (previous_snapshot_20s unavailable for decide_order)",
                    symbol
                );
            }
        }

        // Process for each config (account) that trades this symbol
        for config in configs.iter() {
            if !config.is_strategy_symbol(symbol) {
                continue;
            }

            // Get GateOrderManager for this login_name
            let gate_order_manager = match gate_order_managers.get(&config.login_name) {
                Some(mgr) => mgr,
                None => {
                    let should_warn = {
                        let mut map = last_warn_time_map.write();
                        let last_warn = map.get(&config.login_name).copied().unwrap_or(0);
                        if current_time_ms - last_warn >= 1000 {
                            map.insert(config.login_name.clone(), current_time_ms);
                            true
                        } else {
                            false
                        }
                    };
                    if should_warn {
                        error!(
                            "[Strategy] No GateOrderManager found for {}",
                            config.login_name
                        );
                    }
                    // error!(
                    //     "[Strategy] No GateOrderManager found for {}",
                    //     config.login_name
                    // );
                    continue;
                }
            };

            // One snapshot per (account, symbol); on failure skip this opportunity
            let snapshot = match gate_order_manager.try_snapshot_for_symbol(symbol) {
                Some(s) => s,
                None => {
                    let should_warn = {
                        let key = format!("{}_snapshot_fail", symbol, config.login_name);
                        let mut map = last_warn_time_map.write();
                        let last_warn = map.get(&config.login_name).copied().unwrap_or(0);
                        if current_time_ms - last_warn >= 5000 {
                            map.insert(key, current_time_ms);
                            true
                        } else {
                            false
                        }
                    };
                    if should_warn {
                        error!(
                            "[Strategy] Failed to get snapshot for symbol: {}: {}",
                            symbol, config.login_name
                        );
                    }
                    continue;
                }
            };

            let contract = &snapshot.contract;
            let gate_last_trade = cache.get_gate_trade(symbol);
            let gap_st = market_gap_state.load();
            let avg_mid_gap = gap_st.get_avg_gap(symbol);
            let avg_spread = gap_st.get_avg_spread(symbol);
            // Calculate signal using StrategyCore
            let mut signal = core.calculate_signal(
                &book_snapshot,
                contract,
                gate_last_trade,
                avg_mid_gap,
                gap_st.as_ref(),
            );
            signal.apply_market_gap_1m(gap_st.as_ref(), symbol);

            // Flipster-mode signal diagnostics — log every 500th evaluation per symbol.
            if crate::flipster_cookie_store::global().is_some() {
                use std::sync::atomic::{AtomicU64, Ordering as AtomicOrder};
                static CTR: AtomicU64 = AtomicU64::new(0);
                static HAS_SIG: AtomicU64 = AtomicU64::new(0);
                let c = CTR.fetch_add(1, AtomicOrder::Relaxed);
                if signal.has_signal() {
                    HAS_SIG.fetch_add(1, AtomicOrder::Relaxed);
                }
                if c % 2000 == 0 {
                    info!(
                        "[FLIPSTER-DIAG] evaluations={} signals={} (ratio={:.4}) sample_sym={} api_bid={} api_ask={} web_bid={} web_ask={} base_bid={:?} base_ask={:?}",
                        c,
                        HAS_SIG.load(AtomicOrder::Relaxed),
                        HAS_SIG.load(AtomicOrder::Relaxed) as f64 / (c as f64).max(1.0),
                        symbol,
                        book_snapshot.gate_bt.bid_price,
                        book_snapshot.gate_bt.ask_price,
                        book_snapshot.gate_web_bt.bid_price,
                        book_snapshot.gate_web_bt.ask_price,
                        book_snapshot.base_bt.map(|b| b.bid_price),
                        book_snapshot.base_bt.map(|b| b.ask_price),
                    );
                }
            }

            // if !signal.has_signal() {
            //     continue;
            // }

            let quanto_multiplier: f64 = contract.quanto_multiplier.parse().unwrap_or(1.0);
            let funding_rate: f64 = contract.funding_rate.parse().unwrap_or(0.0);

            // Calculate order size and triggers
            let gate_mid = (gate_bt.ask_price + gate_bt.bid_price) * 0.5;

            let contract_order_size =
                ((config.trade_settings.order_size / (gate_mid + EPS)) / quanto_multiplier) as i64;

            let size_trigger = ((config.trade_settings.trade_size_trigger / (gate_mid + EPS))
                / quanto_multiplier) as i64;
            let max_size_trigger_usdt = config
                .trade_settings
                .max_trade_size_trigger
                .unwrap_or(9999999999999.0);
            let max_size_trigger =
                ((max_size_trigger_usdt / (gate_mid + EPS)) / quanto_multiplier) as i64;
            // Python: close_order_size 우선, 없으면 close_trade_size_trigger, 없으면 기본 order_size

            let _close_order_size_usdt = config
                .trade_settings
                .close_order_size
                .or(Some(config.trade_settings.order_size))
                .unwrap_or(config.trade_settings.order_size);

            let close_size_trigger_usdt = config
                .trade_settings
                .close_trade_size_trigger
                .or(Some(config.trade_settings.order_size))
                .unwrap_or(config.trade_settings.order_size);

            let close_size_trigger =
                ((close_size_trigger_usdt / (gate_mid + EPS)) / quanto_multiplier) as i64;

            let order_count_size = snapshot.order_count_size.unwrap_or(0.0);
            let usdt_position_size = snapshot.usdt_position_size;
            let avg_entry_price = snapshot.avg_entry_price;

            // Decide order using StrategyCore
            let decision = core.decide_order(
                symbol,
                &signal,
                &config.trade_settings,
                current_time_ms,
                gate_bt_server_time,
                base_bt_server_time,
                gate_web_bt_server_time,
                funding_rate,
                order_count_size,
                size_trigger,
                max_size_trigger,
                close_size_trigger,
                contract_order_size,
                usdt_position_size,
                avg_entry_price,
                gate_last_trade,
                avg_mid_gap,
                avg_spread,
                total_net_positions_usdt_size,
                previous_book_snapshot_1s.as_ref(),
                previous_book_snapshot_5s.as_ref(),
                previous_book_snapshot_10s.as_ref(),
                previous_book_snapshot_20s.as_ref(),
                gate_order_manager,
                gap_st.as_ref(),
            );

            // Flipster-mode: count how often signals pass through decide_order.
            if crate::flipster_cookie_store::global().is_some() && signal.has_signal() {
                use std::sync::atomic::{AtomicU64, Ordering as AtomicOrder};
                static SIG_THRU: AtomicU64 = AtomicU64::new(0);
                static DEC_THRU: AtomicU64 = AtomicU64::new(0);
                let s = SIG_THRU.fetch_add(1, AtomicOrder::Relaxed);
                if decision.is_some() {
                    DEC_THRU.fetch_add(1, AtomicOrder::Relaxed);
                }
                if s % 200 == 0 {
                    info!(
                        "[FLIPSTER-DIAG] signals_passed_through={} decide_order_returned_some={} (sample side={:?} price={:?} mid_gap_bp={:.3})",
                        s,
                        DEC_THRU.load(AtomicOrder::Relaxed),
                        signal.order_side,
                        signal.order_price,
                        signal.mid_gap_chance_bp,
                    );
                }
            }

            if let Some(decision) = decision {
                if let Some(memo) = decision.memo.as_ref() {
                    let should_log = {
                        let key = format!("{}_memo_info", symbol);
                        let mut map = last_warn_time_map.write();
                        let last = map.get(&key).copied().unwrap_or(0);
                        if current_time_ms - last >= 1000 {
                            map.insert(key, current_time_ms);
                            true
                        } else {
                            false
                        }
                    };
                    if should_log {
                        let memo_color = if memo.contains("close") {
                            "\x1b[1;91m"
                        } else if memo.contains("open") {
                            "\x1b[1;92m"
                        } else {
                            "\x1b[1;95m"
                        };
                        let side_str = signal.order_side.as_deref().unwrap_or("-");
                        let side_color = match side_str {
                            "buy" => "\x1b[1;92m",
                            "sell" => "\x1b[1;91m",
                            _ => "\x1b[1;97m",
                        };
                        info!(
                            "\x1b[1;95m✨📝 [Strategy] memo:\x1b[0m {}{}\x1b[0m \x1b[1;95m| 🪙\x1b[0m \x1b[1;93m{}\x1b[0m \x1b[1;95m| 🎯\x1b[0m {}{}\x1b[0m \x1b[1;95m| 📊 level={}\x1b[0m",
                            memo_color,
                            memo,
                            symbol,
                            side_color,
                            side_str,
                            decision.level,
                        );
                    }
                }
                let (futures_total, futures_unrealised_pnl) = match snapshot.futures_account_data {
                    Some((total, pnl)) => (total, pnl),
                    None => continue,
                };

                // Implement bypass_max_position_size (TODO: from config?)
                let bypass_max_position_size = false;

                let price = if decision.level.contains("market") {
                    0.0
                } else {
                    decision.order_price.unwrap()
                };

                // Create chance (same as Python)
                let chance = Chance {
                    symbol: symbol.to_string(), // Need to clone for Chance struct
                    side: signal.order_side.clone().unwrap(),
                    price,
                    tif: decision.order_tif.clone(),
                    level: decision.level.clone(),
                    size: decision.order_size,
                    bypass_time_restriction: false,
                    bypass_safe_limit_close: config.trade_settings.bypass_safe_limit_close,
                    bypass_max_position_size: bypass_max_position_size,
                };

                let handle_result = match core.clone().handle_chance(
                    &chance.clone(),
                    &gate_order_manager.clone(),
                    &config.trade_settings.clone(),
                    futures_total,
                    futures_unrealised_pnl,
                    total_net_positions_usdt_size,
                ) {
                    Some(res) => res,
                    None => continue,
                };

                let (final_order_size, final_usdt_order_size_abs, final_bypass_safe_limit_close) =
                    handle_result;

                // Apply direction as sign once here for clarity and traceability
                let signed_size = if chance.side == "buy" {
                    final_order_size.abs()
                } else {
                    -final_order_size.abs()
                };

                // Create order request
                let order_request = OrderRequest {
                    symbol: chance.symbol.clone(),
                    side: chance.side.clone(),
                    price: get_valid_order_price(
                        contract,
                        &chance.side,
                        decision.order_price.unwrap(),
                    ),
                    size: signed_size,
                    tif: chance.tif.clone(),
                    level: chance.level.clone(),
                    login_name: config.login_name.clone(),
                    uid: gate_order_manager.get_uid(),
                    if_not_healthy_ws: true,
                    only_reduce_only_for_ws: true,
                    bypass_safe_limit_close: final_bypass_safe_limit_close,
                    usdt_size: final_usdt_order_size_abs,
                };

                info!(
                    "[Strategy] Sending order: {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    order_request.side,
                    order_request.symbol,
                    order_request.price,
                    order_request.usdt_size,
                    order_request.tif,
                    order_request.level,
                    gate_web_bt.ask_price,
                    gate_web_bt.bid_price,
                    total_net_positions_usdt_size
                );

                // Add to queue for parallel processing (non-blocking, same as Python _put_chance_to_queue)
                // This allows data reading/calculation to continue without waiting for order placement
                if let Err(e) = order_tx.send(order_request) {
                    error!("[Strategy] Failed to send order to queue: {}", e);
                }
                // Note: Order count increment is done in order_worker_loop after order is sent
            }
        }
    }

    pub fn stop(&self) {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.data_client.stop();
    }
}

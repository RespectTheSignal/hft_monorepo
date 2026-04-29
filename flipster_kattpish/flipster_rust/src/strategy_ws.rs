// Main strategy structure and signal loop

use crate::data_client::DataClient;
use crate::gate_order_manager::LastOrder;
use crate::{
    config::StrategyConfig,
    data_cache::{DataCache, SymbolBooktickerSnapshot},
    gate_order_manager::GateOrderManager,
    handle_chance::Chance,
    market_watcher::SharedMarketGapState,
    order_manager_client::{OrderManagerClient, OrderRequest},
    strategy_core::StrategyCore,
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
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const EPS: f64 = 1e-12;

static LAST_ORDER_LOG_TIME: AtomicI64 = AtomicI64::new(0);
static LAST_AMEND_LOG_TIME: AtomicI64 = AtomicI64::new(0);
static LAST_NOT_HEALTHY_WARN_MS: AtomicI64 = AtomicI64::new(0);

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
    no_safe_open_order: bool,
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
        no_safe_open_order: bool,
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
            no_safe_open_order,
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
        let no_safe_open_order = self.no_safe_open_order;

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
                let no_safe_open_order = no_safe_open_order;

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
                        no_safe_open_order,
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
            let order_manager_client = order_manager_client.clone();
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
        let no_safe_open_order = self.no_safe_open_order;

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
                no_safe_open_order,
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
        let poll_ms: u64 = env::var("ORDER_WORKER_POLL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5)
            .clamp(1, 100);
        let poll_duration = Duration::from_millis(poll_ms);
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            match order_rx.recv_timeout(poll_duration) {
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
                        // Fallback: no GateOrderManager found, send directly (client resolves URL/grpc by req.login_name)
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
        no_safe_open_order: bool,
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

            if cache.has_data() {
                has_data = true;
                let (base_count, gate_count) = cache.counts();
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

        // Previous snapshot fingerprints per symbol. (fp, same_count, last_process_time_ms): process when fp changed, same_count == 0, or 1s since last process; skip when same_count >= 1 and < 1s since last.
        const FILTER_PASS_INTERVAL_MS: i64 = 200;
        type Fp = (i64, i64, Option<i64>);
        let previous_snapshot_fingerprints: Arc<RwLock<HashMap<String, (Fp, u8, i64)>>> =
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
        if signal_loop_profile {
            info!(
                "[Strategy] Signal loop profiling ON: every {}s log avg_total_ms, avg_counts_ms, avg_snapshots_ms, avg_process_ms (use to find CPU bottleneck)",
                profile_interval_sec
            );
        }

        let (mut last_profile_log, mut n_iters, mut no_data_iters) = (Instant::now(), 0u64, 0u64);
        let (mut sum_total_ms, mut sum_counts_ms, mut sum_snapshots_ms, mut sum_process_ms) =
            (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
        let mut max_total_ms = 0.0_f64;

        // Reused buffer for symbol list to avoid allocating every iteration.
        let mut shuffled_buf = Vec::<String>::with_capacity(512);
        let mut tick_counter: u64 = 0;
        const SHUFFLE_EVERY_N_TICKS: u64 = 10;

        // // cancel all open orders
        // for (login_name, gate_order_manager) in gate_order_managers.iter() {
        //     if let Err(e) = gate_order_manager.cancel_websocket_orders_matched(None, None) {
        //         error!(
        //             "[Strategy] Failed to cancel open orders for {}: {}",
        //             login_name, e
        //         );
        //     }
        //     info!("[Strategy] Cancelled open orders for {}", login_name);
        // }

        // Main loop
        while running.load(std::sync::atomic::Ordering::SeqCst) {
            let (mut t0, mut t1, mut t2, mut t3) = (None, None, None, None);
            if signal_loop_profile {
                t0 = Some(Instant::now());
            }

            // Skip counts() once data has arrived: use one atomic flag set by data receiver.
            if !cache.has_data() {
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

            // Shuffle symbol order every N ticks to spread load; reuse order otherwise to save CPU.
            if tick_counter % SHUFFLE_EVERY_N_TICKS == 0 {
                shuffled_buf.clear();
                shuffled_buf.extend(all_symbols.read().iter().cloned());
                shuffled_buf.shuffle(&mut rand::rng());
            }
            tick_counter = tick_counter.wrapping_add(1);

            let configs_snapshot = configs.read().clone();
            if signal_loop_profile {
                t2 = Some(Instant::now());
            }

            // Phase 1: collect (symbol, snapshot, fp) without holding prev lock (cache reads are per-shard).
            let mut collected: Vec<(String, SymbolBooktickerSnapshot, Fp)> =
                Vec::with_capacity(shuffled_buf.len());
            for symbol in shuffled_buf.iter() {
                let snapshot = match cache.get_symbol_bookticker_snapshot(symbol, current_time_ms) {
                    Some(s) => s,
                    None => {
                        if warn_no_snapshot_interval_sec > 0 {
                            let key = format!("{}_no_snapshot", symbol);
                            let last = last_warn_time_map.read().get(&key).copied().unwrap_or(0);
                            let interval_ms = warn_no_snapshot_interval_sec * 1000;
                            if current_time_ms - last >= interval_ms {
                                last_warn_time_map.write().insert(key, current_time_ms);
                                error!(
                                    "\x1b[31m[Strategy] No bookticker snapshot for {} (gate/base web missing)\x1b[0m",
                                    symbol
                                );
                            }
                        }
                        continue;
                    }
                };
                // Only consider symbols where gate is inside/equal to web and gate spread >= 2x web spread (reduces process_symbol calls).
                let gate_bt = &snapshot.gate_bt;
                let gate_web_bt = &snapshot.gate_web_bt;
                let gate_bt_spread = gate_bt.ask_price - gate_bt.bid_price;
                let gate_web_bt_spread = gate_web_bt.ask_price - gate_web_bt.bid_price;
                let gate_bt_mid = (gate_bt.ask_price + gate_bt.bid_price) / 2.0;

                let gate_bt_latency = snapshot.gate_bt_received_at_ms - gate_bt.server_time;

                if gate_bt_latency > 50 {
                    continue;
                }

                if !(gate_web_bt.ask_price <= gate_bt_mid + EPS
                    || gate_web_bt.bid_price >= gate_bt_mid - EPS)
                {
                    continue;
                }
                // if !(gate_bt.ask_price >= gate_web_bt.ask_price - EPS
                //     && gate_bt.bid_price <= gate_web_bt.bid_price + EPS
                //     && gate_bt_spread >= gate_web_bt_spread * 2.0 - EPS)
                // {
                //     continue;
                // }
                let fp: Fp = (
                    snapshot.gate_bt.server_time,
                    snapshot.gate_web_bt.server_time,
                    snapshot.base_bt.as_ref().map(|b| b.server_time),
                );
                collected.push((symbol.clone(), snapshot, fp));
            }

            // Phase 2: short critical section — update fingerprints and build to_process.
            let to_process: Vec<(String, SymbolBooktickerSnapshot)> = {
                let mut prev = previous_snapshot_fingerprints.write();
                let mut list = Vec::with_capacity(collected.len());
                for (symbol, snapshot, fp) in collected {
                    let (should_skip, new_entry) = match prev.get(&symbol) {
                        Some((prev_fp, same_count, last_ms)) if *prev_fp == fp => {
                            if *same_count >= 1 {
                                if current_time_ms - last_ms >= FILTER_PASS_INTERVAL_MS {
                                    (false, Some((fp, 1, current_time_ms)))
                                } else {
                                    (true, None)
                                }
                            } else {
                                (false, Some((fp, 1, current_time_ms)))
                            }
                        }
                        _ => (false, Some((fp, 0, current_time_ms))),
                    };
                    if should_skip {
                        continue;
                    }
                    if let Some(entry) = new_entry {
                        prev.insert(symbol.clone(), entry);
                    }
                    list.push((symbol, snapshot));
                }
                list
            };

            // Compute once per tick and pass in to avoid per-symbol work.
            let gap_state = market_gap_state.load();
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

            // Process each (symbol, snapshot) in parallel; snapshot is passed in, not fetched inside process_symbol.
            to_process.par_iter().for_each(|(symbol, snapshot)| {
                Self::process_symbol(
                    symbol.as_str(),
                    current_time_ms,
                    snapshot,
                    cache.clone(),
                    configs_snapshot.clone(),
                    order_tx.clone(),
                    gate_order_managers.clone(),
                    latest_data_time_ms.clone(),
                    core.clone(),
                    &*gap_state,
                    total_net_positions_usdt_size,
                    base_last_book_ticker_latency_ms,
                    gate_last_book_ticker_latency_ms,
                    last_warn_time_map.clone(),
                    no_safe_open_order,
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

    /// Process a single symbol (thread-safe, can be called in parallel).
    /// Snapshot is passed in from the loop; caller filters unchanged and fetches once per symbol.
    /// gap_state and total_net_positions_usdt_size are computed once per tick by the caller.
    fn process_symbol(
        symbol: &str,
        current_time_ms: i64,
        snapshot: &SymbolBooktickerSnapshot,
        cache: Arc<DataCache>,
        configs: Arc<Vec<StrategyConfig>>,
        order_tx: CbSender<OrderRequest>,
        gate_order_managers: Arc<std::collections::HashMap<String, Arc<GateOrderManager>>>,
        _latest_data_time_ms: Arc<RwLock<i64>>,
        core: Arc<dyn StrategyCore>,
        gap_state: &crate::market_watcher::MarketGapState,
        total_net_positions_usdt_size: f64,
        _base_last_book_ticker_latency_ms: u64,
        _gate_last_book_ticker_latency_ms: u64,
        _last_warn_time_map: Arc<RwLock<HashMap<String, i64>>>,
        no_safe_open_order: bool,
    ) {
        let gate_bt = snapshot.gate_bt;
        let gate_web_bt = snapshot.gate_web_bt;
        let base_bt = snapshot.base_bt;
        let gate_bt_server_time = gate_bt.server_time;
        let base_bt_server_time = base_bt.as_ref().map(|bt| bt.server_time);
        let gate_web_bt_server_time = gate_web_bt.server_time;
        // let gate_bt_received_at_ms = snapshot.gate_bt_received_at_ms;
        // let base_bt_received_at_ms = snapshot.base_bt_received_at_ms;
        let _gate_web_bt_received_at_ms = snapshot.gate_web_bt_received_at_ms;

        cache.push_bookticker_snapshot_history(symbol, current_time_ms, snapshot.clone());

        // // Latency check - reject if data is too old (200ms threshold)
        // // Print latency log if exceeds threshold (same as Python)
        // if (gate_bt_received_at_ms - gate_bt_server_time)
        //     > (gate_last_book_ticker_latency_ms as i64)
        // {
        //     if gate_bt_received_at_ms - gate_bt_server_time > 200 {
        //         let should_warn = {
        //             let mut map = last_warn_time_map.write();
        //             let last_warn = map.get(symbol).copied().unwrap_or(0);
        //             if current_time_ms - last_warn >= 1000 {
        //                 map.insert(symbol.to_string(), current_time_ms);
        //                 true
        //             } else {
        //                 false
        //             }
        //         };
        //         if should_warn {
        //             warn!(
        //                 "\x1b[33mGate B.T.L {} is too old: {}ms\x1b[0m",
        //                 symbol,
        //                 gate_bt_received_at_ms - gate_bt_server_time
        //             );
        //         }
        //     }
        //     return;
        // }
        // if let Some(base_bt_server_time) = base_bt_server_time {
        //     if (base_bt_received_at_ms - base_bt_server_time)
        //         > (base_last_book_ticker_latency_ms as i64)
        //     {
        //         if base_bt_received_at_ms - base_bt_server_time > 300 {
        //             let should_warn = {
        //                 let mut map = last_warn_time_map.write();
        //                 let last_warn = map.get(symbol).copied().unwrap_or(0);
        //                 if current_time_ms - last_warn >= 1000 {
        //                     map.insert(symbol.to_string(), current_time_ms);
        //                     true
        //                 } else {
        //                     false
        //                 }
        //             };
        //             if should_warn {
        //                 warn!(
        //                     "\x1b[33mBase({}) B.T.L {} is too old: {}ms\x1b[0m",
        //                     cache.base_exchange(),
        //                     symbol,
        //                     base_bt_received_at_ms - base_bt_server_time
        //                 );
        //             }
        //         }
        //         return;
        //     }
        // }

        // // Update latest data time
        // {
        //     let mut latest = latest_data_time_ms.write();

        //     if let Some(base_bt_server_time) = base_bt_server_time {
        //         if base_bt_server_time > 0 && base_bt_server_time > *latest {
        //             *latest = base_bt_server_time;
        //         }
        //     }

        //     let base_data_lag = current_time_ms - *latest;
        //     if base_data_lag > 2000 {
        //         let should_warn = {
        //             let mut map = last_warn_time_map.write();
        //             let last_warn = map.get(symbol).copied().unwrap_or(0);
        //             if current_time_ms - last_warn >= 1000 {
        //                 map.insert(symbol.to_string(), current_time_ms);
        //                 true
        //             } else {
        //                 false
        //             }
        //         };
        //         if should_warn {
        //             warn!(
        //                 "\x1b[31mBase({}) data lag: {}ms\x1b[0m",
        //                 cache.base_exchange(),
        //                 base_data_lag
        //             );
        //         }
        //         return;
        //     }
        // }

        // total_net_positions_usdt_size passed in from signal loop (computed once per tick).

        // Build (config_index, manager) pairs to avoid cloning StrategyConfig; use index into configs
        let config_manager_pairs: Vec<(usize, Arc<GateOrderManager>)> = configs
            .iter()
            .enumerate()
            .filter_map(|(idx, config)| {
                let gate_order_manager = gate_order_managers.get(&config.login_name)?;
                if !config.is_strategy_symbol(symbol)
                    && !gate_order_manager.is_account_symbol(symbol)
                {
                    return None;
                }
                Some((idx, gate_order_manager.clone()))
            })
            .collect();

        let previous_book_snapshot_1s = cache
            .get_bookticker_snapshot_before_wall_ms(symbol, current_time_ms.saturating_sub(1000));
        let previous_book_snapshot_5s = cache
            .get_bookticker_snapshot_before_wall_ms(symbol, current_time_ms.saturating_sub(5000));
        let previous_book_snapshot_10s = cache
            .get_bookticker_snapshot_before_wall_ms(symbol, current_time_ms.saturating_sub(10_000));
        let previous_book_snapshot_20s = cache
            .get_bookticker_snapshot_before_wall_ms(symbol, current_time_ms.saturating_sub(20_000));

        let configs_ref = configs.clone();
        let run_one = |(idx, gate_order_manager): &(usize, Arc<GateOrderManager>)| {
            let config = &configs_ref[*idx];
            // One snapshot per (account, symbol); on failure skip this opportunity
            let account_snapshot = match gate_order_manager.try_snapshot_for_symbol(symbol) {
                Some(s) => s,
                None => {
                    let should_warn = {
                        let key = format!("{}_{}_snapshot_fail", symbol, config.login_name);
                        let mut map = _last_warn_time_map.write();
                        let last = map.get(&key).copied().unwrap_or(0);
                        if current_time_ms - last >= 5000 {
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
                    return;
                }
            };

            let last_ws_order =
                if let Some(last_ws_orders) = gate_order_manager.last_ws_orders.try_read() {
                    last_ws_orders.get(symbol).cloned()
                } else {
                    return;
                };

            let contract = &account_snapshot.contract;
            let min_order_size = contract.order_size_min as i64;
            let gate_last_trade = cache.get_gate_trade(symbol);
            let avg_mid_gap = gap_state.get_avg_gap(symbol); // calculated avg(base_mid - quote_mid) / base_mid
            let avg_spread = gap_state.get_avg_spread(symbol);

            let open_orders = gate_order_manager.get_open_orders_for_contract(symbol);
            let usdt_position_size = account_snapshot.usdt_position_size;

            // let mut has_buy_order = false;
            // let mut has_sell_order = false;

            // let order_price_round = contract.order_price_round.parse().unwrap_or(0.0);
            let gate_mid = (gate_bt.ask_price + gate_bt.bid_price) * 0.5;

            // let max_buy_price = gate_web_bt.bid_price;
            // let min_buy_price = gate_bt.bid_price + order_price_round;
            // let min_sell_price = gate_web_bt.ask_price;
            // let max_sell_price = gate_bt.ask_price - order_price_round;

            // let spread = gate_bt.ask_price - gate_bt.bid_price;

            // let (buy_price_round_multiplier, buy_price_spread_multiplier) =
            //     if usdt_position_size < -config.trade_settings.order_size {
            //         (
            //             config.trade_settings.ws_order_price_round_multiplier,
            //             config.trade_settings.ws_order_spread_multiplier,
            //         )
            //     } else {
            //         (
            //             config.trade_settings.ws_order_price_round_multiplier * 2.0,
            //             config.trade_settings.ws_order_spread_multiplier * 2.0,
            //         )
            //     };
            // let (sell_price_round_multiplier, sell_price_spread_multiplier) =
            //     if usdt_position_size > config.trade_settings.order_size {
            //         (
            //             config.trade_settings.ws_order_price_round_multiplier,
            //             config.trade_settings.ws_order_spread_multiplier,
            //         )
            //     } else {
            //         (
            //             config.trade_settings.ws_order_price_round_multiplier * 2.0,
            //             config.trade_settings.ws_order_spread_multiplier * 2.0,
            //         )
            //     };

            // let buy_price = (gate_bt.bid_price - order_price_round * buy_price_round_multiplier)
            //     .min(max_buy_price)
            //     .min(gate_bt.bid_price - spread * buy_price_spread_multiplier)
            //     .max(gate_bt.bid_price / 2.0);

            // let sell_price = (gate_bt.ask_price + order_price_round * sell_price_round_multiplier)
            //     .max(min_sell_price)
            //     .max(gate_bt.ask_price + spread * sell_price_spread_multiplier)
            //     .max(gate_bt.ask_price / 2.0);

            // let spread_bp = (gate_bt.ask_price - gate_bt.bid_price) / (gate_mid + EPS) * 10_000.0;

            if open_orders.len() > 0 {
                // if repeat profitable order is enabled, don't change order size
                for order in open_orders {
                    // if let Some(last_ws_order) = last_ws_order.clone() {
                    //     if current_time_ms - last_ws_order.timestamp > 1000  {
                    //         let (size, side) = if order.size > 0 {
                    //             (min_order_size, "buy")
                    //         } else {
                    //             (-min_order_size, "sell")
                    //         };
                    //         if let Err(e) = gate_order_manager.amend_websocket_order(
                    //             order.id.as_str(),
                    //             order.price,
                    //             Some(size),
                    //             Some(symbol),
                    //             None,
                    //             Some(side),
                    //             None,
                    //             None,
                    //         ) {
                    //             error!("[Strategy] Failed to amend order {}: {}", order.id, e);
                    //         }
                    //         gate_order_manager.on_new_order_ws(
                    //             symbol.to_string(),
                    //             LastOrder {
                    //                 level: "limit_open".to_string(),
                    //                 side: side.to_string(),
                    //                 price: order.price,
                    //                 timestamp: current_time_ms,
                    //                 contract: symbol.to_string(),
                    //             },
                    //         );
                    //     }
                    // }
                    if order.size > 0 {
                        // has_buy_order = true;
                        if order.price >= gate_mid * 0.8 {
                            // // log every 5 seconds
                            // if current_time_ms - LAST_AMEND_LOG_TIME.load(Ordering::Relaxed)
                            //     >= 60000
                            // {
                            //     LAST_AMEND_LOG_TIME.store(current_time_ms, Ordering::Relaxed);
                            //     info!(
                            //         "[Strategy] Amending order {}: size={} price={} -> {}",
                            //         contract.name, order.size, order.price, buy_price
                            //     );
                            // }

                            if let Err(e) = gate_order_manager.amend_websocket_order(
                                order.id.as_str(),
                                gate_mid * 0.5,
                                None,
                                Some(symbol),
                                None,
                                Some("buy"),
                                None,
                                None,
                            ) {
                                error!("[Strategy] Failed to amend order {}: {}", order.id, e);
                            }
                        }
                    } else if order.size < 0 {
                        // has_sell_order = true;
                        if order.price <= gate_mid * 1.2 {
                            // info!(
                            //     "[Strategy] Amending order {}: size={} price={} -> {}",
                            //     contract.name, order.size, order.price, sell_price
                            // );
                            if let Err(e) = gate_order_manager.amend_websocket_order(
                                order.id.as_str(),
                                gate_mid * 1.5,
                                None,
                                Some(symbol),
                                None,
                                Some("sell"),
                                None,
                                None,
                            ) {
                                error!("[Strategy] Failed to amend order {}: {}", order.id, e);
                            }
                        }
                    }
                }
            }
            // } else if spread_bp > config.trade_settings.spread_bp_threshold - EPS {
            //     // Place orders when no open orders (can disable via WS_PLACE_WHEN_NO_OPEN_ORDERS=false)
            //     let place_when_no_open = env::var("WS_PLACE_WHEN_NO_OPEN_ORDERS")
            //         .unwrap_or_else(|_| "false".to_string());
            //     let place_when_no_open = matches!(
            //         place_when_no_open.trim().to_lowercase().as_str(),
            //         "true" | "1" | "yes" | "on"
            //     );
            //     if place_when_no_open
            //         && current_time_ms - LAST_ORDER_LOG_TIME.load(Ordering::Relaxed) >= 1000
            //     {
            //         LAST_ORDER_LOG_TIME.store(current_time_ms, Ordering::Relaxed);
            //         info!(
            //             "\x1b[33m[Strategy] No open orders for {}: {}\x1b[0m",
            //             symbol, config.login_name
            //         );
            //         let usdt_position_size = gate_order_manager.get_usdt_position_size(symbol);

            //         let order_size_contract = min_order_size;

            //         let buy_order_request = OrderRequest {
            //             symbol: symbol.to_string(),
            //             side: "buy".to_string(),
            //             price: buy_price - order_price_round * 100.0,
            //             size: order_size_contract,
            //             tif: "gtc".to_string(),
            //             level: "limit_open".to_string(),
            //             login_name: config.login_name.clone(),
            //             uid: gate_order_manager.get_uid(),
            //             if_not_healthy_ws: true,
            //             only_reduce_only_for_ws: true,
            //             bypass_safe_limit_close: false,
            //             usdt_size: usdt_position_size.abs(),
            //         };
            //         let sell_order_request = OrderRequest {
            //             symbol: symbol.to_string(),
            //             side: "sell".to_string(),
            //             price: sell_price + order_price_round * 100.0,
            //             size: -order_size_contract,
            //             tif: "gtc".to_string(),
            //             level: "limit_open".to_string(),
            //             login_name: config.login_name.clone(),
            //             uid: gate_order_manager.get_uid(),
            //             if_not_healthy_ws: true,
            //             only_reduce_only_for_ws: true,
            //             bypass_safe_limit_close: false,
            //             usdt_size: usdt_position_size.abs(),
            //         };
            //         if usdt_position_size > config.trade_settings.order_size {
            //             let sell_order_request = OrderRequest {
            //                 symbol: symbol.to_string(),
            //                 side: "sell".to_string(),
            //                 price: sell_price + order_price_round * 100.0,
            //                 size: -order_size_contract,
            //                 tif: "gtc".to_string(),
            //                 level: "limit_close".to_string(),
            //                 login_name: config.login_name.clone(),
            //                 uid: gate_order_manager.get_uid(),
            //                 if_not_healthy_ws: true,
            //                 only_reduce_only_for_ws: true,
            //                 bypass_safe_limit_close: false,
            //                 usdt_size: usdt_position_size.abs(),
            //             };
            //             if let Err(e) = order_tx.send(sell_order_request) {
            //                 error!("[Strategy] Failed to send order to queue: {}", e);
            //             }
            //         } else if usdt_position_size < -config.trade_settings.order_size {
            //             let buy_order_request = OrderRequest {
            //                 symbol: symbol.to_string(),
            //                 side: "buy".to_string(),
            //                 price: buy_price - order_price_round * 100.0,
            //                 size: order_size_contract,
            //                 tif: "gtc".to_string(),
            //                 level: "limit_close".to_string(),
            //                 login_name: config.login_name.clone(),
            //                 uid: gate_order_manager.get_uid(),
            //                 if_not_healthy_ws: true,
            //                 only_reduce_only_for_ws: true,
            //                 bypass_safe_limit_close: false,
            //                 usdt_size: usdt_position_size.abs(),
            //             };
            //             if let Err(e) = order_tx.send(buy_order_request) {
            //                 error!("[Strategy] Failed to send order to queue: {}", e);
            //             }
            //         } else if !gate_order_manager.is_limit_open_blocked(symbol) {
            //             // no position send both buy and sell order
            //             if let Err(e) = order_tx.send(buy_order_request) {
            //                 error!("[Strategy] Failed to send order to queue: {}", e);
            //             }
            //             if let Err(e) = order_tx.send(sell_order_request) {
            //                 error!("[Strategy] Failed to send order to queue: {}", e);
            //             }
            //         }
            //     }
            // }

            // if has_buy_order && has_sell_order {
            //     continue;
            // }

            // Calculate signal using StrategyCore
            let mut signal =
                core.calculate_signal(&snapshot, contract, gate_last_trade, avg_mid_gap, gap_state);
            signal.apply_market_gap_1m(gap_state, symbol);

            if !signal.has_signal() {
                return;
            }

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

            let order_count_size = account_snapshot.order_count_size.unwrap_or(0.0);
            let avg_entry_price = account_snapshot.avg_entry_price;

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
                gap_state,
            );

            if let Some(decision) = decision {
                if let Some(memo) = decision.memo.as_ref() {
                    let should_log = {
                        let key = format!("{}_memo_info", symbol);
                        let mut map = _last_warn_time_map.write();
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
                let (futures_total, futures_unrealised_pnl) =
                    match account_snapshot.futures_account_data {
                        Some((total, pnl)) => (total, pnl),
                        None => return,
                    };

                // Implement bypass_max_position_size (TODO: from config?)
                let bypass_max_position_size = false;

                let price = if decision.level.contains("market") {
                    0.0
                } else {
                    decision.order_price.unwrap_or(signal.order_price.unwrap())
                };

                let level = if usdt_position_size < 0.0
                    && signal.order_side.clone().unwrap() == "buy"
                {
                    "limit_close".to_string()
                } else if usdt_position_size > 0.0 && signal.order_side.clone().unwrap() == "sell" {
                    "limit_close".to_string()
                } else {
                    decision.level.clone()
                };

                // Create chance (same as Python)
                let chance = Chance {
                    symbol: symbol.to_string(), // Need to clone for Chance struct
                    side: signal.order_side.clone().unwrap(),
                    price,
                    tif: decision.order_tif.clone(),
                    level: level,
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
                    None => return,
                };

                let (final_order_size, final_usdt_order_size_abs, final_bypass_safe_limit_close) =
                    handle_result;

                // Apply direction as sign once here for clarity and traceability
                let signed_size = if chance.side == "buy" {
                    final_order_size.abs()
                } else {
                    -final_order_size.abs()
                };

                let side_emoji = if chance.side == "buy" { "🔷" } else { "🔶" };
                let usdt_size_sign = if signed_size > 0 { "+" } else { "-" };
                let usdt_size_str = format!("{}{:.2}", usdt_size_sign, final_usdt_order_size_abs);

                info!(
                    "\x1b[92m[Strategy] Sending order: {} | {} | {} | {} | {} | {}\x1b[0m",
                    side_emoji,
                    chance.symbol,
                    chance.price,
                    usdt_size_str,
                    chance.level,
                    config.login_name
                );
                let position_size = gate_order_manager.get_position_size(symbol);

                let (expiration_time_ms, is_close_order) =
                    if position_size < 0 && chance.side == "buy" {
                        (
                            current_time_ms + config.trade_settings.close_expire_time_ms,
                            true,
                        )
                    } else if position_size > 0 && chance.side == "sell" {
                        (
                            current_time_ms + config.trade_settings.close_expire_time_ms,
                            true,
                        )
                    } else {
                        (
                            current_time_ms + config.trade_settings.expire_time_ms,
                            false,
                        )
                    };

                let open_orders = gate_order_manager.get_open_orders_for_contract(symbol);
                let mut has_matching_order = false;

                for order in open_orders {
                    // if has_matching_order {
                    //     if let Err(e) = gate_order_manager.cancel_websocket_orders_matched(
                    //         Some(order.contract.as_str()),
                    //         Some(chance.side.as_str()),
                    //     ) {
                    //         error!("[Strategy] Failed to cancel order: {}", e);
                    //     }
                    //     break;
                    // }
                    if chance.side == "buy" {
                        if order.size > 0 {
                            if let Err(e) = gate_order_manager.amend_websocket_order(
                                &order.id,
                                chance.price,
                                Some(signed_size),
                                Some(symbol),
                                None,
                                Some("buy"),
                                None,
                                Some(expiration_time_ms),
                            ) {
                                error!("[Strategy] Failed to amend buy order: {}", e);
                            }
                            has_matching_order = true;

                            // sleep 1 millisecond
                            std::thread::sleep(Duration::from_micros(100));

                            if let Err(e) = gate_order_manager.amend_websocket_order(
                                &order.id,
                                gate_mid * 0.5,
                                None,
                                Some(symbol),
                                None,
                                Some("buy"),
                                None,
                                None,
                            ) {
                                error!("[Strategy] Failed to amend buy order: {}", e);
                            }

                            if !is_close_order {
                                std::thread::sleep(Duration::from_micros(100));
                                if let Err(e) = gate_order_manager.amend_websocket_order(
                                    &order.id,
                                    gate_mid * 0.4,
                                    None,
                                    Some(symbol),
                                    None,
                                    Some("buy"),
                                    None,
                                    None,
                                ) {
                                    error!("[Strategy] Failed to amend buy order: {}", e);
                                }
                            }
                            break;
                        }
                    } else if chance.side == "sell" {
                        if order.size < 0 {
                            if let Err(e) = gate_order_manager.amend_websocket_order(
                                &order.id,
                                chance.price,
                                Some(signed_size),
                                Some(symbol),
                                None,
                                Some("sell"),
                                None,
                                Some(expiration_time_ms),
                            ) {
                                error!("[Strategy] Failed to amend sell order: {}", e);
                            }

                            has_matching_order = true;

                            // sleep 1 millisecond
                            std::thread::sleep(Duration::from_micros(100));

                            if let Err(e) = gate_order_manager.amend_websocket_order(
                                &order.id,
                                gate_mid * 1.3,
                                None,
                                Some(symbol),
                                None,
                                Some("sell"),
                                None,
                                None,
                            ) {
                                error!("[Strategy] Failed to amend sell order: {}", e);
                            }

                            if !is_close_order {
                                // sleep 1 millisecond
                                std::thread::sleep(Duration::from_micros(100));

                                if let Err(e) = gate_order_manager.amend_websocket_order(
                                    &order.id,
                                    gate_mid * 1.4,
                                    None,
                                    Some(symbol),
                                    None,
                                    Some("sell"),
                                    None,
                                    None,
                                ) {
                                    error!("[Strategy] Failed to amend sell order: {}", e);
                                }
                            }
                            break;
                        }
                    }
                }

                if has_matching_order {
                    return;
                }

                if gate_order_manager.is_healthy() == Some(false) {
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64;
                    if now_ms - LAST_NOT_HEALTHY_WARN_MS.load(Ordering::Relaxed) >= 1000 {
                        LAST_NOT_HEALTHY_WARN_MS.store(now_ms, Ordering::Relaxed);
                        warn!(
                            "\x1b[33m[Strategy] 🚨 Account is not healthy: {}\x1b[0m",
                            gate_order_manager.login_name
                        );
                    }
                    return;
                }

                gate_order_manager.on_new_order(
                    symbol,
                    LastOrder {
                        level: chance.level.clone(),
                        side: chance.side.clone(),
                        price: chance.price,
                        timestamp: current_time_ms,
                        contract: symbol.to_string(),
                    },
                );

                let price = if chance.level.contains("market") {
                    "0.0".to_string()
                } else {
                    if chance.side == "buy" {
                        let price = if no_safe_open_order {
                            chance.price
                        } else {
                            chance.price * 0.95
                        };
                        gate_order_manager.get_valid_order_price(
                            chance.symbol.as_str(),
                            price,
                            Some("buy"),
                        )
                    } else {
                        let price = if no_safe_open_order {
                            chance.price
                        } else {
                            chance.price * 1.05
                        };
                        gate_order_manager.get_valid_order_price(
                            chance.symbol.as_str(),
                            price,
                            Some("sell"),
                        )
                    }
                };

                // Create order request
                let order_request = OrderRequest {
                    symbol: chance.symbol.clone(),
                    side: chance.side.clone(),
                    price: price,
                    size: signed_size,
                    tif: "gtc".to_string(),
                    level: chance.level.clone(),
                    login_name: config.login_name.clone(),
                    uid: gate_order_manager.get_uid(),
                    if_not_healthy_ws: true,
                    only_reduce_only_for_ws: true,
                    bypass_safe_limit_close: final_bypass_safe_limit_close,
                    usdt_size: final_usdt_order_size_abs,
                };

                // Add to queue for parallel processing (non-blocking, same as Python _put_chance_to_queue)
                // This allows data reading/calculation to continue without waiting for order placement
                if let Err(e) = order_tx.send(order_request) {
                    error!("[Strategy] Failed to send order to queue: {}", e);
                }
                // Note: Order count increment is done in order_worker_loop after order is sent
            }
        };

        if config_manager_pairs.len() <= 2 {
            for pair in &config_manager_pairs {
                run_one(pair);
            }
        } else {
            config_manager_pairs.par_iter().for_each(&run_one);
        }
    }

    pub fn stop(&self) {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.data_client.stop();
    }
}

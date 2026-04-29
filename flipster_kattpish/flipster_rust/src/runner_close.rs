use crate::{
    account_manager, config, data_cache, data_client::DataClient, gate_order_manager, ipc_client,
    order_manager_client, strategy_close, strategy_core, zmq_sub_client,
};
use anyhow::Context;
use log::{error, info, trace, warn};
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;

pub fn initialize() -> anyhow::Result<Option<f64>> {
    dotenvy::dotenv().ok();
    crate::log::init_logging();
    info!("[GateHFT Rust] Starting");

    // Parse CLI arguments
    let args: Vec<String> = std::env::args().collect();
    let mut leverage: Option<f64> = None;

    let mut i = 1;
    while i < args.len() {
        if args[i] == "--leverage" && i + 1 < args.len() {
            leverage = args[i + 1].parse().ok();
            i += 2;
        } else {
            i += 1;
        }
    }

    // Also check environment variable
    if leverage.is_none() {
        if let Ok(env_leverage) = std::env::var("LEVERAGE") {
            leverage = env_leverage.parse().ok();
        }
    }

    Ok(leverage)
}

fn parse_args() -> anyhow::Result<(Vec<String>, u64, u64, u64, u64, f64, f64, u64, String)> {
    let args: Vec<String> = std::env::args().collect();
    let mut spec: Option<String> = None;
    let mut interval: u64 = 10;
    let mut base_last_book_ticker_latency_ms: u64 = 200; // 200ms
    let mut gate_last_book_ticker_latency_ms: u64 = 500; // 500ms
    let mut restart_interval_secs: u64 = 300; // 300s
    let mut slippage: f64 = 2.0; // 2%
    let mut size_threshold: f64 = 300.0; // 300 USDT
    let mut order_divider: u64 = 3; // 3
    let mut base_exchange: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--login_names" && i + 1 < args.len() {
            spec = Some(args[i + 1].clone());
            i += 1;
        }
        if args[i] == "--interval" && i + 1 < args.len() {
            interval = args[i + 1].parse::<u64>().unwrap_or(10);
            i += 1;
        }
        if args[i] == "--binance-latency" && i + 1 < args.len() {
            base_last_book_ticker_latency_ms = args[i + 1].parse::<u64>().unwrap_or(200);
            i += 1;
        }
        if args[i] == "--base-latency" && i + 1 < args.len() {
            base_last_book_ticker_latency_ms = args[i + 1].parse::<u64>().unwrap_or(200);
            i += 1;
        }
        if args[i] == "--gate-latency" && i + 1 < args.len() {
            gate_last_book_ticker_latency_ms = args[i + 1].parse::<u64>().unwrap_or(500);
            i += 1;
        }
        if args[i] == "--restart-interval" && i + 1 < args.len() {
            restart_interval_secs = args[i + 1].parse::<u64>().unwrap_or(300);
            i += 1;
        }
        if args[i] == "--slippage" && i + 1 < args.len() {
            slippage = args[i + 1].parse::<f64>().unwrap_or(10.0);
            i += 1;
        }
        if args[i] == "--size-threshold" && i + 1 < args.len() {
            size_threshold = args[i + 1].parse::<f64>().unwrap_or(300.0);
            i += 1;
        }
        if args[i] == "--order-divider" && i + 1 < args.len() {
            order_divider = args[i + 1].parse::<u64>().unwrap_or(3);
            i += 1;
        }
        if args[i] == "--base-exchange" && i + 1 < args.len() {
            base_exchange = Some(args[i + 1].clone());
            i += 1;
        }
        i += 1;
    }

    if spec.is_none() {
        if let Ok(env_spec) = std::env::var("LOGIN_NAMES") {
            if !env_spec.trim().is_empty() {
                spec = Some(env_spec);
            }
        }
    }
    if base_exchange.is_none() {
        if let Ok(env_base_exchange) = std::env::var("BASE_EXCHANGE") {
            if !env_base_exchange.trim().is_empty() {
                base_exchange = Some(env_base_exchange);
            }
        }
    }

    let spec = spec.ok_or_else(|| {
        anyhow::anyhow!(
            "No login names provided. Use --login_names v3_sb_001~v3_sb_010 or set LOGIN_NAMES env."
        )
    })?;
    let base_exchange = base_exchange.unwrap_or_else(|| "binance".to_string());
    Ok((
        expand_login_spec(&spec),
        interval,
        base_last_book_ticker_latency_ms,
        gate_last_book_ticker_latency_ms,
        restart_interval_secs,
        slippage,
        size_threshold,
        order_divider,
        base_exchange,
    ))
}

fn expand_login_spec(spec: &str) -> Vec<String> {
    // Supports:
    // - "v3_sb_001~v3_sb_010"
    // - "v3_sb_001~v3_sb_010,v3_sb_020"
    // - "v3_sb_001,v3_sb_002"
    let mut result = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('~') {
            result.extend(expand_range(start.trim(), end.trim()));
        } else {
            result.push(part.to_string());
        }
    }
    result
}

fn expand_range(start: &str, end: &str) -> Vec<String> {
    // Find common prefix and numeric suffix, like:
    // "v3_sb_001" -> prefix "v3_sb_", suffix "001"
    fn split_numeric_suffix(s: &str) -> (&str, &str) {
        let mut idx = s.len();
        for (i, ch) in s.char_indices().rev() {
            if !ch.is_ascii_digit() {
                break;
            }
            idx = i;
        }
        (&s[..idx], &s[idx..])
    }

    let (prefix_start, num_start) = split_numeric_suffix(start);
    let (prefix_end, num_end) = split_numeric_suffix(end);

    if prefix_start != prefix_end || num_start.is_empty() || num_end.is_empty() {
        // Fallback: if pattern doesn't match, just return start and end as-is
        return vec![start.to_string(), end.to_string()];
    }

    let width = num_start.len().max(num_end.len());
    let start_n: i64 = num_start.parse().unwrap_or(0);
    let end_n: i64 = num_end.parse().unwrap_or(start_n);

    let (from, to) = if start_n <= end_n {
        (start_n, end_n)
    } else {
        (end_n, start_n)
    };

    let mut v = Vec::new();
    for n in from..=to {
        v.push(format!("{}{:0width$}", prefix_start, n, width = width));
    }
    v
}

fn collect_symbols(configs: &[config::StrategyConfig]) -> Vec<String> {
    let mut all_symbols = Vec::new();
    for config in configs {
        for symbol in &config.symbols {
            if !all_symbols.contains(symbol) {
                all_symbols.push(symbol.clone());
            }
        }
    }
    all_symbols
}

pub fn start(core: Arc<dyn strategy_core::StrategyCore>) -> anyhow::Result<()> {
    // Load configuration with retry (handles transient Supabase/Cloudflare errors)
    let config_manager = config::ConfigManager::new();
    let (
        login_names,
        interval,
        base_last_book_ticker_latency_ms,
        gate_last_book_ticker_latency_ms,
        restart_interval_secs,
        slippage,
        size_threshold,
        order_divider,
        base_exchange,
    ) = parse_args()?;
    info!(
        "[GateHFT Rust] Login names: {:?}, Interval: {:?}, Base({}) latency: {:?}, Gate latency: {:?}, Restart interval: {:?}, Slippage: {:?}, Size threshold: {:?}, Order divider: {:?}",
        login_names,
        interval,
        base_exchange,
        base_last_book_ticker_latency_ms,
        gate_last_book_ticker_latency_ms,
        restart_interval_secs,
        slippage,
        size_threshold,
        order_divider
    );
    let configs = loop {
        match config_manager.load_strategy_configs(login_names.clone()) {
            Ok(cfgs) if !cfgs.is_empty() => break cfgs,
            Ok(_) => {
                error!("[GateHFT Rust] Strategy configs empty, retrying in 300s");
            }
            Err(e) => {
                error!(
                    "[GateHFT Rust] Failed to load strategy configs: {}. Retrying in 300s",
                    e
                );
            }
        }
        std::thread::sleep(Duration::from_secs(300));
    };

    let configs_shared = Arc::new(RwLock::new(Arc::new(configs)));
    let configs_snapshot = configs_shared.read().clone();

    // Collect all symbols
    let mut all_symbols = collect_symbols(configs_snapshot.as_slice());

    // Initialize data cache first (shared across GateOrderManager, IPC, Strategy)
    let cache = Arc::new(data_cache::DataCache::new(base_exchange.clone()));

    // Load account credentials
    let subaccount_keys_path = std::env::var("SUBACCOUNT_KEYS_PATH")
        .unwrap_or_else(|_| "subaccount_keys.json".to_string());
    let main_account_secrets_path = std::env::var("MAIN_ACCOUNT_SECRETS_PATH")
        .unwrap_or_else(|_| "main_account_secrets.json".to_string());
    let account_manager = account_manager::AccountManager::new(
        &subaccount_keys_path,
        &main_account_secrets_path,
        None,
    )
    .context("Failed to load account manager")?;

    let order_manager_client = order_manager_client::OrderManagerClient::instance();
    let overrides_by_login: std::collections::HashMap<String, config::TradingRuntimeOverrides> =
        configs_snapshot
            .iter()
            .map(|c| {
                (
                    c.login_name.clone(),
                    c.trading_runtime.clone().unwrap_or_default(),
                )
            })
            .collect();
    order_manager_client.set_overrides_by_login(overrides_by_login);

    let mut gate_order_managers: std::collections::HashMap<
        String,
        Arc<gate_order_manager::GateOrderManager>,
    > = std::collections::HashMap::new();

    for config in configs_snapshot.iter() {
        // Python과 동일하게: get_sub_account_uid_from_login_name과 get_sub_account_key_from_login_name 사용
        let uid = match account_manager.get_sub_account_uid_from_login_name(&config.login_name) {
            Some(uid) => uid,
            None => {
                warn!(
                    "[GateHFT Rust] Warning: UID not found for login_name: {}",
                    config.login_name
                );
                continue;
            }
        };

        let sub_account_key =
            match account_manager.get_sub_account_key_from_login_name(&config.login_name) {
                Some(key) => key,
                None => {
                    warn!(
                        "[GateHFT Rust] Warning: SubAccountKey not found for login_name: {}",
                        config.login_name
                    );
                    continue;
                }
            };

        let login_status_url_template = config::resolve_string(
            config.trading_runtime.as_ref().and_then(|o| o.login_status_url.as_ref()),
            "LOGIN_STATUS_URL",
            "http://localhost:3005/puppeteer/account-status/{login_name}",
        );
        let manager = Arc::new(gate_order_manager::GateOrderManager::new(
            config.login_name.clone(),
            sub_account_key.key,
            sub_account_key.secret,
            uid,
            config.symbols.clone(),
            config.trade_settings.order_size,
            Some(cache.clone()), // Pass DataCache for print_trade
            config.trade_settings.clone(),
            false,
            false,
            false,
            false, // print_orders
            login_status_url_template,
            Some(order_manager_client.clone()),
            false,
            false,
            None,
        ));

        if let Err(e) = manager.update_positions() {
            error!(
                "[GateHFT Rust] Warning: Failed to update positions: {}. Retrying in 300s",
                e
            );
            continue;
        }

        // sleep 100ms
        std::thread::sleep(Duration::from_millis(100));

        let positions = (**manager.positions.load()).clone();
        trace!(
            "[GateHFT Rust] {} Positions: {:?}",
            config.login_name,
            positions
        );
        let existing_positions = positions
            .iter()
            .filter(|(_contract, position)| position.size != 0)
            .map(|(contract, _position)| contract.clone())
            .collect::<Vec<String>>();
        info!(
            "[GateHFT Rust] {} Existing positions: {:?}",
            config.login_name, existing_positions
        );

        // add existing positions to all_symbols and con
        for contract in existing_positions {
            if !all_symbols.contains(&contract) {
                all_symbols.push(contract.clone());
            }
        }
        // Start background update loop
        manager.start_update_loop();

        if let Err(e) = manager.cancel_all_orders() {
            warn!(
                "[GateHFT Rust] Failed to cancel all orders for {}: {}",
                config.login_name, e
            );
        }

        gate_order_managers.insert(config.login_name.clone(), manager);

        // Don't print - match Python silent behavior
    }

    if gate_order_managers.is_empty() {
        return Err(anyhow::anyhow!(
            "No valid credentials found for any login_name"
        ));
    }

    // Clone gate_order_managers for signal handler before moving it to strategy
    let gate_order_managers_for_signal = gate_order_managers.clone();

    let all_symbols_shared = Arc::new(RwLock::new(all_symbols));
    let initial_symbols = all_symbols_shared.read().clone();

    // Data source: ZMQ (direct from data_publisher) or IPC (via data_subscriber)
    let base_data_url = match base_exchange.to_lowercase().as_str() {
        "binance" => std::env::var("BINANCE_DATA_URL").ok(),
        "bitget" => std::env::var("BITGET_DATA_URL").ok(),
        "bybit" => std::env::var("BYBIT_DATA_URL").ok(),
        "okx" => std::env::var("OKX_DATA_URL").ok(),
        _ => None,
    };
    let data_client: Arc<dyn DataClient> =
        if let Ok(gate_data_url) = std::env::var("GATE_DATA_URL") {
            info!(
                "[GateHFT Rust] ZMQ data client: url={}, base({}): {:?}, symbols={}",
                gate_data_url,
                base_exchange,
                base_data_url,
                initial_symbols.len()
            );
            Arc::new(zmq_sub_client::ZmqSubClient::new(
                gate_data_url,
                std::env::var("GATE_DATA_URL_BACKUP").ok(),
                base_data_url,
                base_exchange.clone(),
                initial_symbols,
                cache.clone(),
            ))
        } else {
            let socket_path = std::env::var("IPC_SOCKET_PATH")
                .unwrap_or_else(|_| "/tmp/hft_data_subscriber.sock".to_string());
            info!(
                "[GateHFT Rust] IPC data client: socket={}, symbols={}",
                socket_path,
                initial_symbols.len()
            );
            Arc::new(ipc_client::IpcClient::new(
                socket_path,
                initial_symbols,
                cache.clone(),
            ))
        };

    // Periodically refresh configs/symbols from Supabase
    let refresh_secs = std::env::var("SUPABASE_REFRESH_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300);
    info!(
        "[GateHFT Rust] Supabase refresh interval: {}s",
        refresh_secs
    );
    let configs_shared_for_refresh = configs_shared.clone();
    let all_symbols_shared_for_refresh = all_symbols_shared.clone();
    let data_client_for_refresh = data_client.clone();
    let gate_order_managers_for_refresh = gate_order_managers.clone();
    let order_manager_client_for_refresh = order_manager_client.clone();
    let login_names_for_refresh = login_names.clone();
    std::thread::spawn(move || {
        let config_manager = config::ConfigManager::new();
        loop {
            std::thread::sleep(Duration::from_secs(refresh_secs));
            let new_configs =
                match config_manager.load_strategy_configs(login_names_for_refresh.clone()) {
                    Ok(cfgs) if !cfgs.is_empty() => cfgs,
                    Ok(_) => {
                        warn!("[GateHFT Rust] Supabase refresh returned empty configs");
                        continue;
                    }
                    Err(e) => {
                        warn!("[GateHFT Rust] Supabase refresh failed: {}", e);
                        continue;
                    }
                };

            let changed = {
                let current = configs_shared_for_refresh.read();
                **current != new_configs
            };
            if !changed {
                continue;
            }

            info!(
                "[GateHFT Rust] Supabase configs updated ({} accounts)",
                new_configs.len()
            );

            {
                let mut guard = configs_shared_for_refresh.write();
                *guard = Arc::new(new_configs.clone());
            }

            let overrides_by_login: std::collections::HashMap<String, config::TradingRuntimeOverrides> =
                new_configs
                    .iter()
                    .map(|c| {
                        (
                            c.login_name.clone(),
                            c.trading_runtime.clone().unwrap_or_default(),
                        )
                    })
                    .collect();
            order_manager_client_for_refresh.set_overrides_by_login(overrides_by_login);

            let mut next_symbols = collect_symbols(&new_configs);

            for config in &new_configs {
                if let Some(manager) = gate_order_managers_for_refresh.get(&config.login_name) {
                    manager.update_strategy_config(
                        config.symbols.clone(),
                        config.trade_settings.clone(),
                    );

                    let positions = manager.positions.load();
                    for (contract, position) in positions.iter() {
                        if position.size == 0 {
                            continue;
                        }
                        if !next_symbols.contains(contract) {
                            next_symbols.push(contract.clone());
                        }
                    }
                } else {
                    warn!(
                        "[GateHFT Rust] No GateOrderManager for refreshed login_name: {}",
                        config.login_name
                    );
                }
            }

            let mut should_restart_ipc = false;
            {
                let mut guard = all_symbols_shared_for_refresh.write();
                if *guard != next_symbols {
                    *guard = next_symbols.clone();
                    should_restart_ipc = true;
                }
            }

            if should_restart_ipc {
                data_client_for_refresh.update_symbols(next_symbols);
                data_client_for_refresh.stop();
                if let Err(e) = data_client_for_refresh.start() {
                    warn!(
                        "[GateHFT Rust] Failed to restart data client after config refresh: {}",
                        e
                    );
                }
            }
        }
    });

    // Initialize and start strategy
    let strategy = strategy_close::StrategyClose::new(
        configs_shared.clone(),
        all_symbols_shared.clone(),
        cache,
        data_client,
        order_manager_client,
        gate_order_managers,
        core,
        interval,
        base_last_book_ticker_latency_ms,
        gate_last_book_ticker_latency_ms,
        restart_interval_secs,
        slippage,
        size_threshold,
        order_divider,
    );

    // Set up Ctrl+C handler to cancel all orders on interrupt
    ctrlc::set_handler(move || {
        info!("[GateHFT Rust] Keyboard interrupt received. Cancelling all orders...");

        // Spawn cancellation in a separate thread to avoid blocking the signal handler
        let managers = gate_order_managers_for_signal.clone();
        std::thread::spawn(move || {
            for (login_name, manager) in managers.iter() {
                info!("[GateHFT Rust] Cancelling all orders for {}", login_name);
                // Use cancel_websocket_orders_matched instead of cancel_all_orders
                // to avoid blocking indefinitely in the signal handler
                if let Err(e) = manager.cancel_websocket_orders_matched(None, None) {
                    error!(
                        "[GateHFT Rust] Failed to cancel orders via WebSocket for {}: {}",
                        login_name, e
                    );
                } else {
                    info!(
                        "[GateHFT Rust] Sent cancel request via WebSocket for {}",
                        login_name
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            info!("[GateHFT Rust] Exiting...");
            std::process::exit(0);
        });

        // Give cancellation a moment to start, then exit
        std::thread::sleep(Duration::from_millis(4000));
        std::process::exit(0);
    })
    .expect("Error setting Ctrl+C handler");

    info!("[GateHFT Rust] Starting strategy");
    strategy.start()?;
    info!("[GateHFT Rust] Strategy started");

    // Keep running until interrupted
    loop {
        trace!("[GateHFT Rust] Sleeping for 1 second");
        std::thread::sleep(Duration::from_secs(1));
    }
}

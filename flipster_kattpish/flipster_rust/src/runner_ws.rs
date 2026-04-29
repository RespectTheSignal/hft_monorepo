use crate::{
    account_manager, config, data_cache, data_client::DataClient, gate_order_manager,
    ipc_client, market_watcher, order_manager_client, state_manager, strategy_core, strategy_ws,
    zmq_sub_client,
};
use anyhow::{bail, Context};
use arc_swap::ArcSwap;
use log::{error, info, trace, warn};
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

pub fn initialize() -> anyhow::Result<Option<f64>> {
    dotenvy::dotenv().ok();
    crate::log::init_logging();
    info!("[GateHFT Rust] Starting");

    // Flipster mode: initialize global cookie store (see runner::initialize).
    if let Ok(cdp_port_s) = std::env::var("FLIPSTER_CDP_PORT") {
        if let Ok(cdp_port) = cdp_port_s.parse::<u16>() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("build tokio runtime for cookie init")?;
            rt.block_on(async {
                crate::flipster_cookie_store::init_global(cdp_port).await
            })
            .context("init flipster cookie store")?;
            info!(
                "[Flipster Rust] runner_ws: Cookie store initialized via CDP port {}",
                cdp_port
            );
        }
    }

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

/// Parses a boolean CLI value: "true", "1", "yes" => true; "false", "0", "no" => false.
fn parse_bool_arg(s: &str) -> bool {
    let s = s.trim().to_lowercase();
    matches!(s.as_str(), "true" | "1" | "yes" | "on")
}

fn parse_args() -> anyhow::Result<(
    Vec<String>,
    u64,
    u64,
    u64,
    u64,
    u64,
    String,
    Option<bool>,
    Option<bool>,
    bool,
    bool,
    bool,
)> {
    // Priority:
    // 1) CLI: --login_names v3_sb_001~v3_sb_010 or comma-separated
    // 2) ENV: LOGIN_NAMES (same format)
    // 3) Error
    let args: Vec<String> = std::env::args().collect();

    let mut spec: Option<String> = None;
    let mut interval: u64 = 10;
    let mut base_last_book_ticker_latency_ms: u64 = 200;
    let mut gate_last_book_ticker_latency_ms: u64 = 100;
    let mut restart_interval_secs: u64 = 60 * 60 * 1;
    let mut initial_sleep_ms: u64 = 100;
    let mut base_exchange: Option<String> = None;
    let mut repeat_profitable_order: Option<bool> = None;
    let mut use_few_orders_closing_trigger: Option<bool> = None;
    let mut print_orders: bool = false;
    let mut run_small_orders_loop: bool = false;
    let mut no_safe_open_order: bool = false;
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
            gate_last_book_ticker_latency_ms = args[i + 1].parse::<u64>().unwrap_or(100);
            i += 1;
        }
        if args[i] == "--restart-interval" && i + 1 < args.len() {
            restart_interval_secs = args[i + 1].parse::<u64>().unwrap_or(60 * 60);
            i += 1;
        }

        if args[i] == "--initial-sleep" && i + 1 < args.len() {
            initial_sleep_ms = args[i + 1].parse::<u64>().unwrap_or(100);
            i += 1;
        }
        if args[i] == "--base-exchange" && i + 1 < args.len() {
            base_exchange = Some(args[i + 1].clone());
            i += 1;
        }
        if args[i] == "--repeat-profitable-order" && i + 1 < args.len() {
            repeat_profitable_order = Some(parse_bool_arg(&args[i + 1]));
            i += 1;
        }
        if args[i] == "--use-few-orders-closing-trigger" && i + 1 < args.len() {
            use_few_orders_closing_trigger = Some(parse_bool_arg(&args[i + 1]));
            i += 1;
        }
        if args[i] == "--print-orders" {
            print_orders = true;
            i += 1;
            continue;
        }
        if args[i] == "--run-small-orders-loop" {
            run_small_orders_loop = true;
            i += 1;
            continue;
        }
        if args[i] == "--no-safe-open-order" {
            no_safe_open_order = true;
            i += 1;
            continue;
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
    if spec.is_none() && std::env::var("FLIPSTER_CDP_PORT").is_ok() {
        spec = Some("flipster".to_string());
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
        initial_sleep_ms,
        base_exchange,
        repeat_profitable_order,
        use_few_orders_closing_trigger,
        print_orders,
        run_small_orders_loop,
        no_safe_open_order,
    ))
}

fn apply_cli_trade_overrides(
    mut ts: config::TradeSettings,
    repeat_profitable_order: Option<bool>,
    use_few_orders_closing_trigger: Option<bool>,
) -> config::TradeSettings {
    if let Some(b) = repeat_profitable_order {
        ts.repeat_profitable_order = b;
    }
    if let Some(b) = use_few_orders_closing_trigger {
        ts.use_few_orders_closing_trigger = Some(b);
    }
    ts
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
    let mut seen = HashSet::new();
    let mut all_symbols = Vec::new();
    for config in configs {
        for symbol in &config.symbols {
            if seen.insert(symbol.as_str()) {
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
        initial_sleep_ms,
        base_exchange,
        cli_repeat_profitable_order,
        cli_use_few_orders_closing_trigger,
        print_orders,
        run_small_orders_loop,
        no_safe_open_order,
    ) = parse_args()?;
    info!(
        "[GateHFT Rust] Login names: {:?}, Interval: {:?}, Base({}) latency: {:?}, Gate latency: {:?}",
        login_names,
        interval,
        base_exchange,
        base_last_book_ticker_latency_ms,
        gate_last_book_ticker_latency_ms
    );
    if cli_repeat_profitable_order.is_some() || cli_use_few_orders_closing_trigger.is_some() {
        info!(
            "[GateHFT Rust] CLI overrides: repeat_profitable_order={:?}, use_few_orders_closing_trigger={:?}",
            cli_repeat_profitable_order, cli_use_few_orders_closing_trigger
        );
    }
    if no_safe_open_order {
        info!("[GateHFT Rust] CLI: --no-safe-open-order (open order price = chance.price, no 0.95/1.05)");
    }
    let configs = if crate::flipster_cookie_store::global().is_some() {
        crate::runner::build_flipster_configs_pub(&login_names)?
    } else {
        loop {
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
        }
    };

    let mut configs: Vec<config::StrategyConfig> = configs
        .into_iter()
        .map(|mut c| {
            c.trade_settings = apply_cli_trade_overrides(
                c.trade_settings.clone(),
                cli_repeat_profitable_order,
                cli_use_few_orders_closing_trigger,
            );
            c
        })
        .collect();

    // v11 series: repeat profitable order via WebSocket amend instead of order_manager_client
    if core.name() == "V11Strategy" || core.name() == "V11_10Strategy" {
        for c in &mut configs {
            c.trade_settings.repeat_profitable_order_use_amend = Some(true);
        }
    }

    let configs_shared = Arc::new(RwLock::new(Arc::new(configs)));
    let configs_snapshot = configs_shared.read().clone();

    // Collect all symbols
    let mut all_symbols = collect_symbols(configs_snapshot.as_slice());

    // Initialize data cache first (shared across GateOrderManager, IPC, Strategy)
    let cache = Arc::new(data_cache::DataCache::new(base_exchange.clone()));
    data_cache::set_global(cache.clone());

    // Load account credentials
    let account_manager = if crate::flipster_cookie_store::global().is_some() {
        crate::runner::write_flipster_dummy_account_files_pub()?;
        account_manager::AccountManager::new(
            "/tmp/flipster_subaccount_keys.json",
            "/tmp/flipster_main_account_secrets.json",
            Some("127.0.0.1".to_string()),
        )
        .context("Failed to init flipster dummy account manager")?
    } else {
        let subaccount_keys_path = std::env::var("SUBACCOUNT_KEYS_PATH")
            .unwrap_or_else(|_| "subaccount_keys.json".to_string());
        let main_account_secrets_path = std::env::var("MAIN_ACCOUNT_SECRETS_PATH")
            .unwrap_or_else(|_| "main_account_secrets.json".to_string());
        account_manager::AccountManager::new(
            &subaccount_keys_path,
            &main_account_secrets_path,
            None,
        )
        .context("Failed to load account manager")?
    };

    let redis_url = std::env::var("REDIS_URL").ok();
    let process_overrides = configs_snapshot.first().and_then(|c| c.trading_runtime.as_ref());
    let redis_state_prefix = config::resolve_string(
        process_overrides.and_then(|o| o.redis_state_prefix.as_ref()),
        "REDIS_STATE_PREFIX",
        "gate_hft:state",
    );
    let state_manager = match redis_url {
        Some(url) => match state_manager::StateManager::new(&url, &redis_state_prefix) {
            Ok(manager) => Some(manager),
            Err(e) => {
                warn!(
                    "[GateHFT Rust] Failed to initialize Redis state manager: {}",
                    e
                );
                None
            }
        },
        None => None,
    };

    // Singleton OrderManagerClient; per-login overrides (trading_runtime) set via set_overrides_by_login and resolved at send time.
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

    // Create GateOrderManager for each login_name
    let mut gate_order_managers: std::collections::HashMap<
        String,
        Arc<gate_order_manager::GateOrderManager>,
    > = std::collections::HashMap::new();

    let total_net_positions_usdt_size_all_accounts =
        Arc::new(ArcSwap::from_pointee(0.0_f64));

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
            true,
            true,
            true,
            print_orders,
            login_status_url_template,
            Some(order_manager_client.clone()),
            run_small_orders_loop,
            false,
            Some(total_net_positions_usdt_size_all_accounts.clone()),
        ));

        // check if is logged in
        let is_logged_in = manager.check_login_status();
        if !is_logged_in {
            warn!(
                "[GateHFT Rust] Warning: {} is not logged in",
                config.login_name
            );
            std::thread::sleep(Duration::from_millis(initial_sleep_ms));
        } else {
            info!(
                "\x1b[92m[GateHFT Rust] ✅ {} is logged in\x1b[0m",
                config.login_name
            );
            std::thread::sleep(Duration::from_millis(100));
        }

        if let Err(e) = manager.update_positions() {
            error!(
                "[GateHFT Rust] Warning: Failed to update positions: {}. Retrying in 300s",
                e
            );
            continue;
        }

        let positions = (**manager.positions.load()).clone();
        trace!(
            "[GateHFT Rust] {} Positions: {:?}",
            config.login_name,
            positions
        );
        let existing_positions = positions
            .iter()
            .filter(|(_, position)| position.size != 0)
            .map(|(contract, _)| contract.clone())
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
        // sleep 100ms
        std::thread::sleep(Duration::from_millis(initial_sleep_ms));

        // Start background update loop
        manager.start_update_loop();

        if let Some(state_manager) = state_manager.clone() {
            start_state_sync(state_manager, manager.clone());
        }

        gate_order_managers.insert(config.login_name.clone(), manager);

        // Don't print - match Python silent behavior
    }

    if gate_order_managers.is_empty() {
        return Err(anyhow::anyhow!(
            "No valid credentials found for any login_name"
        ));
    }

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
    let data_client: Arc<dyn DataClient> = if crate::flipster_cookie_store::global().is_some() {
        let api_key = std::env::var("FLIPSTER_API_KEY").unwrap_or_default();
        let api_secret = std::env::var("FLIPSTER_API_SECRET").unwrap_or_default();
        let cdp_port = crate::flipster_cookie_store::global()
            .map(|s| s.cdp_port())
            .unwrap_or(9230);
        info!(
            "[Flipster Rust] runner_ws: WS data client: cdp_port={}, symbols={}",
            cdp_port,
            initial_symbols.len()
        );
        let rt = crate::flipster_runtime::handle();
        crate::flipster_ws_client::FlipsterWsClient::new(
            cache.clone(),
            api_key,
            api_secret,
            cdp_port,
            initial_symbols,
            rt,
        )
    } else if let Ok(gate_data_url) = std::env::var("GATE_DATA_URL") {
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
    let cli_repeat_for_refresh = cli_repeat_profitable_order;
    let cli_use_few_for_refresh = cli_use_few_orders_closing_trigger;
    let core_name_for_refresh = core.name().to_string();
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

            let mut new_configs: Vec<config::StrategyConfig> = new_configs
                .into_iter()
                .map(|mut c| {
                    c.trade_settings = apply_cli_trade_overrides(
                        c.trade_settings.clone(),
                        cli_repeat_for_refresh,
                        cli_use_few_for_refresh,
                    );
                    c
                })
                .collect();

            if core_name_for_refresh == "V11Strategy" || core_name_for_refresh == "V11_10Strategy" {
                for c in &mut new_configs {
                    c.trade_settings.repeat_profitable_order_use_amend = Some(true);
                }
            }

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
                        "[GateHFT Rust] Failed to restart IPC client after config refresh: {}",
                        e
                    );
                }
            }
        }
    });

    // Initialize and start strategy
    let mut use_market_watcher = std::env::var("USE_MARKET_WATCHER")
        .ok()
        .map(|v| v.to_ascii_lowercase())
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if !use_market_watcher && (core.name() == "V10Strategy" || core.name() == "V11_10Strategy") {
        // bail!(
        //     "[GateHFT Rust] {} requires market watcher (USE_MARKET_WATCHER=true)",
        //     core.name()
        // );
        use_market_watcher = true;
    }
    let market_watcher_update_secs = std::env::var("MARKET_WATCHER_UPDATE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5);
    let market_watcher_window_minutes = std::env::var("MARKET_WATCHER_WINDOW_MINUTES")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(5);

    let market_gap_state: market_watcher::SharedMarketGapState = Arc::new(ArcSwap::from(Arc::new(
        market_watcher::MarketGapState::new(market_watcher_window_minutes),
    )));
    if use_market_watcher {
        let questdb_url = std::env::var("QUESTDB_URL")
            .or_else(|_| std::env::var("QUESTDB_HTTP_URL"))
            .unwrap_or_else(|_| "http://localhost:9000".to_string());
        let market_gap_redis_url = std::env::var("MARKET_GAP_REDIS_URL")
            .ok()
            .or_else(|| std::env::var("REDIS_URL").ok());
        let market_gap_redis_prefix = std::env::var("MARKET_GAP_REDIS_PREFIX")
            .unwrap_or_else(|_| "gate_hft:market_gap".to_string());
        let quote_exchange = if crate::flipster_cookie_store::global().is_some() {
            "flipster".to_string()
        } else {
            "gate".to_string()
        };
        let market_watcher = market_watcher::MarketWatcher::new(
            questdb_url,
            market_watcher_update_secs,
            market_watcher_window_minutes,
            market_gap_state.clone(),
            quote_exchange,
            base_exchange.clone(),
            market_gap_redis_url,
            market_gap_redis_prefix,
        );
        market_watcher.start();
    } else {
        info!("[GateHFT Rust] Market watcher disabled (USE_MARKET_WATCHER)");
    }

    // Ctrl+C handler: optionally cancel all open orders on exit (CLOSE_OPEN_ORDERS_ON_EXIT=true)
    let close_open_orders_on_exit = std::env::var("CLOSE_OPEN_ORDERS_ON_EXIT")
        .as_deref()
        .map(parse_bool_arg)
        .unwrap_or(false);
    let gate_order_managers_for_exit = gate_order_managers.clone();
    ctrlc::set_handler(move || {
        if close_open_orders_on_exit {
            info!("[GateHFT Rust] Shutting down: cancelling all open orders (CLOSE_OPEN_ORDERS_ON_EXIT=true)");
            for (login_name, manager) in gate_order_managers_for_exit.iter() {
                
                if let Err(e) = manager.cancel_all_orders() {
                    error!(
                        "[GateHFT Rust] Failed to cancel all orders for {}: {}",
                        login_name, e
                    );
                } else {
                    info!("[GateHFT Rust] Cancelled all orders for {}", login_name);
                }
            }
        } else {
            info!("[GateHFT Rust] Shutting down: amending all open orders (buy price/2, sell price*2)");
            for (login_name, manager) in gate_order_managers_for_exit.iter() {
                let orders = manager.get_open_orders();
                for (_, order) in orders {
                    let min_order_size = if let Some(contract) = manager.get_contract(order.contract.as_str()) {
                        Some(contract.order_size_min as i64)
                    } else {
                        None
                    };
                    let order_size = if let Some(min_order_size) = min_order_size {
                        if order.size > 0 {
                            min_order_size
                        } else {
                            -min_order_size
                        }
                    } else {
                        order.size
                    };

                    let contract = manager.get_contract(order.contract.as_str());

                    let mark_price = if let Some(contract) = contract {
                        contract.mark_price.parse::<f64>().unwrap_or(order.price)
                    } else {
                        order.price
                    };

                    let usdt_amount = manager.get_usdt_amount_from_size(order.contract.as_str(), order_size);

                    if usdt_amount.abs() < 10.0 {
                        continue;
                    }

                    let new_price = if order.size > 0 {
                        mark_price * 0.5
                    } else {
                        mark_price * 1.5
                    };

                    let side = if order.size > 0 { "buy" } else { "sell" };
                    if let Err(e) = manager.amend_websocket_order(
                        &order.id,
                        new_price,
                        Some(order_size),
                        Some(order.contract.as_str()),
                        None,
                        Some(side),
                        None,
                        None,
                    ) {
                        error!(
                            "[GateHFT Rust] Failed to amend order {} for {} on exit: {}",
                            order.id, login_name, e
                        );
                    } else {
                        info!(
                            "[GateHFT Rust] Amended order {} for {}: price {} -> {}",
                            order.id, login_name, order.price, new_price
                        );
                    }
                }
            }
        }
        std::process::exit(0);
    })
    .context("Failed to set Ctrl+C handler")?;

    let strategy = strategy_ws::Strategy::new(
        configs_shared.clone(),
        all_symbols_shared.clone(),
        cache,
        data_client,
        order_manager_client,
        gate_order_managers,
        core,
        market_gap_state,
        interval,
        base_last_book_ticker_latency_ms,
        gate_last_book_ticker_latency_ms,
        restart_interval_secs,
        no_safe_open_order,
    );

    info!("[GateHFT Rust] Starting strategy");
    strategy.start()?;
    info!("[GateHFT Rust] Strategy started");

    // Keep running until interrupted
    loop {
        trace!("[GateHFT Rust] Sleeping for 1 second");
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn start_state_sync(
    state_manager: state_manager::StateManager,
    manager: Arc<gate_order_manager::GateOrderManager>,
) {
    let login_name = manager.login_name.clone();
    std::thread::spawn(move || loop {
        let is_healthy = manager.is_healthy().unwrap_or(false);
        let too_many_request = manager.is_recently_too_many_orders();
        let num_positions = manager.num_positions();

        let login_base_url = manager.login_base_url();

        let _ = state_manager
            .update_state(
                &login_name,
                is_healthy,
                too_many_request,
                num_positions,
                login_base_url,
            )
            .ok();
        std::thread::sleep(Duration::from_secs(10));
    });
}

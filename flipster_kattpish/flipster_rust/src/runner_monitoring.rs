// Monitoring mode: same as runner_ws (IPC, config refresh, market_watcher, gate_order_manager WS)
// but no order placement. All gate_order_manager WS message prints are visible.

use crate::{
    account_manager, config, data_cache, data_client::DataClient, gate_order_manager, ipc_client,
    market_watcher, order_manager_client, state_manager, strategy_monitoring, zmq_sub_client,
};
use anyhow::Context;
use arc_swap::ArcSwap;
use log::{error, info, trace, warn};
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

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
    u64,
    bool,
    bool,
    bool,
)> {
    let args: Vec<String> = std::env::args().collect();
    let mut spec: Option<String> = None;
    let mut interval: u64 = 10;
    let mut base_last_book_ticker_latency_ms: u64 = 200;
    let mut gate_last_book_ticker_latency_ms: u64 = 100;
    let mut restart_interval_secs: u64 = 60 * 60;
    let mut initial_sleep_ms: u64 = 100;
    let mut base_exchange: Option<String> = None;
    let mut repeat_profitable_order: Option<bool> = None;
    let mut use_few_orders_closing_trigger: Option<bool> = None;
    let mut monitor_interval_secs: u64 = 30;
    let mut health_check: bool = false;
    let mut print_orders: bool = false;
    let mut run_small_orders_loop: bool = false;

    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--login_names" && i + 1 < args.len() {
            spec = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        if arg == "--interval" && i + 1 < args.len() {
            interval = args[i + 1].parse::<u64>().unwrap_or(10);
            i += 2;
            continue;
        }
        if arg == "--monitor-interval" && i + 1 < args.len() {
            monitor_interval_secs = args[i + 1].parse::<u64>().unwrap_or(30);
            i += 2;
            continue;
        }
        if arg == "--base-latency" && i + 1 < args.len() {
            base_last_book_ticker_latency_ms = args[i + 1].parse::<u64>().unwrap_or(200);
            i += 2;
            continue;
        }
        if arg == "--gate-latency" && i + 1 < args.len() {
            gate_last_book_ticker_latency_ms = args[i + 1].parse::<u64>().unwrap_or(100);
            i += 2;
            continue;
        }
        if arg == "--restart-interval" && i + 1 < args.len() {
            restart_interval_secs = args[i + 1].parse::<u64>().unwrap_or(60 * 60);
            i += 2;
            continue;
        }
        if arg == "--initial-sleep" && i + 1 < args.len() {
            initial_sleep_ms = args[i + 1].parse::<u64>().unwrap_or(100);
            i += 2;
            continue;
        }
        if arg == "--base-exchange" && i + 1 < args.len() {
            base_exchange = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        if arg == "--repeat-profitable-order" && i + 1 < args.len() {
            repeat_profitable_order = Some(parse_bool_arg(&args[i + 1]));
            i += 2;
            continue;
        }
        if arg == "--use-few-orders-closing-trigger" && i + 1 < args.len() {
            use_few_orders_closing_trigger = Some(parse_bool_arg(&args[i + 1]));
            i += 2;
            continue;
        }
        if arg == "--health-check" && i + 1 < args.len() {
            health_check = true;
            i += 2;
            continue;
        }
        if arg == "--print-orders" || arg == "--print-order" {
            print_orders = true;
            i += 1;
            continue;
        }
        if arg == "--run-small-orders-loop" && i + 1 < args.len() {
            run_small_orders_loop = true;
            i += 2;
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
    if base_exchange.is_none() {
        if let Ok(env_base_exchange) = std::env::var("BASE_EXCHANGE") {
            if !env_base_exchange.trim().is_empty() {
                base_exchange = Some(env_base_exchange);
            }
        }
    }

    let spec = spec.ok_or_else(|| {
        anyhow::anyhow!("No login names. Use --login_names v3_sb_001~v3_sb_010 or set LOGIN_NAMES")
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
        monitor_interval_secs,
        health_check,
        print_orders,
        run_small_orders_loop,
    ))
}

fn expand_login_spec(spec: &str) -> Vec<String> {
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

/// Start monitoring: same as runner_ws (IPC, config refresh, market_watcher, gate_order_manager)
/// but no order placement. GateOrderManager WS messages print as normal.
pub fn run() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    crate::log::init_logging();
    info!("[Monitoring] Same as runner_ws (IPC, gate WS, etc.) but no orders");

    let (
        login_names,
        _interval,
        _base_last_book_ticker_latency_ms,
        gate_last_book_ticker_latency_ms,
        _restart_interval_secs,
        initial_sleep_ms,
        base_exchange,
        cli_repeat_profitable_order,
        cli_use_few_orders_closing_trigger,
        monitor_interval_secs,
        health_check,
        print_orders,
        run_small_orders_loop,
    ) = parse_args()?;

    info!(
        "[Monitoring] Login names: {:?}, interval: {:?}, base: {:?}, gate latency: {:?}, monitor_interval: {}s",
        login_names, _interval, base_exchange, gate_last_book_ticker_latency_ms, monitor_interval_secs
    );

    let config_manager = config::ConfigManager::new();
    let configs = loop {
        match config_manager.load_strategy_configs(login_names.clone()) {
            Ok(cfgs) if !cfgs.is_empty() => break cfgs,
            Ok(_) => {
                error!("[Monitoring] Strategy configs empty, retrying in 300s");
            }
            Err(e) => {
                error!(
                    "[Monitoring] Failed to load configs: {}. Retrying in 300s",
                    e
                );
            }
        }
        std::thread::sleep(Duration::from_secs(300));
    };

    let configs: Vec<config::StrategyConfig> = configs
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

    let configs_shared = Arc::new(RwLock::new(Arc::new(configs)));
    let configs_snapshot = configs_shared.read().clone();
    let mut all_symbols = collect_symbols(configs_snapshot.as_slice());
    let cache = Arc::new(data_cache::DataCache::new(base_exchange.clone()));

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

    let redis_url = std::env::var("REDIS_URL").ok();
    let process_overrides = configs_snapshot
        .first()
        .and_then(|c| c.trading_runtime.as_ref());
    let redis_state_prefix = config::resolve_string(
        process_overrides.and_then(|o| o.redis_state_prefix.as_ref()),
        "REDIS_STATE_PREFIX",
        "gate_hft:state",
    );
    let state_manager = match redis_url.as_deref() {
        Some(url) => match state_manager::StateManager::new(url, &redis_state_prefix) {
            Ok(m) => Some(m),
            Err(e) => {
                warn!("[Monitoring] Redis state manager init failed: {}", e);
                None
            }
        },
        None => None,
    };

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
        let uid = match account_manager.get_sub_account_uid_from_login_name(&config.login_name) {
            Some(u) => u,
            None => {
                warn!("[Monitoring] UID not found for {}", config.login_name);
                continue;
            }
        };

        let sub_account_key =
            match account_manager.get_sub_account_key_from_login_name(&config.login_name) {
                Some(k) => k,
                None => {
                    warn!("[Monitoring] Key not found for {}", config.login_name);
                    continue;
                }
            };

        let login_status_url_template = config::resolve_string(
            config
                .trading_runtime
                .as_ref()
                .and_then(|o| o.login_status_url.as_ref()),
            "LOGIN_STATUS_URL",
            "http://localhost:3005/puppeteer/account-status/{login_name}",
        );

        let mut trade_settings = config.trade_settings.clone();
        trade_settings.repeat_profitable_order = false;

        let manager = Arc::new(gate_order_manager::GateOrderManager::new(
            config.login_name.clone(),
            sub_account_key.key,
            sub_account_key.secret,
            uid,
            config.symbols.clone(),
            config.trade_settings.order_size,
            Some(cache.clone()),
            trade_settings,
            false,        // save_metrics
            true,         // update_futures_account_data
            health_check, // run_health_check
            print_orders, // print_orders (gate_order_manager WS order/trade/position logs)
            login_status_url_template,
            Some(order_manager_client.clone()),
            run_small_orders_loop,
            true,
            None,
        ));

        let is_logged_in = manager.check_login_status();
        if !is_logged_in {
            warn!("[Monitoring] {} not logged in", config.login_name);
            std::thread::sleep(Duration::from_millis(initial_sleep_ms));
        } else {
            info!("[Monitoring] {} logged in", config.login_name);
            std::thread::sleep(Duration::from_millis(100));
        }

        if let Err(e) = manager.update_positions() {
            error!(
                "[Monitoring] {} update_positions failed: {}",
                config.login_name, e
            );
            continue;
        }

        let positions = (**manager.positions.load()).clone();
        let existing_positions = positions
            .iter()
            .filter(|(_, p)| p.size != 0)
            .map(|(contract, _)| contract.clone())
            .collect::<Vec<String>>();
        trace!(
            "[Monitoring] {} positions: {:?}",
            config.login_name,
            positions
        );
        info!(
            "[Monitoring] {} existing positions: {:?}",
            config.login_name, existing_positions
        );

        for contract in existing_positions {
            if !all_symbols.contains(&contract) {
                all_symbols.push(contract.clone());
            }
        }
        std::thread::sleep(Duration::from_millis(initial_sleep_ms));

        manager.start_update_loop();

        // if let Some(ref sm) = state_manager {
        //     start_state_sync(sm.clone(), manager.clone());
        // }

        gate_order_managers.insert(config.login_name.clone(), manager);
    }

    if gate_order_managers.is_empty() {
        return Err(anyhow::anyhow!(
            "[Monitoring] No valid credentials for any login_name"
        ));
    }

    let all_symbols_shared = Arc::new(RwLock::new(all_symbols));
    let initial_symbols = all_symbols_shared.read().clone();

    let base_data_url = match base_exchange.to_lowercase().as_str() {
        "binance" => std::env::var("BINANCE_DATA_URL").ok(),
        "bitget" => std::env::var("BITGET_DATA_URL").ok(),
        "bybit" => std::env::var("BYBIT_DATA_URL").ok(),
        "okx" => std::env::var("OKX_DATA_URL").ok(),
        _ => None,
    };
    let data_client: Arc<dyn DataClient> = if let Ok(gate_data_url) = std::env::var("GATE_DATA_URL")
    {
        info!(
            "[Monitoring] ZMQ data client: url={}, base({}): {:?}, symbols={}",
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
            "[Monitoring] IPC data client: socket={}, symbols={}",
            socket_path,
            initial_symbols.len()
        );
        Arc::new(ipc_client::IpcClient::new(
            socket_path,
            initial_symbols,
            cache.clone(),
        ))
    };

    let refresh_secs = std::env::var("SUPABASE_REFRESH_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(3600);
    let configs_shared_for_refresh = configs_shared.clone();
    let all_symbols_shared_for_refresh = all_symbols_shared.clone();
    let data_client_for_refresh = data_client.clone();
    let gate_order_managers_for_refresh = gate_order_managers.clone();
    let login_names_for_refresh = login_names.clone();
    let cli_repeat = cli_repeat_profitable_order;
    let cli_use_few = cli_use_few_orders_closing_trigger;
    std::thread::spawn(move || {
        let config_manager = config::ConfigManager::new();
        loop {
            std::thread::sleep(Duration::from_secs(refresh_secs));
            let new_configs =
                match config_manager.load_strategy_configs(login_names_for_refresh.clone()) {
                    Ok(cfgs) if !cfgs.is_empty() => cfgs,
                    Ok(_) => {
                        warn!("[Monitoring] Supabase refresh returned empty configs");
                        continue;
                    }
                    Err(e) => {
                        warn!("[Monitoring] Supabase refresh failed: {}", e);
                        continue;
                    }
                };
            let new_configs: Vec<config::StrategyConfig> = new_configs
                .into_iter()
                .map(|mut c| {
                    c.trade_settings = apply_cli_trade_overrides(
                        c.trade_settings.clone(),
                        cli_repeat,
                        cli_use_few,
                    );
                    c
                })
                .collect();
            let changed = {
                let current = configs_shared_for_refresh.read();
                **current != new_configs
            };
            if !changed {
                continue;
            }
            info!(
                "[Monitoring] Supabase configs updated ({} accounts)",
                new_configs.len()
            );
            {
                let mut guard = configs_shared_for_refresh.write();
                *guard = Arc::new(new_configs.clone());
            }
            let mut next_symbols = collect_symbols(&new_configs);
            for config in &new_configs {
                if let Some(manager) = gate_order_managers_for_refresh.get(&config.login_name) {
                    let mut trade_settings = config.trade_settings.clone();
                    trade_settings.repeat_profitable_order = false;
                    manager.update_strategy_config(config.symbols.clone(), trade_settings);
                    let positions = manager.positions.load();
                    for (contract, position) in positions.iter() {
                        if position.size == 0 {
                            continue;
                        }
                        if !next_symbols.contains(contract) {
                            next_symbols.push(contract.clone());
                        }
                    }
                }
            }
            if *all_symbols_shared_for_refresh.read() != next_symbols {
                let mut guard = all_symbols_shared_for_refresh.write();
                *guard = next_symbols.clone();
                data_client_for_refresh.update_symbols(next_symbols);
                data_client_for_refresh.stop();
                if let Err(e) = data_client_for_refresh.start() {
                    warn!(
                        "[Monitoring] Failed to restart data client after refresh: {}",
                        e
                    );
                }
            }
        }
    });

    let use_market_watcher = std::env::var("USE_MARKET_WATCHER")
        .ok()
        .map(|v| v.to_ascii_lowercase())
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    let market_gap_state: market_watcher::SharedMarketGapState = Arc::new(ArcSwap::from(Arc::new(
        market_watcher::MarketGapState::new(10),
    )));
    if use_market_watcher {
        let market_watcher_update_secs = std::env::var("MARKET_WATCHER_UPDATE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(10);
        let market_watcher_window_minutes = std::env::var("MARKET_WATCHER_WINDOW_MINUTES")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(10);
        let questdb_url =
            std::env::var("QUESTDB_URL").unwrap_or_else(|_| "http://localhost:9000".to_string());
        let market_gap_redis_url = std::env::var("MARKET_GAP_REDIS_URL")
            .ok()
            .or_else(|| std::env::var("REDIS_URL").ok());
        let market_gap_redis_prefix = std::env::var("MARKET_GAP_REDIS_PREFIX")
            .unwrap_or_else(|_| "gate_hft:market_gap".to_string());
        let mw = market_watcher::MarketWatcher::new(
            questdb_url,
            market_watcher_update_secs,
            market_watcher_window_minutes,
            market_gap_state.clone(),
            "gate".to_string(),
            base_exchange.clone(),
            market_gap_redis_url,
            market_gap_redis_prefix,
        );
        mw.start();
        info!("[Monitoring] Market watcher started");
    } else {
        info!("[Monitoring] Market watcher disabled");
    }

    ctrlc::set_handler(move || {
        info!("[Monitoring] Shutting down (no order cancel/amend)");
        std::process::exit(0);
    })
    .context("Failed to set Ctrl+C handler")?;

    strategy_monitoring::run(
        data_client,
        gate_order_managers,
        monitor_interval_secs,
        print_orders,
    )
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
        let _ = state_manager
            .update_state(
                &login_name,
                is_healthy,
                too_many_request,
                num_positions,
                "".to_string(),
            )
            .ok();
        std::thread::sleep(Duration::from_secs(10));
    });
}

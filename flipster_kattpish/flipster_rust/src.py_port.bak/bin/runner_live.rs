// runner_live — full live trading runner.
//
//   api+web feeds → signal loop (rayon) → channel → entry worker (async)
//                                                  → exec log
//                         position_state ← exit monitor (async) → exec log
//
// Usage:
//   cargo run --release --bin runner_live -- --all --cdp-port 9230 --amount 10
//   cargo run --release --bin runner_live -- --symbols BTCUSDT.PERP --amount 5 --duration-min 5
//
// Safety:
//   - FLIPSTER_API_KEY / FLIPSTER_API_SECRET env vars required.
//   - Chrome with flipster.io cookies on localhost:{cdp_port}.
//   - `--kill-usd` triggers shutdown if cum PnL drops below this threshold.

use anyhow::Result;
use flipster_rust::{
    data_cache::DataCache,
    data_client::FlipsterDataClient,
    exec_log::ExecLogger,
    exit::spawn_exit_monitor,
    order_manager::FlipsterOrderManager,
    params::StrategyParams,
    position_state::PositionManager,
    runner::{spawn_entry_worker, status_logger},
    strategies::stale_maker::StaleMaker,
    strategy::{run_signal_loop, ChannelHandler, SignalLoopConfig},
    strategy_core::StrategyCore,
    symbols::SymbolRegistry,
};
use parking_lot::RwLock;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .init();

    let args: Vec<String> = env::args().collect();
    let mut cdp_port: u16 = 9230;
    let mut symbol_arg: Option<String> = None;
    let mut use_all = false;
    let mut amount_usd: f64 = 10.0;
    let mut duration_min: f64 = 0.0;
    let mut kill_usd: f64 = 50.0; // default: stop if cum PnL <= -$50
    let mut exec_log_path: String = "logs/flipster_rust_exec.jsonl".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cdp-port" => { cdp_port = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(9230); i += 2; }
            "--symbols"  => { symbol_arg = args.get(i + 1).cloned(); i += 2; }
            "--all"      => { use_all = true; i += 1; }
            "--amount"   => { amount_usd = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(amount_usd); i += 2; }
            "--duration-min" => { duration_min = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(0.0); i += 2; }
            "--kill-usd"  => { kill_usd = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(kill_usd); i += 2; }
            "--exec-log"  => { exec_log_path = args.get(i + 1).cloned().unwrap_or(exec_log_path); i += 2; }
            _ => i += 1,
        }
    }

    let api_key = env::var("FLIPSTER_API_KEY")?;
    let api_secret = env::var("FLIPSTER_API_SECRET")?;

    let mut params = StrategyParams::default();
    params.amount_usd = amount_usd;

    let reg = Arc::new(SymbolRegistry::load(symbols_path())?);
    let symbols: Vec<String> = if use_all {
        reg.perp_symbols()
    } else if let Some(s) = symbol_arg {
        s.split(',').map(|x| x.trim().to_string()).collect()
    } else {
        vec!["BTCUSDT.PERP".into(), "ETHUSDT.PERP".into()]
    };

    tracing::info!(
        "[cfg] amount=${}  rate_limit={}/min  kill=-${}  universe={}syms  exec_log={}",
        params.amount_usd, params.rate_limit_per_min, kill_usd, symbols.len(), exec_log_path,
    );

    // 1. Data feeds
    let cache = Arc::new(DataCache::new());
    let feeds = FlipsterDataClient::new(cache.clone(), api_key, api_secret, cdp_port);
    feeds.start(symbols.clone());

    // 2. Order manager (private WS + HTTP)
    let om = FlipsterOrderManager::new(reg.clone(), cdp_port).await?;

    // 3. Exec logger
    let exec_log: Option<Arc<ExecLogger>> = match ExecLogger::open(&exec_log_path) {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!("failed to open exec_log at {}: {} — continuing without JSONL log", exec_log_path, e);
            None
        }
    };

    // 4. Position manager
    let pos_mgr = Arc::new(PositionManager::new(params.rate_limit_per_min, kill_usd));

    // 5. Signal loop → channel → entry worker
    let (sig_tx, sig_rx) = crossbeam_channel::bounded(4096);
    let handler = Arc::new(ChannelHandler::new(sig_tx));
    let core: Arc<dyn StrategyCore> = Arc::new(StaleMaker::new(params.clone()));
    let cfg = SignalLoopConfig {
        symbols: Arc::new(RwLock::new(symbols)),
        interval_ms: 10,
    };
    let sig_handle = run_signal_loop(cache.clone(), core.clone(), handler, cfg);
    spawn_entry_worker(
        sig_rx,
        om.clone(),
        pos_mgr.clone(),
        cache.clone(),
        reg.clone(),
        exec_log.clone(),
        params.clone(),
    );

    // 6. Exit monitor
    spawn_exit_monitor(
        om.clone(),
        pos_mgr.clone(),
        cache.clone(),
        reg.clone(),
        exec_log.clone(),
        params.clone(),
    );

    // 7. Status logger
    tokio::spawn(status_logger(
        cache.clone(),
        pos_mgr.clone(),
        sig_handle.stats.clone(),
        30,
    ));

    // 8. Run for duration, or until kill switch / Ctrl-C.
    let pos_mgr_kill = pos_mgr.clone();
    let kill_watcher = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if pos_mgr_kill.kill_triggered() {
                tracing::error!(
                    "[KILL] cum PnL breach (kill=-${})  cum=${:+.4} — halting entries",
                    kill_usd, pos_mgr_kill.cum_pnl()
                );
                break;
            }
        }
    });

    if duration_min > 0.0 {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs_f64(duration_min * 60.0)) => {
                tracing::info!("[AUTOSTOP] {} min reached", duration_min);
            }
            _ = tokio::signal::ctrl_c() => { tracing::info!("[ctrl-c] stopping"); }
            _ = kill_watcher => {}
        }
    } else {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => { tracing::info!("[ctrl-c] stopping"); }
            _ = kill_watcher => {}
        }
    }

    sig_handle.stop();
    Ok(())
}

fn symbols_path() -> PathBuf {
    if let Ok(p) = env::var("SYMBOLS_JSON") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("scripts").join("symbols_v9.json"))
        .unwrap_or_else(|| PathBuf::from("scripts/symbols_v9.json"))
}

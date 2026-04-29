// runner_dryrun — connects feeds + runs signal loop + logs signals (no trading).
// Usage:
//   cargo run --release --bin runner_dryrun -- --all --cdp-port 9230

use anyhow::Result;
use flipster_rust::{
    data_cache::DataCache,
    data_client::FlipsterDataClient,
    params::StrategyParams,
    strategies::stale_maker::StaleMaker,
    strategy::{run_signal_loop, LoggingHandler, SignalLoopConfig},
    strategy_core::StrategyCore,
    symbols::SymbolRegistry,
};
use parking_lot::RwLock;
use std::env;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
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
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cdp-port" => {
                cdp_port = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(9230);
                i += 2;
            }
            "--symbols" => {
                symbol_arg = args.get(i + 1).cloned();
                i += 2;
            }
            "--all" => {
                use_all = true;
                i += 1;
            }
            _ => i += 1,
        }
    }

    let api_key = env::var("FLIPSTER_API_KEY")?;
    let api_secret = env::var("FLIPSTER_API_SECRET")?;

    let reg = Arc::new(SymbolRegistry::load(symbols_path())?);
    let symbols: Vec<String> = if use_all {
        reg.perp_symbols()
    } else if let Some(s) = symbol_arg {
        s.split(',').map(|x| x.trim().to_string()).collect()
    } else {
        vec!["BTCUSDT.PERP".into(), "ETHUSDT.PERP".into()]
    };
    tracing::info!("universe: {} symbols", symbols.len());

    let cache = Arc::new(DataCache::new());
    let feeds = FlipsterDataClient::new(cache.clone(), api_key, api_secret, cdp_port);
    feeds.start(symbols.clone());

    let core: Arc<dyn StrategyCore> = Arc::new(StaleMaker::new(StrategyParams::default()));
    let handler = Arc::new(LoggingHandler::default());
    let cfg = SignalLoopConfig {
        symbols: Arc::new(RwLock::new(symbols)),
        interval_ms: 10,
    };
    let handle = run_signal_loop(cache.clone(), core.clone(), handler, cfg);

    let stats = handle.stats.clone();
    let start = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let t = start.elapsed().as_secs();
        let ticks = stats.ticks.load(Ordering::Relaxed);
        let sigs = stats.signals.load(Ordering::Relaxed);
        let (api_n, web_n) = cache.counts();
        tracing::info!(
            "[status t={}s] api={} web={} ticks={} sigs={} skip(edge={} sp={} age={} stale={} no_cross={} overshoot={} other={})",
            t, api_n, web_n, ticks, sigs,
            stats.skips_below_edge.load(Ordering::Relaxed),
            stats.skips_wide_spread.load(Ordering::Relaxed),
            stats.skips_web_age.load(Ordering::Relaxed),
            stats.skips_orderbook_stale.load(Ordering::Relaxed),
            stats.skips_no_cross.load(Ordering::Relaxed),
            stats.skips_overshoot.load(Ordering::Relaxed),
            stats.skips_other.load(Ordering::Relaxed),
        );
    }
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

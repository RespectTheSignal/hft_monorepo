// Minimal binary: connects to both Flipster feeds and prints per-symbol tick counts every 10s.
// Usage:
//   FLIPSTER_API_KEY=... FLIPSTER_API_SECRET=... \
//     cargo run --release --bin probe -- --symbols BTCUSDT.PERP,ETHUSDT.PERP --cdp-port 9230
// Or:
//   cargo run --release --bin probe -- --all --cdp-port 9230

use anyhow::Result;
use flipster_rust::{data_cache::DataCache, data_client::FlipsterDataClient, symbols::SymbolRegistry};
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
            _ => {
                i += 1;
            }
        }
    }

    let api_key = env::var("FLIPSTER_API_KEY")?;
    let api_secret = env::var("FLIPSTER_API_SECRET")?;

    let symbols: Vec<String> = if use_all {
        let path = symbols_path();
        let reg = SymbolRegistry::load(&path)?;
        tracing::info!("loaded {} symbols from {}", reg.len(), path.display());
        reg.perp_symbols()
    } else if let Some(s) = symbol_arg {
        s.split(',').map(|x| x.trim().to_string()).collect()
    } else {
        vec!["BTCUSDT.PERP".into(), "ETHUSDT.PERP".into()]
    };

    let cache = Arc::new(DataCache::new());
    let client = FlipsterDataClient::new(cache.clone(), api_key, api_secret, cdp_port);
    client.start(symbols.clone());

    tracing::info!("subscribing {} symbols, cdp_port={}", symbols.len(), cdp_port);

    let mut interval = tokio::time::interval(Duration::from_secs(10));
    let start = std::time::Instant::now();
    loop {
        interval.tick().await;
        let (a, w) = cache.counts();
        let elapsed = start.elapsed().as_secs();
        tracing::info!("t={}s  api_syms={}  web_syms={}", elapsed, a, w);

        // Sample a few symbols to show current state
        for sym in symbols.iter().take(3) {
            if let (Some(api), Some(web)) = (cache.get_api_bookticker(sym), cache.get_web_bookticker(sym)) {
                let diff_bp = (api.mid() - web.mid()) / web.mid() * 1.0e4;
                tracing::info!(
                    "  {}  api_mid={:.6}  web_mid={:.6}  diff={:+.2}bp  api_sp={:.2}bp  web_sp={:.2}bp",
                    sym, api.mid(), web.mid(), diff_bp, api.spread_bp(), web.spread_bp(),
                );
            }
        }
    }
}

fn symbols_path() -> PathBuf {
    if let Ok(p) = env::var("SYMBOLS_JSON") {
        return PathBuf::from(p);
    }
    // default: ../scripts/symbols_v9.json relative to crate
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("scripts").join("symbols_v9.json"))
        .unwrap_or_else(|| PathBuf::from("scripts/symbols_v9.json"))
}

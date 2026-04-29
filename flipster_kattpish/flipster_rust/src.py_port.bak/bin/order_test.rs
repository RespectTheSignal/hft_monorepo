// order_test — places a tiny post-only LIMIT BUY at 50% below mid for BTC,
// waits 3s, then cancels. Verifies the full submit→WS-observe→cancel path works.
// USAGE:
//   FLIPSTER_API_KEY=... FLIPSTER_API_SECRET=... \
//     cargo run --release --bin order_test -- --symbol BTCUSDT.PERP --cdp-port 9230
//
// WARNING: places a real order on Flipster (far below market, should not fill).

use anyhow::{anyhow, Result};
use flipster_rust::{
    data_cache::DataCache, data_client::FlipsterDataClient,
    order_manager::{FlipsterOrderManager, OrderRequest, OrderType},
    symbols::SymbolRegistry, types::Side,
};
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
    let mut symbol = "BTCUSDT.PERP".to_string();
    let mut amount_usd: f64 = 2.0;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cdp-port" => {
                cdp_port = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(9230);
                i += 2;
            }
            "--symbol" => {
                symbol = args.get(i + 1).cloned().unwrap_or(symbol);
                i += 2;
            }
            "--amount" => {
                amount_usd = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(amount_usd);
                i += 2;
            }
            _ => i += 1,
        }
    }

    let api_key = env::var("FLIPSTER_API_KEY")?;
    let api_secret = env::var("FLIPSTER_API_SECRET")?;

    let reg = Arc::new(SymbolRegistry::load(symbols_path())?);

    let cache = Arc::new(DataCache::new());
    let feeds = FlipsterDataClient::new(cache.clone(), api_key, api_secret, cdp_port);
    feeds.start(vec![symbol.clone()]);

    // Wait for quotes
    tracing::info!("waiting for quotes…");
    let mut mid = 0.0;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Some(api) = cache.get_api_bookticker(&symbol) {
            mid = api.mid();
            if mid > 0.0 {
                break;
            }
        }
    }
    if mid <= 0.0 {
        return Err(anyhow!("no quote received in 10s"));
    }
    tracing::info!("{} mid = {:.4}", symbol, mid);

    // Order manager (starts private WS)
    let om = FlipsterOrderManager::new(reg.clone(), cdp_port).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Post LIMIT BUY at 95% of mid (won't fill but within Flipster's deviation limit).
    let raw_price = mid * 0.95;
    let tick = reg.tick(&symbol);
    let limit_price = flipster_rust::symbols::tick_round(tick, raw_price, "down")
        .unwrap_or(raw_price);
    let leverage = reg.max_leverage(&symbol);
    let req = OrderRequest {
        symbol: symbol.clone(),
        side: Side::Buy,
        price: limit_price,
        amount_usd,
        order_type: OrderType::Limit,
        post_only: true,
        reduce_only: false,
        leverage,
    };
    tracing::info!(
        "submitting LIMIT BUY {} @ {:.6} amount=${:.2} lev={}",
        symbol, limit_price, amount_usd, leverage
    );
    let resp = om.submit_order(&req).await?;
    tracing::info!(
        "response: order_id={:?} filled={} avg={:.6}",
        resp.order_id, resp.filled_size, resp.avg_fill
    );
    let oid = resp.order_id.clone().ok_or_else(|| anyhow!("no order_id in response"))?;

    // Observe via private WS
    for i in 0..6 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let orders = om.state().orders.read();
        let has = orders.contains_key(&oid);
        tracing::info!("  {}ms: private WS sees our order = {}", (i + 1) * 500, has);
        if has {
            if let Some(o) = orders.get(&oid) {
                tracing::info!(
                    "    status={:?} price={:?} amount={:?}",
                    o.status(), o.price(), o.amount()
                );
            }
            break;
        }
    }

    tokio::time::sleep(Duration::from_secs(2)).await;

    tracing::info!("cancelling {}", oid);
    om.cancel_order(&symbol, &oid).await?;
    tracing::info!("cancel ok");

    tokio::time::sleep(Duration::from_secs(2)).await;
    let still = om.state().orders.read().contains_key(&oid);
    tracing::info!("after cancel: still in private WS = {}", still);

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

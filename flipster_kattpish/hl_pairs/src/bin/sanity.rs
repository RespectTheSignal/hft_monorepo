//! HL agent wallet sanity check.
//!
//! Reads HLP_AGENT_KEY (set HLP_TESTNET=0 for mainnet), connects, places a
//! tiny BTC bid 10% below current mid (= guaranteed maker, can't accidentally
//! fill), then cancels. Verifies sign / order / cancel pipeline.
//!
//! Usage:
//!   HLP_AGENT_KEY=0x... HLP_TESTNET=0 ./target/release/sanity

use anyhow::{Context, Result};
use hl_pairs::exec::{agent_address, LiveCfg, LiveExec};
use hyperliquid_rust_sdk::{BaseUrl, InfoClient};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hl_pairs=info")),
        )
        .init();

    let cfg = LiveCfg::from_env().context("HLP_AGENT_KEY not set in env / .env")?;
    let net = if cfg.testnet { "TESTNET" } else { "MAINNET" };
    println!("=== HL sanity check ({}) ===", net);
    println!("agent address: {}", agent_address(&cfg.agent_key)?);

    let exec = LiveExec::new(cfg.clone()).await.context("LiveExec::new")?;
    println!("✓ ExchangeClient connected");

    // Get current BTC mid via Info channel
    let info = InfoClient::new(None, Some(if cfg.testnet { BaseUrl::Testnet } else { BaseUrl::Mainnet }))
        .await
        .context("InfoClient::new")?;
    let mids = info.all_mids().await.context("all_mids")?;
    let btc_mid: f64 = mids
        .get("BTC")
        .context("no BTC mid")?
        .parse()
        .context("parse BTC mid")?;
    println!("✓ BTC mid = ${btc_mid:.2}");

    // Bid 10% below mid → guaranteed maker, can't accidentally take.
    // Round to nearest dollar (HL BTC tick = $1).
    let bid_px = (btc_mid * 0.90).round();
    // HL minimum order value = $10. Use $12 with cushion.
    let sz = (12.0 / bid_px * 100000.0).ceil() / 100000.0; // 5dp, ceil to clear min
    println!("→ placing Alo bid: {sz} BTC @ ${bid_px}  (= ${:.2} notional)", sz * bid_px);

    let oid = exec
        .place_alo("BTC", true, bid_px, sz, false)
        .await
        .context("place_alo")?;
    println!("✓ order placed, oid={oid}");

    println!("→ sleeping 3s before cancel…");
    tokio::time::sleep(Duration::from_secs(3)).await;

    exec.cancel("BTC", oid).await.context("cancel")?;
    println!("✓ order cancelled");
    println!("=== sanity OK — pipeline verified ===");
    Ok(())
}

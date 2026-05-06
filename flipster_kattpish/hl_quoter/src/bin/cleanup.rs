//! Cancel all open orders + close all positions on master via agent.

use anyhow::{Context, Result};
use hl_pairs::exec::{LiveCfg, LiveExec};
use hyperliquid_rust_sdk::{BaseUrl, InfoClient};
use std::str::FromStr;
use std::time::Duration;
use ethers::types::H160;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
    ).init();

    let cfg = LiveCfg::from_env().context("HLP_AGENT_KEY missing")?;
    let master = std::env::var("HLP_MASTER_ADDRESS").context("HLP_MASTER_ADDRESS")?;
    let master_addr = H160::from_str(master.trim_start_matches("0x"))?;
    let exec = LiveExec::new(cfg.clone()).await?;
    let info = InfoClient::new(None, Some(if cfg.testnet { BaseUrl::Testnet } else { BaseUrl::Mainnet })).await?;

    // 1) Cancel all open orders
    let orders = info.open_orders(master_addr).await?;
    println!("=== {} open orders ===", orders.len());
    for o in &orders {
        match exec.cancel(&o.coin, o.oid).await {
            Ok(()) => println!("  cancel {} oid={} ok", o.coin, o.oid),
            Err(e) => println!("  cancel {} oid={} err: {}", o.coin, o.oid, e),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 2) Close all positions via IOC reduce-only
    let state = info.user_state(master_addr).await?;
    let mids = info.all_mids().await?;
    println!("\n=== {} open positions ===", state.asset_positions.len());
    for ap in &state.asset_positions {
        let szi: f64 = ap.position.szi.parse().unwrap_or(0.0);
        if szi == 0.0 { continue; }
        let coin = ap.position.coin.clone();
        let mid: f64 = mids.get(&coin).and_then(|s| s.parse().ok()).unwrap_or(0.0);
        if mid <= 0.0 { println!("  {} no mid, skip", coin); continue; }
        let close_buy = szi < 0.0;  // short → buy to close
        let cushion = 0.01;          // 100 bp aggressive to ensure fill
        let limit_px = if close_buy { mid * (1.0 + cushion) } else { mid * (1.0 - cushion) };
        let limit_px = (limit_px * 10000.0).round() / 10000.0;
        let close_sz = szi.abs();
        match exec.place_ioc(&coin, close_buy, limit_px, close_sz, true).await {
            Ok((fpx, fsz)) => println!("  close {} {} sz={} fill_px={}", coin, if close_buy {"BUY"} else {"SELL"}, fsz, fpx),
            Err(e) => println!("  close {} fail: {}", coin, e),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("\n=== done ===");
    Ok(())
}

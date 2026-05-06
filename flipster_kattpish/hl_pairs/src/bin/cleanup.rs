//! Emergency cleanup: cancel ALL open orders + market-close ALL open positions
//! on the master HL account. Uses agent wallet (HLP_AGENT_KEY).
//!
//! Usage:
//!   HLP_AGENT_KEY=0x... HLP_TESTNET=0 \
//!   HLP_MASTER_ADDRESS=0x8973AF... \
//!   ./target/release/cleanup

use anyhow::{Context, Result};
use hl_pairs::exec::{LiveCfg, LiveExec};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hl_pairs=info")),
        )
        .init();

    let cfg = LiveCfg::from_env().context("HLP_AGENT_KEY missing")?;
    let master = std::env::var("HLP_MASTER_ADDRESS")
        .context("HLP_MASTER_ADDRESS env missing — needed to query positions/orders")?;
    println!("=== HL cleanup (master={}) ===", master);
    let exec = LiveExec::new(cfg.clone()).await?;

    let info_url = if cfg.testnet {
        "https://api.hyperliquid-testnet.xyz/info"
    } else {
        "https://api.hyperliquid.xyz/info"
    };
    let client = reqwest::Client::new();

    // 1) Cancel all open orders
    let body = serde_json::json!({"type":"openOrders","user":master});
    let v: serde_json::Value = client.post(info_url).json(&body).send().await?.json().await?;
    let orders = v.as_array().cloned().unwrap_or_default();
    println!("\n--- cancelling {} open orders ---", orders.len());
    let mut cancel_ok = 0;
    for o in &orders {
        let coin = o.get("coin").and_then(|x| x.as_str()).unwrap_or("");
        let oid = o.get("oid").and_then(|x| x.as_u64()).unwrap_or(0);
        if coin.is_empty() || oid == 0 {
            continue;
        }
        match exec.cancel(coin, oid).await {
            Ok(()) => {
                println!("  ✓ cancel {} oid={}", coin, oid);
                cancel_ok += 1;
            }
            Err(e) => println!("  ✗ cancel {} oid={}: {}", coin, oid, e),
        }
    }
    println!("--- cancelled {}/{} ---", cancel_ok, orders.len());

    // 2) Close all open positions via IOC reduce_only
    let body = serde_json::json!({"type":"clearinghouseState","user":master});
    let v: serde_json::Value = client.post(info_url).json(&body).send().await?.json().await?;
    let positions = v
        .get("assetPositions")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let mut to_close: Vec<(String, f64)> = Vec::new();
    for ap in positions {
        if let Some(p) = ap.get("position") {
            let coin = p.get("coin").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let szi: f64 = p
                .get("szi")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            if szi != 0.0 && !coin.is_empty() {
                to_close.push((coin, szi));
            }
        }
    }
    println!("\n--- closing {} open positions ---", to_close.len());
    // Pull all_mids for ref price
    let body = serde_json::json!({"type":"allMids"});
    let v: serde_json::Value = client.post(info_url).json(&body).send().await?.json().await?;
    let mids = v.as_object().cloned().unwrap_or_default();

    let mut close_ok = 0;
    for (coin, szi) in &to_close {
        let mid: f64 = mids
            .get(coin)
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        if mid <= 0.0 {
            println!("  ✗ {} no mid → skip", coin);
            continue;
        }
        let is_buy = *szi < 0.0; // szi<0 (short) → BUY to close; szi>0 (long) → SELL
        let cushion = 0.01; // 100bp aggressive cushion to ensure fill
        let limit_px = if is_buy { mid * (1.0 + cushion) } else { mid * (1.0 - cushion) };
        let limit_px = (limit_px * 10000.0).round() / 10000.0;
        // Use exact abs(szi) — HL already gave us a valid-precision number.
        // Don't re-round (over-rounding flips the order to "increase position").
        let sz = szi.abs();
        match exec
            .place_ioc(coin, is_buy, limit_px, sz, /*reduce_only=*/true)
            .await
        {
            Ok((fpx, fsz)) => {
                println!(
                    "  ✓ close {} {} sz={:.4} fill_px={}",
                    coin,
                    if is_buy { "BUY" } else { "SELL" },
                    fsz,
                    fpx
                );
                close_ok += 1;
            }
            Err(e) => println!("  ✗ close {}: {}", coin, e),
        }
    }
    println!("--- closed {}/{} ---", close_ok, to_close.len());
    println!("=== cleanup DONE ===");
    Ok(())
}

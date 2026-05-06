//! One-shot close for a specific position. Args: COIN SIZE BUY|SELL [px_round]
//! Example: ./close_one DASH 0.53 SELL 0.01
use anyhow::{Context, Result};
use ethers::types::H160;
use hl_pairs::exec::{LiveCfg, LiveExec};
use hyperliquid_rust_sdk::{BaseUrl, InfoClient};
use std::env;
use std::str::FromStr;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt().init();
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 { anyhow::bail!("usage: close_one COIN SIZE BUY|SELL [tick=0.01]"); }
    let coin = args[1].clone();
    let sz: f64 = args[2].parse()?;
    let is_buy = args[3].eq_ignore_ascii_case("BUY");
    let tick: f64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0.01);

    let cfg = LiveCfg::from_env().context("HLP_AGENT_KEY")?;
    let exec = LiveExec::new(cfg.clone()).await?;
    let info = InfoClient::new(None, Some(if cfg.testnet { BaseUrl::Testnet } else { BaseUrl::Mainnet })).await?;
    let mids = info.all_mids().await?;
    let mid: f64 = mids.get(&coin).context("no mid")?.parse()?;
    let px_raw = if is_buy { mid * 1.005 } else { mid * 0.995 };
    let px = (px_raw / tick).round() * tick;
    println!("close {coin} {} sz={sz} mid={mid} px={px} tick={tick}", if is_buy {"BUY"} else {"SELL"});
    let (fpx, fsz) = exec.place_ioc(&coin, is_buy, px, sz, true).await?;
    println!("filled: px={fpx} sz={fsz}");
    Ok(())
}

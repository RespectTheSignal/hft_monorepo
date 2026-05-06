//! Standalone CLI to send a single BingX trade through the same code path
//! the live executor will use. Tiny-size test harness — sends 0.0001 BTC
//! and prints the response. Use to confirm the Rust client works against
//! the live BingX endpoint before wiring into the main executor.
//!
//! Usage:
//!   target/release/bingx_test open  Bid 0.0001 81000   # market open long
//!   target/release/bingx_test open  Ask 0.0001 81000   # market open short
//!   target/release/bingx_test close <position_id> Bid 81000  # close
//!
//! Reads JWT from BINGX_JWT env var, userId from BINGX_USER_ID. cookies
//! from ~/.config/flipster_kattpish/cookies.json (optional).

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use anyhow::{anyhow, Result};

#[path = "../bingx_helpers.rs"]
mod bingx_helpers;

use bingx_helpers::{BingxClient, BingxIdentity, Side};

fn load_bingx_cookies() -> HashMap<String, String> {
    let p = env::var("HOME")
        .map(|h| PathBuf::from(h).join(".config/flipster_kattpish/cookies.json"))
        .ok();
    let Some(p) = p else { return HashMap::new(); };
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return HashMap::new();
    };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let Some(o) = v.get("bingx").and_then(|x| x.as_object()) else {
        return HashMap::new();
    };
    o.iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    let user_id = env::var("BINGX_USER_ID")
        .map_err(|_| anyhow!("set BINGX_USER_ID env var (e.g. 1507939247354732548)"))?;
    let jwt =
        env::var("BINGX_JWT").map_err(|_| anyhow!("set BINGX_JWT env var (Bearer token)"))?;
    let id = BingxIdentity::defaults_with_jwt(user_id, jwt);
    let cookies = load_bingx_cookies();
    let client = BingxClient::new(id, cookies)?;

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage:\n  bingx_test open  <Bid|Ask> <notional_usdt> <last_price>\n  \
             bingx_test close <position_id> <Bid|Ask> <last_price>"
        );
        std::process::exit(2);
    }
    let cmd = args[1].as_str();
    match cmd {
        "open" => {
            let side = match args.get(2).map(String::as_str) {
                Some("Bid") => Side::Bid,
                Some("Ask") => Side::Ask,
                other => return Err(anyhow!("bad side {:?}", other)),
            };
            let notional: f64 = args[3].parse()?;
            let last: f64 = args[4].parse()?;
            let resp = client
                .place_market_open("BTC-USDT", side, notional, last)
                .await?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
        "close" => {
            let pos_id = args[2].clone();
            let side = match args.get(3).map(String::as_str) {
                Some("Bid") => Side::Bid,
                Some("Ask") => Side::Ask,
                other => return Err(anyhow!("bad side {:?}", other)),
            };
            let last: f64 = args[4].parse()?;
            let resp = client
                .close_market_full("BTC-USDT", side, Some(&pos_id), last)
                .await?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
        "list" => {
            let resp = client.list_positions().await?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
        }
        other => return Err(anyhow!("unknown cmd {other}")),
    }
    Ok(())
}

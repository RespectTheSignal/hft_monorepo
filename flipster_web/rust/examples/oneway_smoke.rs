//! Validate the new `place_order_oneway` + `cancel_order` paths.
//!
//! Strategy: place a tiny postOnly LIMIT BUY on BTCUSDT.PERP at $1
//! (guaranteed not to cross). Confirm we get an `orderId`, then cancel it.
//! No position is ever taken — postOnly + far-from-market price ensures
//! the order sits, then we DELETE it.
//!
//! Run on teamreporter:
//!     /tmp/oneway_smoke ~/.config/flipster_kattpish/cookies.json

use std::collections::HashMap;
use std::env;
use std::fs;
use std::process::ExitCode;

use flipster_client::FlipsterClient;

#[tokio::main]
async fn main() -> ExitCode {
    let path = env::args().nth(1).unwrap_or_else(|| {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.config/flipster_kattpish/cookies.json")
    });

    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => { eprintln!("[smoke] read {path}: {e}"); return ExitCode::from(1); }
    };
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("invalid JSON");
    let flip_obj = parsed.get("flipster").and_then(|v| v.as_object()).expect("no .flipster");
    let cookies: HashMap<String, String> = flip_obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect();
    println!("[smoke] loaded {} flipster cookies", cookies.len());

    let client = FlipsterClient::from_all_cookies(&cookies);

    // Far-below-market postOnly LIMIT BUY — won't cross, won't fill.
    print!("[smoke] place_order_oneway BTCUSDT.PERP postOnly LIMIT BUY @ $1 ... ");
    let resp = match client
        .place_order_oneway(
            "BTCUSDT.PERP",
            "long",
            5.0,        // $5 notional
            1.0,        // way below market
            10,
            false,
            "ORDER_TYPE_LIMIT",
            true,       // postOnly
        )
        .await
    {
        Ok(r) => r,
        Err(e) => { println!("FAIL: {e}"); return ExitCode::from(2); }
    };
    let order_id = resp
        .get("order")
        .and_then(|o| o.get("orderId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    match &order_id {
        Some(id) => println!("OK orderId={id}"),
        None => {
            println!("FAIL — no orderId in response: {}",
                serde_json::to_string(&resp).unwrap_or_default().chars().take(400).collect::<String>());
            return ExitCode::from(3);
        }
    }
    let order_id = order_id.unwrap();

    print!("[smoke] cancel_order ... ");
    match client.cancel_order("BTCUSDT.PERP", &order_id).await {
        Ok(r) => println!("OK ({} bytes)", serde_json::to_string(&r).unwrap_or_default().len()),
        Err(e) => { println!("FAIL: {e}"); return ExitCode::from(4); }
    }

    println!("[smoke] all checks passed");
    ExitCode::from(0)
}

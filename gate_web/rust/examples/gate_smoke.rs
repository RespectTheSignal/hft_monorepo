//! Smoke test: load cookies.json, instantiate GateClient, hit `accounts`
//! and `positions` endpoints. Validates that the cookie ingestion path
//! (Python dump_cookies → JSON file → Rust loader → reqwest) produces
//! authenticated requests.
//!
//! Run on teamreporter (the only host whose IP isn't CF-blacklisted).
//!
//! Usage:
//!     cargo run --release --example gate_smoke -- ~/.config/flipster_kattpish/cookies.json

use std::collections::HashMap;
use std::env;
use std::fs;
use std::process::ExitCode;

use gate_client::GateClient;

#[tokio::main]
async fn main() -> ExitCode {
    let path = env::args().nth(1).unwrap_or_else(|| {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.config/flipster_kattpish/cookies.json")
    });

    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[gate_smoke] failed to read {path}: {e}");
            return ExitCode::from(1);
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[gate_smoke] invalid JSON: {e}");
            return ExitCode::from(1);
        }
    };
    let gate_obj = match parsed.get("gate").and_then(|v| v.as_object()) {
        Some(o) => o,
        None => {
            eprintln!("[gate_smoke] cookies.json has no .gate object");
            return ExitCode::from(1);
        }
    };
    let cookies: HashMap<String, String> = gate_obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect();
    println!("[gate_smoke] loaded {} gate cookies from {path}", cookies.len());

    let client = match GateClient::from_cookies(&cookies) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[gate_smoke] client build failed: {e}");
            return ExitCode::from(1);
        }
    };

    print!("[gate_smoke] check_login... ");
    match client.check_login().await {
        Ok(v) => {
            let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            let msg = v.get("message").and_then(|c| c.as_str()).unwrap_or("");
            if code == 0 && msg.eq_ignore_ascii_case("success") {
                println!("OK code={code}");
            } else {
                println!("FAIL code={code} msg={msg} body={}",
                    serde_json::to_string(&v).unwrap_or_default().chars().take(300).collect::<String>());
                return ExitCode::from(2);
            }
        }
        Err(e) => {
            println!("FAIL: {e}");
            return ExitCode::from(2);
        }
    }

    print!("[gate_smoke] accounts... ");
    match client.accounts().await {
        Ok(v) => {
            let total = v
                .get("data")
                .and_then(|d| d.as_array())
                .and_then(|a| a.first())
                .and_then(|a| a.get("total"))
                .and_then(|t| t.as_str())
                .unwrap_or("?");
            println!("OK total=${total}");
        }
        Err(e) => {
            println!("FAIL: {e}");
            return ExitCode::from(3);
        }
    }

    print!("[gate_smoke] positions... ");
    match client.positions().await {
        Ok(v) => {
            let n = v
                .get("data")
                .and_then(|d| d.as_array())
                .map(|a| {
                    a.iter()
                        .filter(|p| {
                            p.get("size")
                                .and_then(|s| s.as_str())
                                .and_then(|s| s.parse::<f64>().ok())
                                .map(|s| s != 0.0)
                                .unwrap_or(false)
                        })
                        .count()
                })
                .unwrap_or(0);
            println!("OK open={n}");
        }
        Err(e) => {
            println!("FAIL: {e}");
            return ExitCode::from(4);
        }
    }

    println!("[gate_smoke] all checks passed");
    ExitCode::from(0)
}

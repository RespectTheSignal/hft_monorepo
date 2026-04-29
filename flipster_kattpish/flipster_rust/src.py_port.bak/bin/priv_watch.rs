// priv_watch — connects to Flipster private WS and prints current positions / margin every 5s.
// Read-only, safe. Usage: cargo run --release --bin priv_watch -- --cdp-port 9230

use anyhow::Result;
use flipster_rust::{
    cookies::fetch_flipster_cookies, private_ws::{PrivateWsClient, PrivateWsState},
};
use std::env;
use std::sync::Arc;
use std::time::Duration;
use arc_swap::ArcSwap;
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
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cdp-port" => {
                cdp_port = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(9230);
                i += 2;
            }
            _ => i += 1,
        }
    }

    let cookies = fetch_flipster_cookies(cdp_port).await?;
    tracing::info!("fetched {} cookies", cookies.len());
    let cookies = Arc::new(ArcSwap::from_pointee(cookies));
    let state = PrivateWsState::new();
    let client = Arc::new(PrivateWsClient::new(state.clone(), cookies));
    client.spawn();

    let start = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let elapsed = start.elapsed().as_secs();
        let pos_count = state.positions.read().len();
        let open_count = state
            .positions
            .read()
            .values()
            .filter(|p| p.is_open())
            .count();
        let ord_count = state.orders.read().len();
        let margin = state.margin.load();
        tracing::info!(
            "t={}s  connected={}  pos={} (open={})  orders={}  margin_avail=${:.2}  balance=${:.2}",
            elapsed,
            state.is_connected(),
            pos_count,
            open_count,
            ord_count,
            margin.available,
            margin.balance,
        );
        for (k, p) in state.positions.read().iter().take(5) {
            if !p.is_open() {
                continue;
            }
            if let Some((side, qty, entry)) = p.side_qty_entry() {
                tracing::info!("  {:<20} {:<5} qty={:.6} entry={:.6}", k, side, qty, entry);
            }
        }
    }
}

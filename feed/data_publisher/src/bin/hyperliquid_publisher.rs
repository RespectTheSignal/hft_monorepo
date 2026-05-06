//! Hyperliquid bookticker publisher → QuestDB `hyperliquid_bookticker`.

use data_publisher::error::Result;
use data_publisher::exchanges::common::{install_tracing, PublisherConfig};
use data_publisher::exchanges::hyperliquid;
use tracing::info;

const TABLE: &str = "hyperliquid_bookticker";

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();
    let cfg = PublisherConfig::from_env(50);
    info!("config: {:?}", cfg);

    let writer = cfg.writer(TABLE)?;
    let http = reqwest::Client::builder()
        .user_agent("data_publisher/hyperliquid")
        .build()
        .expect("reqwest client");

    let mut coins = hyperliquid::fetch_active_coins(&http).await?;
    info!("HL: discovered {} active perp coins", coins.len());
    if let Some(n) = cfg.max_symbols {
        coins.truncate(n);
        info!("limited to {n}");
    }

    hyperliquid::spawn_all(hyperliquid::WS_URL, coins, cfg.topics_per_conn, writer);

    tokio::signal::ctrl_c().await.ok();
    info!("shutdown");
    Ok(())
}

//! Pyth Network fair-price publisher → QuestDB `pyth_bookticker`.

use data_publisher::error::Result;
use data_publisher::exchanges::common::{install_tracing, PublisherConfig};
use data_publisher::exchanges::pyth;
use tracing::info;

const TABLE: &str = "pyth_bookticker";

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();
    let cfg = PublisherConfig::from_env(200);
    info!("config: {:?}", cfg);

    let writer = cfg.writer(TABLE)?;
    let http = reqwest::Client::builder()
        .user_agent("data_publisher/pyth")
        .build()
        .expect("reqwest client");

    let mut feeds = pyth::fetch_usd_crypto_feeds(&http).await?;
    info!("Pyth: discovered {} USD crypto feeds", feeds.len());
    if let Some(n) = cfg.max_symbols {
        feeds.truncate(n);
        info!("limited to {n}");
    }

    pyth::spawn_all(feeds, cfg.topics_per_conn, writer);
    tokio::signal::ctrl_c().await.ok();
    info!("shutdown");
    Ok(())
}

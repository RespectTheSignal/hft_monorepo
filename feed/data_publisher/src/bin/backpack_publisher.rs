//! Backpack bookticker publisher → QuestDB `backpack_bookticker`.

use data_publisher::error::Result;
use data_publisher::exchanges::backpack;
use data_publisher::exchanges::common::{install_tracing, PublisherConfig};
use tracing::info;

const TABLE: &str = "backpack_bookticker";

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();
    let cfg = PublisherConfig::from_env(50);
    info!("config: {:?}", cfg);

    let writer = cfg.writer(TABLE)?;
    let http = reqwest::Client::builder()
        .user_agent("data_publisher/backpack")
        .build()
        .expect("reqwest client");

    let mut symbols = backpack::fetch_perp_symbols(&http).await?;
    info!("Backpack: discovered {} PERP markets", symbols.len());
    if let Some(n) = cfg.max_symbols {
        symbols.truncate(n);
        info!("limited to {n}");
    }

    backpack::spawn_all(symbols, cfg.topics_per_conn, writer);
    tokio::signal::ctrl_c().await.ok();
    info!("shutdown");
    Ok(())
}

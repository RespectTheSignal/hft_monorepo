//! Kraken Futures bookticker publisher → QuestDB `kraken_bookticker`.

use data_publisher::error::Result;
use data_publisher::exchanges::common::{install_tracing, PublisherConfig};
use data_publisher::exchanges::kraken;
use tracing::info;

const TABLE: &str = "kraken_bookticker";

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();
    let cfg = PublisherConfig::from_env(150);
    info!("config: {:?}", cfg);

    let writer = cfg.writer(TABLE)?;
    let http = reqwest::Client::builder()
        .user_agent("data_publisher/kraken")
        .build()
        .expect("reqwest client");

    let mut symbols = kraken::fetch_perp_symbols(&http).await?;
    info!("Kraken: discovered {} perpetual instruments", symbols.len());
    if let Some(n) = cfg.max_symbols {
        symbols.truncate(n);
        info!("limited to {n}");
    }

    kraken::spawn_all(symbols, cfg.topics_per_conn, writer);
    tokio::signal::ctrl_c().await.ok();
    info!("shutdown");
    Ok(())
}

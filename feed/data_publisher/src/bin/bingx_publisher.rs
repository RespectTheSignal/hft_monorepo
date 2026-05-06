//! BingX bookticker publisher → QuestDB `bingx_bookticker`.

use data_publisher::error::Result;
use data_publisher::exchanges::bingx;
use data_publisher::exchanges::common::{install_tracing, PublisherConfig};
use tracing::info;

const TABLE: &str = "bingx_bookticker";

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();
    let cfg = PublisherConfig::from_env(80);
    info!("config: {:?}", cfg);

    let writer = cfg.writer(TABLE)?;
    let http = reqwest::Client::builder()
        .user_agent("data_publisher/bingx")
        .build()
        .expect("reqwest client");

    let mut symbols = bingx::fetch_perp_symbols(&http).await?;
    info!("BingX: discovered {} USDT perp contracts", symbols.len());
    if let Some(n) = cfg.max_symbols {
        symbols.truncate(n);
        info!("limited to {n}");
    }

    bingx::spawn_all(symbols, cfg.topics_per_conn, writer);
    tokio::signal::ctrl_c().await.ok();
    info!("shutdown");
    Ok(())
}

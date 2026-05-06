//! MEXC Futures bookticker publisher → QuestDB `mexc_bookticker`.

use data_publisher::error::Result;
use data_publisher::exchanges::common::{install_tracing, PublisherConfig};
use data_publisher::exchanges::mexc;
use tracing::info;

const TABLE: &str = "mexc_bookticker";

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();
    let cfg = PublisherConfig::from_env(50);
    info!("config: {:?}", cfg);

    let writer = cfg.writer(TABLE)?;
    let http = reqwest::Client::builder()
        .user_agent("data_publisher/mexc")
        .build()
        .expect("reqwest client");

    let mut symbols = mexc::fetch_usdt_perp_symbols(&http).await?;
    info!("MEXC: discovered {} USDT perp contracts", symbols.len());
    if let Some(n) = cfg.max_symbols {
        symbols.truncate(n);
        info!("limited to {n}");
    }

    mexc::spawn_all(symbols, cfg.topics_per_conn, writer);
    tokio::signal::ctrl_c().await.ok();
    info!("shutdown");
    Ok(())
}

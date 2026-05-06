//! KuCoin Futures bookticker publisher → QuestDB `kucoin_bookticker`.

use data_publisher::error::Result;
use data_publisher::exchanges::common::{install_tracing, PublisherConfig};
use data_publisher::exchanges::kucoin;
use tracing::info;

const TABLE: &str = "kucoin_bookticker";

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();
    let cfg = PublisherConfig::from_env(100);
    info!("config: {:?}", cfg);

    let writer = cfg.writer(TABLE)?;
    let http = reqwest::Client::builder()
        .user_agent("data_publisher/kucoin")
        .build()
        .expect("reqwest client");

    let mut symbols = kucoin::fetch_usdt_perp_symbols(&http).await?;
    info!("KuCoin: discovered {} USDT-margined contracts", symbols.len());
    if let Some(n) = cfg.max_symbols {
        symbols.truncate(n);
        info!("limited to {n}");
    }

    kucoin::spawn_all(symbols, cfg.topics_per_conn, writer);
    tokio::signal::ctrl_c().await.ok();
    info!("shutdown");
    Ok(())
}

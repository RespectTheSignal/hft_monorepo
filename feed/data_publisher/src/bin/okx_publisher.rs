//! OKX bookticker publisher → QuestDB `okx_bookticker`.

use data_publisher::error::Result;
use data_publisher::exchanges::common::{install_tracing, PublisherConfig};
use data_publisher::exchanges::okx;
use tracing::info;

const TABLE: &str = "okx_bookticker";

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();
    let cfg = PublisherConfig::from_env(100);
    info!("config: {:?}", cfg);

    let writer = cfg.writer(TABLE)?;
    let http = reqwest::Client::builder()
        .user_agent("data_publisher/okx")
        .build()
        .expect("reqwest client");

    let mut instruments = okx::fetch_usdt_swap_instruments(&http).await?;
    info!("OKX: discovered {} USDT-SWAP instruments", instruments.len());
    if let Some(n) = cfg.max_symbols {
        instruments.truncate(n);
        info!("limited to {n}");
    }

    okx::spawn_all(instruments, cfg.topics_per_conn, writer);

    tokio::signal::ctrl_c().await.ok();
    info!("shutdown");
    Ok(())
}

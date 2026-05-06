//! Binance Futures depth5@100ms publisher → QuestDB `binance_depth5`.
//! Same overlap-alts universe as binance_trade_publisher.

use data_publisher::error::Result;
use data_publisher::exchanges::binance;
use data_publisher::exchanges::common::{install_tracing, PublisherConfig};
use data_publisher::ilp::Depth5Writer;
use tracing::info;

const TABLE: &str = "binance_depth5";
const MAJORS: &[&str] = &["BTC", "ETH", "SOL", "BNB", "XRP", "DOGE"];

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();
    let cfg = PublisherConfig::from_env(200);
    info!("config: {:?}", cfg);

    let writer = Depth5Writer::spawn(cfg.qdb_host.clone(), cfg.qdb_ilp_port, TABLE.into())?;
    let http = reqwest::Client::builder()
        .user_agent("data_publisher/binance_depth")
        .build()
        .expect("reqwest");

    let mut symbols = binance::fetch_overlap_alts(&http, MAJORS).await?;
    info!("Binance↔BingX alt overlap: {} symbols", symbols.len());
    if let Some(n) = cfg.max_symbols {
        symbols.truncate(n);
        info!("limited to {n}");
    }

    binance::spawn_depth_all(symbols, cfg.topics_per_conn, writer);
    tokio::signal::ctrl_c().await.ok();
    info!("shutdown");
    Ok(())
}

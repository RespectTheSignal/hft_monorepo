//! Binance Futures aggTrade publisher → QuestDB `binance_trade`.
//! Filters to alts that overlap with BingX (excludes BTC/ETH/SOL/BNB/XRP/DOGE).

use data_publisher::error::Result;
use data_publisher::exchanges::binance;
use data_publisher::exchanges::common::{install_tracing, PublisherConfig};
use data_publisher::ilp::TradeWriter;
use tracing::info;

const TABLE: &str = "binance_trade";
const MAJORS: &[&str] = &["BTC", "ETH", "SOL", "BNB", "XRP", "DOGE"];

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();
    let cfg = PublisherConfig::from_env(200);
    info!("config: {:?}", cfg);

    let writer = TradeWriter::spawn(cfg.qdb_host.clone(), cfg.qdb_ilp_port, TABLE.into())?;
    let http = reqwest::Client::builder()
        .user_agent("data_publisher/binance_trade")
        .build()
        .expect("reqwest");

    let mut symbols = binance::fetch_overlap_alts(&http, MAJORS).await?;
    info!("Binance↔BingX alt overlap: {} symbols", symbols.len());
    if let Some(n) = cfg.max_symbols {
        symbols.truncate(n);
        info!("limited to {n}");
    }

    binance::spawn_aggtrade_all(symbols, cfg.topics_per_conn, writer);
    tokio::signal::ctrl_c().await.ok();
    info!("shutdown");
    Ok(())
}

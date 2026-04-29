// Trait for market data source (IPC or ZMQ). Strategy uses this to receive bookticker/trade.

use anyhow::Result;

pub trait DataClient: Send + Sync {
    fn start(&self) -> Result<()>;
    fn stop(&self);
    fn update_symbols(&self, symbols: Vec<String>);
    fn is_running(&self) -> bool;
}

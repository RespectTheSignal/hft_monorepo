//! JSONL writer for closed-trade records. Schema is field-for-field
//! compatible with the Python executor's `_append_trade` so existing
//! Grafana dashboards and analysis scripts keep working.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// One closed-trade record. Field names + ordering preserved exactly to
/// match the Python schema (the live_trades.jsonl is consumed by analysis
/// scripts that inspect specific keys).
#[derive(Debug, Clone, Serialize)]
pub struct TradeRecord {
    pub ts_close: String,
    pub exit_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_spread_bp: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_captured_bp: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub f_entry_slip_bp: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub g_entry_slip_bp: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub f_exit_slip_bp: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub g_exit_slip_bp: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal_lag_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub f_entry_order_rtt_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub f_entry_total_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub f_maker_exit_order_rtt_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub f_maker_cancel_rtt_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub f_exit_order_rtt_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_bbo_recheck_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_bbo_adverse_bp: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_topbook_qty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_order_qty_before_topbook: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_size_before_topbook_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_size_after_topbook_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paper_f_entry: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paper_g_entry: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paper_f_exit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paper_g_exit: Option<f64>,
    pub ts_entry: String,
    pub pos_id: i64,
    pub base: String,
    pub flipster_side: String,
    pub size_usd: f64,
    pub flipster_entry: f64,
    pub flipster_exit: f64,
    pub gate_entry: f64,
    pub gate_exit: f64,
    pub gate_size: i64,
    pub gate_contract: String,
    pub f_pnl_usd: f64,
    pub g_pnl_usd: f64,
    pub net_pnl_usd: f64,
    pub approx_fee_usd: f64,
    pub net_after_fees_usd: f64,
}

#[derive(Clone)]
pub struct TradeLog {
    inner: Arc<Mutex<TradeLogInner>>,
}

struct TradeLogInner {
    path: PathBuf,
}

impl TradeLog {
    pub fn new(path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TradeLogInner { path })),
        }
    }

    /// Append one record. Creates the parent directory if missing.
    pub async fn append(&self, rec: &TradeRecord) -> std::io::Result<()> {
        let line = serde_json::to_string(rec)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let inner = self.inner.lock().await;
        if let Some(parent) = inner.path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&inner.path)
            .await?;
        f.write_all(line.as_bytes()).await?;
        f.write_all(b"\n").await?;
        f.flush().await?;
        Ok(())
    }
}

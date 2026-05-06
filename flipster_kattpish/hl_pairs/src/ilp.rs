//! ILP writer for closed paper trades → central QuestDB position_log.
//!
//! Schema (existing, shared across strategies):
//!   account_id, symbol, side, strategy, exit_reason, mode SYMBOL
//!   entry_price, exit_price, size, pnl_bp DOUBLE
//!   exit_time, timestamp TIMESTAMP

use anyhow::Result;
use chrono::{DateTime, Utc};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct ClosedTrade {
    pub account_id: String, // variant id, e.g. "hlp_B_minstd3"
    pub symbol: String,
    pub side: &'static str, // "long" | "short"
    pub exit_reason: &'static str,
    pub entry_price: f64,
    pub exit_price: f64,
    pub size_usd: f64,
    pub pnl_bp: f64,
    pub entry_time: DateTime<Utc>,
    pub exit_time: DateTime<Utc>,
}

#[derive(Clone)]
pub struct Writer {
    tx: mpsc::SyncSender<ClosedTrade>,
}

impl Writer {
    pub fn spawn(host: String, port: u16) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<ClosedTrade>(8192);
        let conf = format!("tcp::addr={host}:{port};");
        let sender = Sender::from_conf(&conf)?;
        info!("hl_pairs ILP writer connected: {host}:{port} → position_log");
        thread::Builder::new()
            .name("hlp-ilp".into())
            .spawn(move || writer_thread(sender, rx))?;
        Ok(Self { tx })
    }

    pub fn submit(&self, t: ClosedTrade) {
        let _ = self.tx.try_send(t);
    }
}

fn writer_thread(mut sender: Sender, rx: mpsc::Receiver<ClosedTrade>) {
    let mut buffer = Buffer::new();
    let mut pending = 0usize;
    let mut last_flush = Instant::now();
    loop {
        let timeout = Duration::from_millis(500)
            .checked_sub(last_flush.elapsed())
            .unwrap_or(Duration::ZERO);
        match rx.recv_timeout(timeout) {
            Ok(t) => {
                if let Err(e) = encode(&mut buffer, &t) { warn!("encode: {e}"); continue; }
                pending += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if pending > 0 { let _ = sender.flush(&mut buffer); }
                return;
            }
        }
        if pending > 0 && (pending >= 50 || last_flush.elapsed() >= Duration::from_millis(500)) {
            if let Err(e) = sender.flush(&mut buffer) {
                warn!("ilp flush: {e}");
            }
            pending = 0;
            last_flush = Instant::now();
        }
    }
}

fn encode(buf: &mut Buffer, t: &ClosedTrade) -> Result<()> {
    let exit_ns = t.exit_time.timestamp_nanos_opt().unwrap_or(0);
    buf.table("position_log")?
       .symbol("account_id", t.account_id.as_str())?
       .symbol("symbol", t.symbol.as_str())?
       .symbol("side", t.side)?
       .symbol("strategy", "hl_pairs")?
       .symbol("exit_reason", t.exit_reason)?
       .symbol("mode", "paper")?
       .column_f64("entry_price", t.entry_price)?
       .column_f64("exit_price", t.exit_price)?
       .column_f64("size", t.size_usd)?
       .column_f64("pnl_bp", t.pnl_bp)?
       .column_ts("exit_time", TimestampNanos::new(exit_ns))?
       .at(TimestampNanos::new(exit_ns))?;
    Ok(())
}

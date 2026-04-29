// JSONL execution logger — one line per open/close/rejection.
// Format compatible with Python exec_live.jsonl schema.
//
// Usage:
//   let logger = ExecLogger::open("logs/exec.jsonl")?;
//   logger.open_event(sym, side, kind, intended, actual, slip_bp, api_bid, api_ask, web_bid, web_ask, filled);
//   logger.close_event(sym, side, kind, reason, entry, exit, move_bp, hold_ms, pnl_usd, cum_pnl);
//   logger.rejection(sym, reason, side, cross_bp, details);

use anyhow::Result;
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

pub struct ExecLogger {
    w: Mutex<BufWriter<File>>,
}

impl ExecLogger {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Arc<Self>> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Arc::new(Self {
            w: Mutex::new(BufWriter::new(f)),
        }))
    }

    fn write(&self, v: Value) {
        let line = match serde_json::to_string(&v) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut w = self.w.lock();
        let _ = writeln!(w, "{}", line);
        let _ = w.flush();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_event(
        &self,
        sym: &str,
        side: &str,
        kind: &str,
        intended: f64,
        actual: f64,
        slip_bp: f64,
        api_bid: f64,
        api_ask: f64,
        web_bid: f64,
        web_ask: f64,
        filled: bool,
    ) {
        self.write(json!({
            "ts_ms": Utc::now().timestamp_millis(),
            "event": "open",
            "sym": sym, "side": side, "kind": kind,
            "intended": round6(intended), "actual": round6(actual),
            "slip_bp": round_bp(slip_bp),
            "filled": filled,
            "api_bid": round6(api_bid), "api_ask": round6(api_ask),
            "web_bid": round6(web_bid), "web_ask": round6(web_ask),
        }));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn close_event(
        &self,
        sym: &str,
        side: &str,
        kind: &str,
        reason: &str,
        entry: f64,
        exit: f64,
        move_bp: f64,
        hold_ms: i64,
        pnl_usd: f64,
        cum_pnl: f64,
    ) {
        self.write(json!({
            "ts_ms": Utc::now().timestamp_millis(),
            "event": "close",
            "sym": sym, "side": side, "kind": kind, "reason": reason,
            "entry": round6(entry), "exit": round6(exit),
            "move_bp": round_bp(move_bp), "hold_ms": hold_ms,
            "pnl_usd": round4(pnl_usd), "cum_pnl": round4(cum_pnl),
        }));
    }

    pub fn rejection(&self, sym: &str, reason: &str, side: Option<&str>, cross_bp: f64, extra: Value) {
        self.write(json!({
            "ts_ms": Utc::now().timestamp_millis(),
            "event": "reject",
            "sym": sym, "reason": reason, "side": side,
            "cross_bp": round_bp(cross_bp),
            "extra": extra,
        }));
    }
}

fn round6(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

fn round4(x: f64) -> f64 {
    (x * 1e4).round() / 1e4
}

fn round_bp(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

// Local per-strategy position state + rate limit + blacklist.
// Mirrors STATE.positions / STATE.trade_times / SYM_BLACKLIST in Python.

use crate::types::Side;
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Locally tracked position (separate from private WS positions — this is our
/// strategy's view of positions we've opened, with TP bracket and timing state).
#[derive(Debug, Clone)]
pub struct OpenPosition {
    pub sym: String,
    pub side: Side,
    pub size_usd: f64,
    pub entry_mid_api: f64,
    pub entry_fill_price: f64,
    pub entry_time_ms: i64,
    pub tp_target_price: f64,
    pub tp_order_id: Option<String>,
    /// If true, stop has been tripped and confirm_ticks reached.
    pub stop_confirm_count: u32,
}

impl OpenPosition {
    pub fn age_ms(&self, now_ms: i64) -> i64 {
        now_ms - self.entry_time_ms
    }
}

/// Local state — not the same as private WS positions (those are exchange truth).
/// This holds our *strategy intent*: what we opened, when, where TP is.
pub struct PositionManager {
    open: Arc<DashMap<String, OpenPosition>>,
    /// Trade submit timestamps for rate limit.
    trade_times: Arc<Mutex<VecDeque<i64>>>,
    /// sym → unlock_at_ms (blacklist after loss streak).
    blacklist: Arc<DashMap<String, i64>>,
    /// sym → when 'skip this sym' was last set (e.g. post_only rejection lockout).
    sym_cooldown: Arc<DashMap<String, i64>>,
    rate_limit_per_min: i64,
    /// Running sum of realized PnL (USD), for status + kill switch.
    cum_pnl_usd: Arc<RwLock<f64>>,
    /// Negative threshold (USD). If cum_pnl <= -kill_usd, kill flag fires.
    kill_usd: f64,
    kill_triggered: Arc<AtomicBool>,
    // Counters for status.
    pub signals: Arc<std::sync::atomic::AtomicU64>,
    pub opens: Arc<std::sync::atomic::AtomicU64>,
    pub closes: Arc<std::sync::atomic::AtomicU64>,
    pub flips: Arc<std::sync::atomic::AtomicU64>,
}

impl PositionManager {
    pub fn new(rate_limit_per_min: i64, kill_usd: f64) -> Self {
        Self {
            open: Arc::new(DashMap::new()),
            trade_times: Arc::new(Mutex::new(VecDeque::new())),
            blacklist: Arc::new(DashMap::new()),
            sym_cooldown: Arc::new(DashMap::new()),
            rate_limit_per_min,
            cum_pnl_usd: Arc::new(RwLock::new(0.0)),
            kill_usd: kill_usd.abs(),
            kill_triggered: Arc::new(AtomicBool::new(false)),
            signals: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            opens: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            closes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            flips: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    pub fn record_pnl(&self, pnl_usd: f64) -> f64 {
        let mut c = self.cum_pnl_usd.write();
        *c += pnl_usd;
        let cum = *c;
        if self.kill_usd > 0.0 && cum <= -self.kill_usd {
            self.kill_triggered.store(true, Ordering::SeqCst);
        }
        cum
    }

    pub fn cum_pnl(&self) -> f64 {
        *self.cum_pnl_usd.read()
    }

    pub fn kill_triggered(&self) -> bool {
        self.kill_triggered.load(Ordering::SeqCst)
    }

    pub fn is_open(&self, sym: &str) -> bool {
        self.open.contains_key(sym)
    }

    pub fn num_open(&self) -> usize {
        self.open.len()
    }

    pub fn get(&self, sym: &str) -> Option<OpenPosition> {
        self.open.get(sym).map(|e| e.clone())
    }

    pub fn insert(&self, pos: OpenPosition) {
        self.open.insert(pos.sym.clone(), pos);
    }

    pub fn update_tp_order_id(&self, sym: &str, oid: String) {
        if let Some(mut e) = self.open.get_mut(sym) {
            e.tp_order_id = Some(oid);
        }
    }

    pub fn bump_stop_confirm(&self, sym: &str) -> u32 {
        let mut e = match self.open.get_mut(sym) {
            Some(e) => e,
            None => return 0,
        };
        e.stop_confirm_count += 1;
        e.stop_confirm_count
    }

    pub fn reset_stop_confirm(&self, sym: &str) {
        if let Some(mut e) = self.open.get_mut(sym) {
            e.stop_confirm_count = 0;
        }
    }

    pub fn remove(&self, sym: &str) -> Option<OpenPosition> {
        self.open.remove(sym).map(|(_, v)| v)
    }

    pub fn all(&self) -> Vec<OpenPosition> {
        self.open.iter().map(|e| e.value().clone()).collect()
    }

    // ---- rate limit ----

    pub fn record_trade_attempt(&self, now_ms: i64) {
        let mut q = self.trade_times.lock();
        q.push_back(now_ms);
        let cutoff = now_ms - 60_000;
        while let Some(&front) = q.front() {
            if front < cutoff {
                q.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn rate_limited(&self, now_ms: i64) -> bool {
        let q = self.trade_times.lock();
        let cutoff = now_ms - 60_000;
        let recent = q.iter().rev().take_while(|&&t| t >= cutoff).count() as i64;
        recent >= self.rate_limit_per_min
    }

    // ---- blacklist ----

    pub fn is_blacklisted(&self, sym: &str, now_ms: i64) -> bool {
        self.blacklist
            .get(sym)
            .map(|until| *until > now_ms)
            .unwrap_or(false)
    }

    pub fn blacklist(&self, sym: &str, until_ms: i64) {
        self.blacklist.insert(sym.to_string(), until_ms);
    }

    pub fn cooldown_remaining_ms(&self, sym: &str, now_ms: i64) -> i64 {
        self.sym_cooldown
            .get(sym)
            .map(|until| (*until - now_ms).max(0))
            .unwrap_or(0)
    }

    pub fn set_cooldown(&self, sym: &str, until_ms: i64) {
        self.sym_cooldown.insert(sym.to_string(), until_ms);
    }
}

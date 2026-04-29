// Main signal loop — mirrors gate_hft_rust::strategy::signal_loop.
// Parallel per-symbol processing via rayon, shuffled order.

use crate::data_cache::DataCache;
use crate::strategy_core::StrategyCore;
use chrono::Utc;
use parking_lot::RwLock;
use rand::seq::SliceRandom;
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

#[derive(Default)]
pub struct StrategyStats {
    pub ticks: AtomicU64,
    pub signals: AtomicU64,
    pub skips_below_edge: AtomicU64,
    pub skips_wide_spread: AtomicU64,
    pub skips_web_age: AtomicU64,
    pub skips_no_cross: AtomicU64,
    pub skips_overshoot: AtomicU64,
    pub skips_orderbook_stale: AtomicU64,
    pub skips_other: AtomicU64,
}

impl StrategyStats {
    fn record_skip(&self, reason: &str) {
        match reason {
            "below_min_edge" => self.skips_below_edge.fetch_add(1, Ordering::Relaxed),
            "wide_spread" => self.skips_wide_spread.fetch_add(1, Ordering::Relaxed),
            "web_age" => self.skips_web_age.fetch_add(1, Ordering::Relaxed),
            "no_cross" => self.skips_no_cross.fetch_add(1, Ordering::Relaxed),
            "overshoot" => self.skips_overshoot.fetch_add(1, Ordering::Relaxed),
            "orderbook_stale" => self.skips_orderbook_stale.fetch_add(1, Ordering::Relaxed),
            _ => self.skips_other.fetch_add(1, Ordering::Relaxed),
        };
    }
}

pub struct SignalLoopHandle {
    pub stats: Arc<StrategyStats>,
    pub running: Arc<AtomicBool>,
}

impl SignalLoopHandle {
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

/// Config for `run_signal_loop`.
pub struct SignalLoopConfig {
    pub symbols: Arc<RwLock<Vec<String>>>,
    /// 10ms tick interval (matches Python 10ms sleep in gate_hft signal loop).
    pub interval_ms: u64,
}

/// Callback invoked for each new signal. In dry-run: just log. In live: submit order.
pub trait SignalHandler: Send + Sync {
    fn on_signal(&self, symbol: &str, sig: &crate::signal::SignalResult);
}

/// Dry-run handler — just logs signals.
pub struct LoggingHandler {
    /// per-symbol rate limit on log (avoid spam).
    last_log: Arc<RwLock<HashMap<String, i64>>>,
    min_log_gap_ms: i64,
}

impl Default for LoggingHandler {
    fn default() -> Self {
        Self {
            last_log: Arc::new(RwLock::new(HashMap::new())),
            min_log_gap_ms: 500,
        }
    }
}

impl SignalHandler for LoggingHandler {
    fn on_signal(&self, symbol: &str, sig: &crate::signal::SignalResult) {
        let now = Utc::now().timestamp_millis();
        {
            let last = self.last_log.read().get(symbol).copied().unwrap_or(0);
            if now - last < self.min_log_gap_ms {
                return;
            }
        }
        self.last_log.write().insert(symbol.to_string(), now);
        if let Some(side) = sig.side {
            info!(
                "[SIG] {:<22} {:<4} cross={:+.2}bp maker_px={:.6} tp={:.6}",
                symbol,
                side.as_str(),
                sig.cross_bp,
                sig.maker_price.unwrap_or(0.0),
                sig.api_tp_target.unwrap_or(0.0),
            );
        }
    }
}

/// Live handler — pushes signals into a crossbeam channel for an async worker.
pub struct ChannelHandler {
    tx: crossbeam_channel::Sender<(String, crate::signal::SignalResult)>,
}

impl ChannelHandler {
    pub fn new(tx: crossbeam_channel::Sender<(String, crate::signal::SignalResult)>) -> Self {
        Self { tx }
    }
}

impl SignalHandler for ChannelHandler {
    fn on_signal(&self, symbol: &str, sig: &crate::signal::SignalResult) {
        // try_send: full queue → drop (better than blocking the signal loop).
        let _ = self.tx.try_send((symbol.to_string(), sig.clone()));
    }
}

pub fn run_signal_loop(
    cache: Arc<DataCache>,
    core: Arc<dyn StrategyCore>,
    handler: Arc<dyn SignalHandler>,
    cfg: SignalLoopConfig,
) -> SignalLoopHandle {
    let stats = Arc::new(StrategyStats::default());
    let running = Arc::new(AtomicBool::new(true));

    let stats_loop = stats.clone();
    let running_loop = running.clone();
    let cache_loop = cache.clone();
    let core_loop = core.clone();
    let handler_loop = handler.clone();

    std::thread::spawn(move || {
        // Warmup — wait for feeds
        for _ in 0..50 {
            if cache_loop.has_data() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        info!(
            "[strategy:{}] warmed up, starting signal loop",
            core_loop.name()
        );

        while running_loop.load(Ordering::SeqCst) {
            if !cache_loop.has_data() {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            let now_ms = Utc::now().timestamp_millis();

            let symbols_snapshot = { cfg.symbols.read().clone() };
            let mut shuffled = symbols_snapshot;
            shuffled.shuffle(&mut rand::thread_rng());

            shuffled.par_iter().for_each(|sym| {
                let snap = match cache_loop.get_snapshot(sym) {
                    Some(s) => s,
                    None => return,
                };
                stats_loop.ticks.fetch_add(1, Ordering::Relaxed);

                let sig = core_loop.calculate_signal(&snap, now_ms);
                if let Some(reason) = sig.skip_reason {
                    stats_loop.record_skip(reason);
                    return;
                }
                if sig.has_signal() {
                    stats_loop.signals.fetch_add(1, Ordering::Relaxed);
                    handler_loop.on_signal(sym, &sig);
                }
            });

            std::thread::sleep(Duration::from_millis(cfg.interval_ms));
        }
    });

    SignalLoopHandle { stats, running }
}

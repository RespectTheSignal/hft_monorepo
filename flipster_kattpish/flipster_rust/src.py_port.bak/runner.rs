// Entry worker — bridges signal_loop's channel to async open_position.
// Enforces per-sym single open, rate limit, blacklist cooldown before submitting.

use crate::data_cache::DataCache;
use crate::entry::open_position_arc;
use crate::exec_log::ExecLogger;
use crate::order_manager::FlipsterOrderManager;
use crate::params::StrategyParams;
use crate::position_state::PositionManager;
use crate::signal::SignalResult;
use crate::symbols::SymbolRegistry;
use chrono::Utc;
use crossbeam_channel::Receiver;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::info;

pub fn spawn_entry_worker(
    rx: Receiver<(String, SignalResult)>,
    om: Arc<FlipsterOrderManager>,
    pos_mgr: Arc<PositionManager>,
    cache: Arc<DataCache>,
    reg: Arc<SymbolRegistry>,
    exec_log: Option<Arc<ExecLogger>>,
    params: StrategyParams,
) {
    let rt = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        while let Ok((sym, sig)) = rx.recv() {
            // Kill switch — stop opening new positions if cum PnL breached.
            if pos_mgr.kill_triggered() {
                continue;
            }
            pos_mgr.signals.fetch_add(1, Ordering::Relaxed);
            if should_skip(&pos_mgr, &sym, &params) {
                continue;
            }
            let om = om.clone();
            let pos_mgr = pos_mgr.clone();
            let cache = cache.clone();
            let reg = reg.clone();
            let exec_log = exec_log.clone();
            let params = params.clone();
            rt.spawn(async move {
                let _ = open_position_arc(om, pos_mgr, cache, reg, exec_log, sym, sig, params).await;
            });
        }
    });
}

fn should_skip(pos_mgr: &PositionManager, sym: &str, params: &StrategyParams) -> bool {
    let now = Utc::now().timestamp_millis();
    if pos_mgr.is_open(sym) {
        return true; // already have a position on this symbol
    }
    if pos_mgr.rate_limited(now) {
        return true;
    }
    if pos_mgr.is_blacklisted(sym, now) {
        return true;
    }
    let cd = pos_mgr.cooldown_remaining_ms(sym, now);
    if cd > 0 {
        return true;
    }
    let _ = params; // currently only rate_limit_per_min comes from params via PositionManager::new
    false
}

/// Convenience: periodic status logger.
pub async fn status_logger(
    cache: Arc<DataCache>,
    pos_mgr: Arc<PositionManager>,
    stats: Arc<crate::strategy::StrategyStats>,
    interval_s: u64,
) {
    let start = std::time::Instant::now();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_s));
    loop {
        interval.tick().await;
        let (api_n, web_n) = cache.counts();
        info!(
            "[status t={}s] api={} web={} ticks={} sig={} open={} close={} pos={} cum=${:+.4} kill={} skip(edge={} sp={} age={} stale={} no_cross={})",
            start.elapsed().as_secs(),
            api_n,
            web_n,
            stats.ticks.load(Ordering::Relaxed),
            stats.signals.load(Ordering::Relaxed),
            pos_mgr.opens.load(Ordering::Relaxed),
            pos_mgr.closes.load(Ordering::Relaxed),
            pos_mgr.num_open(),
            pos_mgr.cum_pnl(),
            pos_mgr.kill_triggered(),
            stats.skips_below_edge.load(Ordering::Relaxed),
            stats.skips_wide_spread.load(Ordering::Relaxed),
            stats.skips_web_age.load(Ordering::Relaxed),
            stats.skips_other.load(Ordering::Relaxed),
            stats.skips_no_cross.load(Ordering::Relaxed),
        );
    }
}

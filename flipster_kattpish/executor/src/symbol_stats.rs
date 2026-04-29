//! Per-symbol dynamic filtering.
//!
//! Tracks rolling-window stats per base symbol (last N trades) and
//! disables symbols that show:
//!   - negative recent net PnL  → spread-eaten / weak edge → cooldown
//!   - weak paper-mid signal     → too noisy to follow → cooldown
//!   - wide current spread       → entry would be unprofitable → skip this tick
//!
//! Cooldown allows occasional probe trades to detect recovery.
//! Persisted to disk so executor restarts retain learning.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

pub const WINDOW: usize = 20;
pub const MIN_SAMPLES: usize = 8;
pub const PNL_THRESHOLD_BP: f64 = -2.0;
pub const PAPER_THRESHOLD_BP: f64 = 3.0;
pub const SPREAD_THRESHOLD_BP: f64 = 8.0;
pub const COOLDOWN_SECS: f64 = 1800.0; // 30 min
pub const PROBE_COUNT: u32 = 5;

// Dynamic sizing: per-symbol scale_factor adapts based on observed
// slippage. Healthy symbols (low slip + positive pnl) grow toward
// MAX_SCALE; expensive ones shrink toward MIN_SCALE.
pub const SCALE_MIN: f64 = 0.05; // 5% of base — e.g. $5 on $100 base
pub const SCALE_MAX: f64 = 4.0; // 400% of base
pub const SLIP_HIGH_BP: f64 = 5.0; // total in+ex slip — above = downsize
pub const SLIP_LOW_BP: f64 = 3.0; // below = upsize (if pnl positive)
pub const SCALE_DOWN_FACTOR: f64 = 0.7;
pub const SCALE_UP_FACTOR: f64 = 1.1;
pub const MIN_SAMPLES_FOR_SCALE: usize = 5;
// PnL-based size triggers (independent of slip).
pub const PNL_DOWNSIZE_BP: f64 = 1.0; // avg net pnl below this = downsize
pub const PNL_UPSIZE_BP: f64 = 5.0; // avg net pnl above + low slip = upsize

#[derive(Clone, Serialize, Deserialize)]
pub struct SymbolStats {
    pub n: u64,
    pub total_pnl_bp: f64,
    pub total_paper_bp: f64,
    pub recent_pnl_bp: VecDeque<f64>,
    pub recent_paper_bp: VecDeque<f64>,
    pub recent_slip_bp: VecDeque<f64>, // total = in_slip + ex_slip
    /// Unix epoch seconds when cooldown expires; None = active.
    pub cooldown_until_epoch: Option<f64>,
    pub probe_remaining: u32,
    /// Multiplicative scale on base size. Adapts per-trade.
    #[serde(default = "default_scale")]
    pub scale_factor: f64,
}

fn default_scale() -> f64 { 1.0 }

impl Default for SymbolStats {
    fn default() -> Self {
        Self {
            n: 0,
            total_pnl_bp: 0.0,
            total_paper_bp: 0.0,
            recent_pnl_bp: VecDeque::new(),
            recent_paper_bp: VecDeque::new(),
            recent_slip_bp: VecDeque::new(),
            cooldown_until_epoch: None,
            probe_remaining: 0,
            scale_factor: 1.0,
        }
    }
}

impl SymbolStats {
    fn push(&mut self, pnl_bp: f64, paper_bp: f64, slip_bp: f64) {
        self.n += 1;
        self.total_pnl_bp += pnl_bp;
        self.total_paper_bp += paper_bp;
        for q in [
            &mut self.recent_pnl_bp,
            &mut self.recent_paper_bp,
            &mut self.recent_slip_bp,
        ] {
            if q.len() >= WINDOW {
                q.pop_front();
            }
        }
        self.recent_pnl_bp.push_back(pnl_bp);
        self.recent_paper_bp.push_back(paper_bp);
        self.recent_slip_bp.push_back(slip_bp);
    }

    fn avg_pnl(&self) -> Option<f64> {
        if self.recent_pnl_bp.len() < MIN_SAMPLES {
            return None;
        }
        Some(self.recent_pnl_bp.iter().sum::<f64>() / self.recent_pnl_bp.len() as f64)
    }

    fn avg_paper(&self) -> Option<f64> {
        if self.recent_paper_bp.len() < MIN_SAMPLES {
            return None;
        }
        Some(self.recent_paper_bp.iter().sum::<f64>() / self.recent_paper_bp.len() as f64)
    }

    fn avg_slip(&self) -> Option<f64> {
        if self.recent_slip_bp.len() < MIN_SAMPLES_FOR_SCALE {
            return None;
        }
        Some(self.recent_slip_bp.iter().sum::<f64>() / self.recent_slip_bp.len() as f64)
    }

    /// Update scale_factor based on recent slip + pnl. Called after each
    /// trade. No-op until MIN_SAMPLES_FOR_SCALE trades collected.
    ///
    /// Two independent triggers (whichever fires more aggressively):
    ///   - slip-based: high absolute slip → downsize
    ///   - net-pnl-based: low/negative net edge → downsize
    /// Upsize only when BOTH look healthy.
    fn update_scale(&mut self) {
        let Some(slip) = self.avg_slip() else {
            return;
        };
        let Some(pnl) = self.avg_pnl() else {
            return;
        };
        // Downsize triggers (most aggressive wins).
        if slip > SLIP_HIGH_BP || pnl < PNL_DOWNSIZE_BP {
            self.scale_factor =
                (self.scale_factor * SCALE_DOWN_FACTOR).max(SCALE_MIN);
            return;
        }
        // Upsize: low slip AND clearly positive net (not just slightly >0).
        if slip < SLIP_LOW_BP && pnl > PNL_UPSIZE_BP {
            self.scale_factor =
                (self.scale_factor * SCALE_UP_FACTOR).min(SCALE_MAX);
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum FilterDecision {
    /// Pass — proceed with trade as usual.
    Pass,
    /// Skip with reason; logged so user can see why.
    Skip(SkipReason),
    /// In cooldown but spend a probe slot.
    Probe,
}

#[derive(Debug, Clone, Copy)]
pub enum SkipReason {
    NegPnlCooldown,
    WeakPaper,
    WideSpread,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::NegPnlCooldown => "neg_pnl_cooldown",
            SkipReason::WeakPaper => "weak_paper_signal",
            SkipReason::WideSpread => "wide_spread",
        }
    }
}

pub struct SymbolStatsStore {
    pub stats: DashMap<String, SymbolStats>,
    persist_path: Option<PathBuf>,
    save_lock: Mutex<()>,
}

impl SymbolStatsStore {
    pub fn new(persist_path: Option<PathBuf>) -> Arc<Self> {
        let stats: DashMap<String, SymbolStats> = if let Some(p) = persist_path.as_ref() {
            match std::fs::read(p) {
                Ok(bytes) => match serde_json::from_slice::<
                    std::collections::HashMap<String, SymbolStats>,
                >(&bytes)
                {
                    Ok(m) => {
                        tracing::info!(
                            path = %p.display(),
                            n_symbols = m.len(),
                            "[symstats] loaded"
                        );
                        m.into_iter().collect()
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "[symstats] parse failed; starting fresh");
                        DashMap::new()
                    }
                },
                Err(_) => DashMap::new(),
            }
        } else {
            DashMap::new()
        };
        Arc::new(Self {
            stats,
            persist_path,
            save_lock: Mutex::new(()),
        })
    }

    pub async fn save(&self) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };
        let _guard = self.save_lock.lock().await;
        let snapshot: std::collections::HashMap<String, SymbolStats> = self
            .stats
            .iter()
            .map(|kv| (kv.key().clone(), kv.value().clone()))
            .collect();
        match serde_json::to_vec_pretty(&snapshot) {
            Ok(buf) => {
                let tmp = path.with_extension("tmp");
                if let Err(e) = std::fs::write(&tmp, &buf) {
                    tracing::warn!(error = %e, "[symstats] write tmp failed");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, path) {
                    tracing::warn!(error = %e, "[symstats] rename failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "[symstats] serialize failed"),
        }
    }

    /// Called BEFORE entry. spread_bp is the live Flipster bid-ask
    /// spread in bp (computed by caller from WS state). Returns Pass /
    /// Skip / Probe.
    pub fn check_entry(&self, base: &str, spread_bp: Option<f64>) -> FilterDecision {
        // Wide-spread skip is independent of stats — applies even with
        // zero history (cold start safety).
        if let Some(sp) = spread_bp {
            if sp > SPREAD_THRESHOLD_BP {
                return FilterDecision::Skip(SkipReason::WideSpread);
            }
        }
        let Some(s) = self.stats.get(base) else {
            return FilterDecision::Pass;
        };
        let now = epoch_now();
        if let Some(until) = s.cooldown_until_epoch {
            if now < until {
                if s.probe_remaining > 0 {
                    drop(s);
                    if let Some(mut m) = self.stats.get_mut(base) {
                        if m.probe_remaining > 0 {
                            m.probe_remaining -= 1;
                        }
                    }
                    return FilterDecision::Probe;
                }
                return FilterDecision::Skip(SkipReason::NegPnlCooldown);
            }
        }
        if let Some(avg) = s.avg_pnl() {
            if avg < PNL_THRESHOLD_BP {
                drop(s);
                self.set_cooldown(base);
                return FilterDecision::Skip(SkipReason::NegPnlCooldown);
            }
        }
        if let Some(avg) = s.avg_paper() {
            if avg < PAPER_THRESHOLD_BP {
                drop(s);
                self.set_cooldown(base);
                return FilterDecision::Skip(SkipReason::WeakPaper);
            }
        }
        FilterDecision::Pass
    }

    /// Called AFTER on_exit completes.
    pub fn record_trade(&self, base: &str, pnl_bp: f64, paper_bp: f64, slip_bp: f64) {
        let mut entry = self.stats.entry(base.to_string()).or_default();
        entry.push(pnl_bp, paper_bp, slip_bp);
        let prev_scale = entry.scale_factor;
        entry.update_scale();
        if (entry.scale_factor - prev_scale).abs() > 1e-9 {
            tracing::info!(
                base = %base,
                old = prev_scale,
                new = entry.scale_factor,
                avg_slip = entry.avg_slip().unwrap_or(0.0),
                avg_pnl = entry.avg_pnl().unwrap_or(0.0),
                "[symstats] scale_factor adjusted"
            );
        }
        if let Some(until) = entry.cooldown_until_epoch {
            if epoch_now() < until && pnl_bp > 0.0 && paper_bp > PAPER_THRESHOLD_BP {
                entry.cooldown_until_epoch = None;
                entry.probe_remaining = 0;
                tracing::info!(base = %base, "[symstats] probe trade positive — cooldown cleared");
            }
        }
    }

    /// Returns the size to use for `base` given the configured base size.
    /// Clamps to [SCALE_MIN * base, SCALE_MAX * base].
    pub fn adjusted_size(&self, base: &str, base_size: f64) -> f64 {
        let scale = self
            .stats
            .get(base)
            .map(|s| s.scale_factor)
            .unwrap_or(1.0);
        let sized = base_size * scale;
        sized
            .max(base_size * SCALE_MIN)
            .min(base_size * SCALE_MAX)
    }

    fn set_cooldown(&self, base: &str) {
        let mut entry = self.stats.entry(base.to_string()).or_default();
        entry.cooldown_until_epoch = Some(epoch_now() + COOLDOWN_SECS);
        entry.probe_remaining = PROBE_COUNT;
        tracing::warn!(
            base = %base,
            cooldown_secs = COOLDOWN_SECS,
            probes = PROBE_COUNT,
            "[symstats] symbol disabled (cooldown started)"
        );
    }

    pub fn report(&self) -> Vec<(String, u64, f64, f64, f64, f64, bool)> {
        let mut out: Vec<_> = self
            .stats
            .iter()
            .map(|kv| {
                let s = kv.value();
                let avg_pnl = if s.n > 0 { s.total_pnl_bp / s.n as f64 } else { 0.0 };
                let avg_paper = if s.n > 0 {
                    s.total_paper_bp / s.n as f64
                } else {
                    0.0
                };
                let avg_slip = s.avg_slip().unwrap_or(0.0);
                let cooling = s
                    .cooldown_until_epoch
                    .map(|u| epoch_now() < u)
                    .unwrap_or(false);
                (
                    kv.key().clone(),
                    s.n,
                    avg_pnl,
                    avg_paper,
                    avg_slip,
                    s.scale_factor,
                    cooling,
                )
            })
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1));
        out
    }
}

fn epoch_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

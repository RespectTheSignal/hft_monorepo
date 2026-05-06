//! Three live strategies against paired binance / flipster feeds.
//!
//! Strategy A — cross-venue funding arbitrage
//!   Funding settles every N hours; if flipster's rate differs from binance's
//!   by more than `funding_diff_bp`, open a delta-neutral pair until the next
//!   funding time and then close. Profit ≈ rate_diff × size.
//!
//! Strategy B — 1-10s latency arbitrage
//!   Binance leads flipster by several seconds on large moves. When binance
//!   1s return exceeds `latency_trigger_bp`, take flipster in the same
//!   direction (because flipster will follow), hedge with binance.
//!
//! Strategy D — pairs mean-reversion
//!   (flipster - binance)/binance is a mean-reverting AR(1) process with
//!   half-life ~2-10s. Maintain an EWMA mean/std per symbol; when the
//!   current deviation exceeds `pairs_entry_sigma` × σ, bet on reversion.
//!
//! All three write delta-neutral trades to `position_log` with distinct
//! `strategy` labels: "funding" | "latency" | "pairs".

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use crate::ilp::IlpWriter;
use crate::market_watcher::SharedGapState;
use crate::model::{BookTick, ExchangeName};
use pairs_core::{
    base_of as pairs_core_base_of, pair_pnl_bp, Quote, RollingWindow,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};

// -----------------------------------------------------------------------------
// tunables
// -----------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Params {
    pub fee_bp: f64,
    pub size: f64,

    /// Maximum age in milliseconds for a mid to be considered "live". Entries
    /// and exits are refused if either leg's last tick is older than this.
    /// Protects against stale prices left over from a disconnected exchange.
    pub max_mid_age_ms: i64,

    /// Maximum allowed flipster bid-ask spread in bp at entry. A wide quote
    /// means crossing cost alone will eat any plausible edge — skip entry.
    /// Default 2.0 bp (anything wider than a few ticks on majors).
    pub max_entry_spread_bp: f64,

    /// Cap on total concurrent open positions across all strategies. Maps
    /// to the wallet capital constraint: total_usd / max_concurrent gives
    /// the per-leg USD notional.
    pub max_concurrent_positions: usize,

    /// Symbols that the strategy should never open a position on (comma-
    /// separated in env). Useful for quick iteration — black-list obvious
    /// losers without redeploying.
    pub blacklist: std::collections::HashSet<String>,

    /// Flip pairs direction: normally we bet on reversion. When this is
    /// true, we bet WITH the deviation (trend following). Used as a diagnostic
    /// — if reversion is net negative and trend is net positive, the spread
    /// process is actually trending, not mean-reverting.
    pub pairs_invert: bool,

    // Strategy B
    pub latency_trigger_bp: f64,
    pub latency_hold_ms: i64,
    pub latency_cooldown_ms: i64,

    // Strategy D
    pub pairs_ewma_tau_sec: f64,
    pub pairs_entry_sigma: f64,
    pub pairs_exit_sigma: f64,
    pub pairs_stop_sigma: f64,
    pub pairs_max_hold_ms: i64,
    pub pairs_min_hold_ms: i64,
    pub pairs_min_std_bp: f64,
    pub pairs_cooldown_ms: i64,

    /// Paper-execution latency (ms). When > 0, paper entries and exits for
    /// the pairs strategy are materialised at `decision_time + latency_ms`
    /// using the bid/ask prevailing at the fill moment — a crude model for
    /// maker→taker round-trip / network latency. Latency/funding strategies
    /// are unaffected.
    pub paper_fill_latency_ms: i64,

    // ---- Spread-aware cost filter (#1) ----
    /// Safety multiplier on the cost of crossing the book (spreads + fee).
    /// Entry requires `potential_edge_bp >= total_cost_bp * safety`. 0 = off.
    pub pairs_spread_edge_safety: f64,

    // ---- Binance-change cooldown (#3 stealth / adverse avoidance) ----
    /// When > 0, skip entries for a symbol whose hedge-leg quote changed
    /// within this many ms. Avoids trading in the exact window where Binance
    /// information is being absorbed by Flipster.
    pub pairs_bn_change_cooldown_ms: i64,

    // ---- Signal mode (#4 rolling window vs EWMA) ----
    /// When > 0, use a simple rolling-window mean/std over this many seconds
    /// instead of the EWMA. 0 keeps the EWMA path.
    pub pairs_rolling_window_sec: f64,

    // ---- Per-symbol auto-blacklist (#5) ----
    /// Minimum closed trades before a symbol may be auto-blacklisted. 0 = off.
    pub pairs_auto_blacklist_min_trades: usize,
    /// Threshold (bp, typically negative). If `avg_pnl_bp < this`, blacklist.
    pub pairs_auto_blacklist_max_avg_bp: f64,
    /// How long a symbol stays blacklisted (ms) once tripped.
    pub pairs_auto_blacklist_duration_ms: i64,

    // ---- Asymmetric exit (#6 "close only when we're actually ahead") ----
    /// When true, the convergence-exit branch closes only if the projected
    /// net pnl (after spreads + fee) would be positive. Stop-loss and
    /// hard-timeout still fire unconditionally.
    pub pairs_asymmetric_exit: bool,

    /// Tag written into `position_log.mode` — "paper" for live paper, or
    /// "backtest" during historical replay so the two streams don't mix.
    pub mode_tag: String,
    /// Skip side-loops that don't fit the backtest replay (funding_poller
    /// queries the live funding_rate table, etc.). `run()` gates on this.
    pub backtest_mode: bool,

    // Strategy A
    pub funding_scan_interval_sec: u64,
    pub funding_trigger_bp: f64, // require |diff| > this in bp per funding period
    pub funding_settle_buffer_sec: i64,

    /// Which exchange is the hedge leg paired against Flipster.
    /// `binance` (default) or `gate`. Pairs strategy uses only this venue.
    pub hedge_venue: ExchangeName,

    /// Variant identifier — written to `position_log.account_id` so multiple
    /// parallel paper-bot strategies can be compared by SQL filter.
    pub account_id: String,

    // ---- Slow-average pairs mode (replaces EWMA when enabled) ----
    /// Optional shared handle to the market_watcher's slow-gap state. When
    /// present, try_enter_pairs uses (current_gap - slow_avg) instead of the
    /// EWMA-based deviation. Eliminates the "mean drifts toward entry" bug.
    pub gap_state: Option<SharedGapState>,
    /// Absolute bp threshold to open on `|dev|`. Only used when gap_state is
    /// Some. 0 disables.
    pub pairs_entry_bp: f64,
    /// Absolute bp threshold to close on `|dev|`. Only used when gap_state is
    /// Some.
    pub pairs_exit_bp: f64,
    /// Require at least this many of (1s, 5s, 10s, 20s) snapshots to show
    /// narrowing deviation before opening. 0 disables the filter.
    pub pairs_snapshot_confirm_min: usize,
    /// Override for sizing mode. When Some(f), every pairs trade uses a fixed
    /// USD notional and Kelly is bypassed. Enables the gate_hft-style
    /// fixed-size comparison variant.
    pub pairs_fixed_size_usd: Option<f64>,

    // ---- Kelly sizing ----
    /// Starting bankroll per variant (USDT). Each variant compounds independently.
    pub bankroll_usd: f64,
    /// Multiplier on f* (1.0 = full Kelly, 0.25 = quarter-Kelly).
    pub kelly_fraction: f64,
    /// Max leverage cap on f* (e.g. 5.0 = at most 5x bankroll per trade).
    pub kelly_max_leverage: f64,
    /// Trades per symbol before switching from bootstrap to Kelly.
    pub kelly_min_samples: usize,
    /// Bootstrap sizing as fraction of bankroll (e.g. 0.001 = 0.1%).
    pub kelly_bootstrap_frac: f64,
}

impl Params {
    pub fn from_env() -> Self {
        let fget = |k: &str, d: f64| -> f64 {
            std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d)
        };
        let iget = |k: &str, d: i64| -> i64 {
            std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d)
        };
        let uget = |k: &str, d: u64| -> u64 {
            std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d)
        };
        Self {
            fee_bp: fget("PARAM_FEE_BP", 0.375),
            size: fget("PARAM_SIZE", 1.0),
            max_mid_age_ms: iget("MAX_MID_AGE_MS", 10_000),
            max_entry_spread_bp: fget("MAX_ENTRY_SPREAD_BP", 2.0),
            max_concurrent_positions: uget("MAX_CONCURRENT_POSITIONS", 50) as usize,
            blacklist: std::env::var("PAIRS_BLACKLIST")
                .ok()
                .map(|s| {
                    s.split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            pairs_invert: std::env::var("PAIRS_INVERT")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),

            latency_trigger_bp: fget("LATENCY_TRIGGER_BP", 1.0),
            latency_hold_ms: iget("LATENCY_HOLD_MS", 3_000),
            latency_cooldown_ms: iget("LATENCY_COOLDOWN_MS", 5_000),

            pairs_ewma_tau_sec: fget("PAIRS_EWMA_TAU_SEC", 60.0),
            pairs_entry_sigma: fget("PAIRS_ENTRY_SIGMA", 1.5),
            pairs_exit_sigma: fget("PAIRS_EXIT_SIGMA", 0.5),
            pairs_stop_sigma: fget("PAIRS_STOP_SIGMA", 3.0),
            pairs_max_hold_ms: iget("PAIRS_MAX_HOLD_MS", 30_000),
            pairs_min_hold_ms: iget("PAIRS_MIN_HOLD_MS", 0),
            pairs_min_std_bp: fget("PAIRS_MIN_STD_BP", 0.5),
            pairs_cooldown_ms: iget("PAIRS_COOLDOWN_MS", 2_000),

            paper_fill_latency_ms: iget("PAPER_FILL_LATENCY_MS", 50),

            pairs_spread_edge_safety:     fget("PAIRS_SPREAD_EDGE_SAFETY", 0.0),
            pairs_bn_change_cooldown_ms:  iget("PAIRS_BN_CHANGE_COOLDOWN_MS", 0),
            pairs_rolling_window_sec:     fget("PAIRS_ROLLING_WINDOW_SEC", 0.0),
            pairs_auto_blacklist_min_trades: uget("PAIRS_AUTO_BLACKLIST_MIN_TRADES", 0) as usize,
            pairs_auto_blacklist_max_avg_bp: fget("PAIRS_AUTO_BLACKLIST_MAX_AVG_BP", -2.0),
            pairs_auto_blacklist_duration_ms: iget("PAIRS_AUTO_BLACKLIST_DURATION_MS", 600_000),
            pairs_asymmetric_exit: std::env::var("PAIRS_ASYMMETRIC_EXIT")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            mode_tag: std::env::var("PAPER_MODE_TAG").unwrap_or_else(|_| "paper".into()),
            backtest_mode: std::env::var("BACKTEST_MODE")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),

            funding_scan_interval_sec: uget("FUNDING_SCAN_INTERVAL_SEC", 300),
            funding_trigger_bp: fget("FUNDING_TRIGGER_BP", 5.0),
            funding_settle_buffer_sec: iget("FUNDING_SETTLE_BUFFER_SEC", 10),

            hedge_venue: match std::env::var("PAIRS_HEDGE_VENUE")
                .unwrap_or_default()
                .to_lowercase()
                .as_str()
            {
                "gate" => ExchangeName::Gate,
                "hyperliquid" | "hl" => ExchangeName::Hyperliquid,
                _ => ExchangeName::Binance,
            },

            account_id: std::env::var("PAPER_VARIANT_ID")
                .unwrap_or_else(|_| "paper".into()),

            gap_state: None,
            pairs_entry_bp: fget("PAIRS_ENTRY_BP", 5.0),
            pairs_exit_bp: fget("PAIRS_EXIT_BP", 1.0),
            pairs_snapshot_confirm_min: uget("PAIRS_SNAPSHOT_CONFIRM_MIN", 0) as usize,
            pairs_fixed_size_usd: std::env::var("PAIRS_FIXED_SIZE_USD")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|v| *v > 0.0),

            bankroll_usd: fget("BANKROLL_USD", 10_000.0),
            kelly_fraction: fget("KELLY_FRACTION", 0.25),
            kelly_max_leverage: fget("KELLY_MAX_LEVERAGE", 5.0),
            kelly_min_samples: uget("KELLY_MIN_SAMPLES", 20) as usize,
            kelly_bootstrap_frac: fget("KELLY_BOOTSTRAP_FRAC", 0.001),
        }
    }
}

// -----------------------------------------------------------------------------
// shared state
// -----------------------------------------------------------------------------

// `Quote` and `RollingWindow` now live in `pairs_core` (same shape, single
// source of truth across collector/executor/future strategies).

#[derive(Debug)]
pub struct EwmaStats {
    pub alpha: f64,
    pub initialized: bool,
    pub mean: f64,
    pub var: f64,
    pub last_update: Option<DateTime<Utc>>,
}

impl EwmaStats {
    fn new(tau_sec: f64) -> Self {
        Self {
            // Placeholder alpha — real alpha is computed per-update from dt.
            alpha: (1.0 - (-1.0 / tau_sec).exp()),
            initialized: false,
            mean: 0.0,
            var: 0.0,
            last_update: None,
        }
    }

    fn update(&mut self, x: f64, ts: DateTime<Utc>, tau_sec: f64) {
        let dt = self
            .last_update
            .map(|t| (ts - t).num_milliseconds() as f64 / 1000.0)
            .unwrap_or(0.0)
            .max(0.0);
        let a = 1.0 - (-dt / tau_sec).exp();
        let a = if !self.initialized { 1.0 } else { a.clamp(0.0, 1.0) };
        if !self.initialized {
            self.mean = x;
            self.var = 0.0;
            self.initialized = true;
        } else {
            let delta = x - self.mean;
            self.mean += a * delta;
            self.var = (1.0 - a) * (self.var + a * delta * delta);
        }
        self.last_update = Some(ts);
    }

    fn std(&self) -> f64 {
        self.var.max(0.0).sqrt()
    }
}

// (RollingWindow now lives in `pairs_core::window`; imported above.)

#[derive(Debug)]
pub struct BinanceWindow {
    pub pts: VecDeque<(DateTime<Utc>, f64)>, // (ts, mid)
    pub last_entry_ts: Option<DateTime<Utc>>,
}

impl BinanceWindow {
    fn new() -> Self {
        Self { pts: VecDeque::with_capacity(64), last_entry_ts: None }
    }
    fn push(&mut self, ts: DateTime<Utc>, mid: f64) {
        self.pts.push_back((ts, mid));
        while let Some(&(t0, _)) = self.pts.front() {
            if (ts - t0) > Duration::seconds(2) {
                self.pts.pop_front();
            } else {
                break;
            }
        }
    }
    fn return_bp_over(&self, win_ms: i64) -> Option<f64> {
        let (_, now_mid) = self.pts.back()?;
        let cutoff = self.pts.back()?.0 - Duration::milliseconds(win_ms);
        let (_, past_mid) = self.pts.iter().find(|(t, _)| *t >= cutoff)?;
        if *past_mid <= 0.0 {
            return None;
        }
        Some(((now_mid - past_mid) / past_mid) * 10_000.0)
    }
}

// -----------------------------------------------------------------------------
// positions
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum Strategy {
    Latency,
    Pairs,
    Funding,
}

impl Strategy {
    fn label(&self) -> &'static str {
        match self {
            Self::Latency => "latency",
            Self::Pairs => "pairs",
            Self::Funding => "funding",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenPosition {
    pub id: u64,
    pub strategy: Strategy,
    pub base: String,
    /// +1 = long flipster / short binance;  -1 = short flipster / long binance
    pub flipster_side: i32,
    pub flipster_entry: f64,
    pub binance_entry: f64,
    pub entry_time: DateTime<Utc>,

    /// USD notional used for this trade (per leg). Set by Kelly sizing at entry,
    /// used to convert pnl_bp → $ pnl at exit.
    pub size_usd: f64,

    pub hard_timeout: DateTime<Utc>,

    // Strategy-specific exit metadata
    pub entry_spread_bp: Option<f64>,     // for pairs
    pub pairs_mean_bp: Option<f64>,       // mean at entry (EWMA or slow_avg)
    pub pairs_std_bp: Option<f64>,        // std at entry (EWMA mode only)
    pub stop_deviation_bp: Option<f64>,   // pairs stop-loss threshold
    /// Slow-avg mode: absolute bp threshold for convergence exit. When Some,
    /// sweep_exits uses this instead of `exit_sigma × std`.
    pub pairs_exit_bp: Option<f64>,
    pub settle_time: Option<DateTime<Utc>>, // for funding arb

    // Stored for funding PnL attribution
    pub flipster_rate_entry: Option<f64>,
    pub binance_rate_entry: Option<f64>,

    /// Paper-execution latency model. When Some, the position's
    /// `flipster_entry` / `binance_entry` are placeholders until `now` reaches
    /// this instant; at that point the sweep loop freezes them to the prevailing
    /// bid/ask. Only used by the pairs strategy.
    pub entry_fill_at: Option<DateTime<Utc>>,
    /// Paper-execution latency model for exits. When Some, the position has
    /// been flagged for close but the close prices are frozen at this instant,
    /// not at the moment the exit condition was detected.
    pub exit_fill_at: Option<DateTime<Utc>>,
}

fn net_pnl_bp(pos: &OpenPosition, f_exit: f64, b_exit: f64, fee_bp: f64) -> f64 {
    pair_pnl_bp(
        pos.flipster_entry,
        f_exit,
        pos.binance_entry,
        b_exit,
        pos.flipster_side,
    ) - fee_bp
}

// -----------------------------------------------------------------------------
// shared book
// -----------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct PaperBook {
    pub binance_mids: HashMap<String, Quote>,
    pub flipster_mids: HashMap<String, Quote>,
    pub pair_stats: HashMap<String, EwmaStats>,
    pub binance_win: HashMap<String, BinanceWindow>,
    pub positions: Vec<OpenPosition>,
    // Position IDs come from pairs_core::pos_id::global(); no per-PaperBook
    // counter is needed any longer (the global gen is process-monotonic).
    // cooldowns[(strategy, base)] = next allowed entry time
    pub cooldowns: HashMap<(&'static str, String), DateTime<Utc>>,

    // Kelly sizing state (per variant).
    /// Compounding bankroll, updated after every closed trade.
    pub bankroll_usd: f64,
    /// Per-symbol rolling realized stats: (n, sum_pnl_bp, sum_pnl_bp_squared).
    pub symbol_stats: HashMap<String, (usize, f64, f64)>,

    /// Ring buffer of paired bookticker snapshots (last ~30s) used for
    /// multi-timeframe confirmation filters in the "slow_avg" pairs variant.
    /// Keyed by base; oldest entries evicted once window exceeds 30s.
    pub snapshot_history: HashMap<String, VecDeque<PairSnapshot>>,

    // ---- Rolling window alt signal (#4) ----
    /// Per-symbol rolling mean/std of diff_bp, maintained in parallel with
    /// `pair_stats` (EWMA). Variants pick which to use via
    /// `Params.pairs_rolling_window_sec`.
    pub pair_stats_rolling: HashMap<String, RollingWindow>,

    // ---- Binance change tracking (#3 stealth) ----
    /// Last seen hedge-leg quote per symbol (bid, ask). Used to detect price
    /// changes for the stealth cooldown.
    pub bn_last_quote: HashMap<String, (f64, f64)>,
    /// Timestamp of the most recent hedge-leg quote change per symbol.
    pub bn_last_change: HashMap<String, DateTime<Utc>>,

    // ---- Auto-blacklist (#5) ----
    /// Symbol → instant at which the auto-blacklist expires. Entries with
    /// `now < until` are skipped in `try_enter_pairs`.
    pub auto_blacklist_until: HashMap<String, DateTime<Utc>>,

    /// Simulated "current time", driven from the latest tick ts we've seen.
    /// In live mode this tracks wall-clock approximately. In backtest mode
    /// it is the replay's current instant, and `sweep_exits` uses it instead
    /// of `Utc::now()` so hard_timeout / min_hold / fill_at checks compare
    /// against market time rather than wall clock.
    pub sim_now: Option<DateTime<Utc>>,
    /// sim_now at which sweep_exits last ran. Backtest mode uses this to
    /// fire a sweep whenever >= 100ms of simulated time has elapsed since
    /// the previous sweep, instead of relying on a wall-clock interval.
    pub last_sweep_sim_now: Option<DateTime<Utc>>,
}

/// Single paired observation at one wall-clock time.
#[derive(Debug, Clone)]
pub struct PairSnapshot {
    pub ts: DateTime<Utc>,
    pub flipster: Quote,
    pub hedge: Quote,
}

impl PaperBook {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_bankroll(bankroll_usd: f64) -> Self {
        let mut b = Self::default();
        b.bankroll_usd = bankroll_usd;
        b
    }
}

/// Kelly sizing: returns USD notional to deploy on this trade. 0 = skip entry
/// (no positive edge yet observed for this symbol).
fn kelly_size_usd(book: &PaperBook, base: &str, params: &Params) -> f64 {
    // Available capital = bankroll minus notional already committed to open positions.
    // Without this deduction, each new position is sized against the full bankroll
    // regardless of how many positions are already open, causing capital double-counting.
    let open_notional: f64 = book
        .positions
        .iter()
        .filter(|p| matches!(p.strategy, Strategy::Pairs))
        .map(|p| p.size_usd)
        .sum();
    let available = (book.bankroll_usd - open_notional).max(0.0);

    let stats = book.symbol_stats.get(base).copied().unwrap_or((0, 0.0, 0.0));
    let n = stats.0;
    if n < params.kelly_min_samples {
        return available * params.kelly_bootstrap_frac;
    }
    let mean = stats.1 / n as f64;
    if mean <= 0.0 {
        // Negative or zero observed edge → don't trade this symbol under Kelly.
        return 0.0;
    }
    let nf = n as f64;
    let variance = (stats.2 - stats.1 * stats.1 / nf) / (nf - 1.0).max(1.0);
    if variance <= 0.0 {
        return 0.0;
    }
    // f* in fractional-return units: (mean_bp/1e4) / (var_bp/1e8) = mean_bp * 1e4 / var_bp
    let f_star = (mean * 10_000.0 / variance) * params.kelly_fraction;
    let f_capped = f_star.clamp(0.0, params.kelly_max_leverage);
    available * f_capped
}

// -----------------------------------------------------------------------------
// helpers: symbol normalization
// -----------------------------------------------------------------------------

/// Local thunk over `pairs_core::base_of` so existing call sites that say
/// `base_of(ex, s)` keep working. Pairs strategies only run against
/// Flipster/Binance/Gate ticks; non-USDT and other venues map to None.
fn base_of(exchange: ExchangeName, symbol: &str) -> Option<String> {
    pairs_core_base_of(exchange, symbol)
}

// -----------------------------------------------------------------------------
// strategy engines
// -----------------------------------------------------------------------------

pub async fn run(
    writer: IlpWriter,
    tick_rx: broadcast::Receiver<BookTick>,
    params: Params,
) {
    let book = Arc::new(Mutex::new(PaperBook::with_bankroll(params.bankroll_usd)));

    // A: funding scan loop. Skipped in backtest mode because it polls live
    // funding_rate tables and uses wall-clock `now`, which would produce
    // phantom positions during historical replay.
    if !params.backtest_mode {
        tokio::spawn(funding_loop(
            book.clone(),
            writer.clone(),
            params.clone(),
        ));
    }

    // B + D share the tick stream. Exit sweeper runs alongside.
    // Live mode uses a wall-clock 100ms timer; backtest mode instead drives
    // sweeps from the tick loop itself (see on_tick) so they align with the
    // replay's simulated clock.
    if !params.backtest_mode {
        let book_exit = book.clone();
        let writer_exit = writer.clone();
        let params_exit = params.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                tick.tick().await;
                sweep_exits(&book_exit, &writer_exit, &params_exit).await;
            }
        });
    }

    tick_loop(book, writer, tick_rx, params).await;
}

/// Main tick ingest loop. Updates mids, feeds Strategy B and D entry logic.
async fn tick_loop(
    book: Arc<Mutex<PaperBook>>,
    writer: IlpWriter,
    mut rx: broadcast::Receiver<BookTick>,
    params: Params,
) {
    loop {
        match rx.recv().await {
            Ok(t) => {
                if let Err(e) = on_tick(&book, &writer, &t, &params).await {
                    warn!(error = %e, "paper_bot on_tick err");
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(missed = n, "paper_bot tick lagged");
            }
            Err(_) => break,
        }
    }
}

async fn on_tick(
    book: &Arc<Mutex<PaperBook>>,
    writer: &IlpWriter,
    tick: &BookTick,
    params: &Params,
) -> Result<()> {
    let bid = tick.bid_price;
    let ask = tick.ask_price;
    if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask <= 0.0 {
        return Ok(());
    }
    let mid = (bid + ask) * 0.5;
    let Some(base) = base_of(tick.exchange, &tick.symbol) else {
        return Ok(());
    };
    let ts = tick.timestamp;

    let mut b = book.lock().await;
    // Advance the sim clock from the latest tick we've seen. sweep_exits and
    // the paper-fill latency logic both consult `sim_now` so that backtest
    // replays use market time instead of wall clock.
    match b.sim_now {
        Some(prev) if prev >= ts => {}
        _ => b.sim_now = Some(ts),
    }

    match tick.exchange {
        e if e == params.hedge_venue => {
            // Hedge-leg price-change tracking for the stealth cooldown (#3).
            let cur = (bid, ask);
            let prev = b.bn_last_quote.get(&base).copied();
            if prev.map_or(true, |p| p != cur) {
                b.bn_last_quote.insert(base.clone(), cur);
                b.bn_last_change.insert(base.clone(), ts);
            }
            b.binance_mids.insert(base.clone(), Quote { bid, ask, ts });
            b.binance_win
                .entry(base.clone())
                .or_insert_with(BinanceWindow::new)
                .push(ts, mid);
        }
        ExchangeName::Flipster => {
            let fq = Quote { bid, ask, ts };
            b.flipster_mids.insert(base.clone(), fq.clone());
            // update pair stats + snapshot history if we have a hedge mid
            if let Some(bin) = b.binance_mids.get(&base).cloned() {
                let diff_bp = (mid - bin.mid()) / bin.mid() * 10_000.0;
                let stats = b
                    .pair_stats
                    .entry(base.clone())
                    .or_insert_with(|| EwmaStats::new(params.pairs_ewma_tau_sec));
                stats.update(diff_bp, ts, params.pairs_ewma_tau_sec);

                // Rolling window alt signal (#4). We always maintain it in
                // parallel with the EWMA so variants can opt-in per-params.
                // Use a non-zero default window so the data is ready when a
                // variant flips to rolling mode.
                let win_sec = if params.pairs_rolling_window_sec > 0.0 {
                    params.pairs_rolling_window_sec
                } else {
                    30.0
                };
                let roll = b
                    .pair_stats_rolling
                    .entry(base.clone())
                    .or_insert_with(|| RollingWindow::new(win_sec));
                roll.push(ts, diff_bp);

                // Ring buffer of paired snapshots (last ~30s) for multi-timeframe
                // confirmation in slow-avg mode. Evict older than 30s.
                let hist = b
                    .snapshot_history
                    .entry(base.clone())
                    .or_insert_with(VecDeque::new);
                hist.push_back(PairSnapshot {
                    ts,
                    flipster: fq,
                    hedge: bin,
                });
                while let Some(front) = hist.front() {
                    if (ts - front.ts).num_milliseconds() > 30_000 {
                        hist.pop_front();
                    } else {
                        break;
                    }
                }
            }
        }
        _ => return Ok(()),
    }

    // Strategy B evaluates on binance ticks (binance is the trigger leg).
    try_enter_latency(&mut b, writer, &base, tick.exchange, params).await?;
    // Strategy D must evaluate ONLY on flipster ticks — evaluating on
    // binance ticks while flipster mid is stale creates phantom spread
    // deviations (binance moves, flipster stuck, apparent deviation that
    // isn't real). This matches the offline replay that uses aligned 1s
    // buckets.
    if matches!(tick.exchange, ExchangeName::Flipster) {
        try_enter_pairs(&mut b, writer, &base, params).await?;
    }

    // Backtest mode: drive sweep_exits from the tick stream so it aligns
    // with sim_now (there's no wall-clock sweep task in this mode). Fire
    // whenever >= 100ms of simulated time has passed since the previous
    // sweep. In live mode the dedicated timer task handles this instead.
    if params.backtest_mode {
        let due = match (b.sim_now, b.last_sweep_sim_now) {
            (Some(now), Some(last)) => (now - last).num_milliseconds() >= 100,
            (Some(_), None) => true,
            _ => false,
        };
        if due {
            b.last_sweep_sim_now = b.sim_now;
            drop(b);
            sweep_exits(book, writer, params).await;
        }
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Strategy B: latency
// -----------------------------------------------------------------------------

async fn try_enter_latency(
    b: &mut PaperBook,
    _writer: &IlpWriter,
    base: &str,
    src: ExchangeName,
    params: &Params,
) -> Result<()> {
    // Only fire the evaluator on a binance tick (binance is the trigger leg).
    if src != ExchangeName::Binance {
        return Ok(());
    }
    let now = match b.binance_mids.get(base) {
        Some(q) => q.ts,
        None => return Ok(()),
    };
    // cooldown
    if let Some(until) = b.cooldowns.get(&("latency", base.to_string())).copied() {
        if now < until {
            return Ok(());
        }
    }
    // capital cap
    if b.positions.len() >= params.max_concurrent_positions {
        return Ok(());
    }
    // already open?
    if b.positions.iter().any(|p| matches!(p.strategy, Strategy::Latency) && p.base == base) {
        return Ok(());
    }
    let win = match b.binance_win.get(base) {
        Some(w) => w,
        None => return Ok(()),
    };
    let Some(ret_bp) = win.return_bp_over(1_000) else {
        return Ok(());
    };
    if ret_bp.abs() < params.latency_trigger_bp {
        return Ok(());
    }
    // need live quotes on both sides; refuse if either is stale
    let (fq, bq) = match (b.flipster_mids.get(base), b.binance_mids.get(base)) {
        (Some(f), Some(bin)) => (f.clone(), bin.clone()),
        _ => return Ok(()),
    };
    if (now - fq.ts).num_milliseconds() > params.max_mid_age_ms {
        return Ok(());
    }
    // Spread filter: if crossing the book would cost more than the expected
    // latency edge, don't bother entering. Major pairs have 0 bp spread;
    // long-tail flipster symbols have 10+ bp and are structurally untradeable.
    if fq.spread_bp() > params.max_entry_spread_bp
        || bq.spread_bp() > params.max_entry_spread_bp
    {
        return Ok(());
    }
    // open: same direction as binance move, on flipster (flipster will catch up)
    let dir = if ret_bp > 0.0 { 1 } else { -1 };
    // Cross the book at entry: long → pay flipster ask and sell binance bid,
    // short → sell flipster bid and buy binance ask. Hedge leg is opposite.
    let f_entry = if dir > 0 { fq.buy_at() } else { fq.sell_at() };
    let b_entry = if dir > 0 { bq.sell_at() } else { bq.buy_at() };
    let pos = OpenPosition {
        id: pairs_core::pos_id::global().next(),
        strategy: Strategy::Latency,
        base: base.to_string(),
        flipster_side: dir,
        flipster_entry: f_entry,
        binance_entry: b_entry,
        entry_time: now,
        size_usd: params.size,
        hard_timeout: now + Duration::milliseconds(params.latency_hold_ms),
        entry_spread_bp: None,
        pairs_mean_bp: None,
        pairs_std_bp: None,
        stop_deviation_bp: None,
        pairs_exit_bp: None,
        settle_time: None,
        flipster_rate_entry: None,
        binance_rate_entry: None,
        entry_fill_at: None,
        exit_fill_at: None,
    };
    info!(
        strategy = "latency",
        base = %base,
        dir,
        ret_bp = format!("{:+.2}", ret_bp),
        "paper open"
    );
    b.positions.push(pos);
    b.cooldowns.insert(
        ("latency", base.to_string()),
        now + Duration::milliseconds(params.latency_cooldown_ms),
    );
    Ok(())
}

// -----------------------------------------------------------------------------
// Strategy D: pairs reversion
// -----------------------------------------------------------------------------

async fn try_enter_pairs(
    b: &mut PaperBook,
    _writer: &IlpWriter,
    base: &str,
    params: &Params,
) -> Result<()> {
    let now = match b.flipster_mids.get(base) {
        Some(q) => q.ts,
        None => return Ok(()),
    };
    if params.blacklist.contains(base) {
        return Ok(());
    }
    // Auto-blacklist (#5): symbols the bot has empirically lost on are
    // silenced for `auto_blacklist_duration_ms` once the threshold is hit.
    if let Some(until) = b.auto_blacklist_until.get(base).copied() {
        if now < until {
            return Ok(());
        }
    }
    if let Some(until) = b.cooldowns.get(&("pairs", base.to_string())).copied() {
        if now < until {
            return Ok(());
        }
    }
    // Binance-change stealth cooldown (#3): avoid the window right after a
    // hedge-leg price change, where adverse selection is highest.
    if params.pairs_bn_change_cooldown_ms > 0 {
        if let Some(last) = b.bn_last_change.get(base).copied() {
            if (now - last).num_milliseconds() < params.pairs_bn_change_cooldown_ms {
                return Ok(());
            }
        }
    }
    // capital cap
    if b.positions.len() >= params.max_concurrent_positions {
        return Ok(());
    }
    if b.positions.iter().any(|p| matches!(p.strategy, Strategy::Pairs) && p.base == base) {
        return Ok(());
    }
    let (fq, bq) = match (b.flipster_mids.get(base), b.binance_mids.get(base)) {
        (Some(f), Some(bin)) => (f.clone(), bin.clone()),
        _ => return Ok(()),
    };
    if (now - fq.ts).num_milliseconds() > params.max_mid_age_ms
        || (now - bq.ts).num_milliseconds() > params.max_mid_age_ms
    {
        return Ok(());
    }
    if fq.spread_bp() > params.max_entry_spread_bp
        || bq.spread_bp() > params.max_entry_spread_bp
    {
        return Ok(());
    }

    // Choose between the legacy EWMA path and the new slow-avg + snapshot
    // confirmation path. slow-avg mode is enabled when a gap_state handle is
    // configured on this variant.
    let diff_bp = (fq.mid() - bq.mid()) / bq.mid() * 10_000.0;

    let (mean_bp, std_bp_opt, deviation, is_slow_avg) = if let Some(ref gs) = params.gap_state {
        let slow_avg = gs
            .read()
            .ok()
            .and_then(|g| g.get_avg_gap_bp(base));
        let Some(slow_avg) = slow_avg else {
            // Not yet populated for this symbol — skip rather than fall back
            // to zero, otherwise unbiased symbols would always look extreme
            // at startup.
            return Ok(());
        };
        let dev = diff_bp - slow_avg;
        if dev.abs() < params.pairs_entry_bp {
            return Ok(());
        }
        (slow_avg, None, dev, true)
    } else if params.pairs_rolling_window_sec > 0.0 {
        // Rolling-window alt signal (#4). Requires at least 40 samples and a
        // meaningful standard deviation before it fires. Mean no longer
        // drifts toward outliers the way EWMA does.
        let (stats_mean, std_bp) = match b.pair_stats_rolling.get(base) {
            Some(r) if r.len() >= 40 => match (r.mean(), r.std()) {
                (Some(m), Some(s)) => (m, s),
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };
        if std_bp < params.pairs_min_std_bp {
            return Ok(());
        }
        let dev = diff_bp - stats_mean;
        if dev.abs() < params.pairs_entry_sigma * std_bp {
            return Ok(());
        }
        (stats_mean, Some(std_bp), dev, false)
    } else {
        // Legacy EWMA path
        let (stats_mean, std_bp) = match b.pair_stats.get(base) {
            Some(s) if s.initialized => (s.mean, s.std()),
            _ => return Ok(()),
        };
        if std_bp < params.pairs_min_std_bp {
            return Ok(());
        }
        let dev = diff_bp - stats_mean;
        if dev.abs() < params.pairs_entry_sigma * std_bp {
            return Ok(());
        }
        (stats_mean, Some(std_bp), dev, false)
    };

    // Spread-aware cost filter (#1). Compare realistic captured edge to the
    // round-trip mechanical cost (both leg spreads + round-trip fee) with a
    // configurable safety margin. Biggest single change — structurally
    // refuses trades where the numbers cannot possibly work.
    if params.pairs_spread_edge_safety > 0.0 {
        let exit_band_bp = if is_slow_avg {
            params.pairs_exit_bp
        } else if let Some(std_bp) = std_bp_opt {
            params.pairs_exit_sigma * std_bp
        } else {
            0.0
        };
        // Potential bp captured = distance from current deviation to the
        // exit band (assuming a clean reversion). Negative values (exit band
        // already inside us) are clamped to zero and will always fail.
        let potential_edge_bp = (deviation.abs() - exit_band_bp).max(0.0);
        let total_cost_bp = fq.spread_bp() + bq.spread_bp() + params.fee_bp;
        if potential_edge_bp < total_cost_bp * params.pairs_spread_edge_safety {
            return Ok(());
        }
    }

    // Multi-timeframe snapshot confirmation (slow-avg mode only). For each of
    // the 1s/5s/10s/20s lookbacks we require the deviation to be shrinking
    // vs the previous snapshot (already converging toward mean).
    if is_slow_avg && params.pairs_snapshot_confirm_min > 0 {
        let hist = b.snapshot_history.get(base);
        let mut clear: usize = 0;
        let mut checked: usize = 0;
        for offset_ms in [1_000i64, 5_000, 10_000, 20_000] {
            let target = now - Duration::milliseconds(offset_ms);
            let prev = hist.and_then(|h| {
                h.iter().rev().find(|s| s.ts <= target)
            });
            if let Some(prev) = prev {
                checked += 1;
                let prev_diff_bp =
                    (prev.flipster.mid() - prev.hedge.mid()) / prev.hedge.mid() * 10_000.0;
                let prev_dev = prev_diff_bp - mean_bp;
                // "Narrowing": current dev magnitude < previous dev magnitude
                if deviation.abs() < prev_dev.abs() {
                    clear += 1;
                }
            }
        }
        // Require N/4 snapshots confirming. If we couldn't find any snapshots
        // (cold start) fall through without blocking.
        if checked > 0 && clear < params.pairs_snapshot_confirm_min {
            return Ok(());
        }
    }

    // Default (reversion): diff above mean → short flipster, diff below → long.
    let dir = if params.pairs_invert {
        if deviation > 0.0 { 1 } else { -1 }
    } else if deviation > 0.0 { -1 } else { 1 };

    // Sizing: fixed override takes precedence over Kelly.
    let size_usd = if let Some(fixed) = params.pairs_fixed_size_usd {
        fixed
    } else {
        kelly_size_usd(b, base, params)
    };
    if size_usd <= 0.0 {
        return Ok(());
    }

    // Decision-time book cross (used as a placeholder when paper fill latency
    // is enabled — the actual entry prices are frozen later, in sweep_exits,
    // at `entry_fill_at`).
    let f_entry = if dir > 0 { fq.buy_at() } else { fq.sell_at() };
    let b_entry = if dir > 0 { bq.sell_at() } else { bq.buy_at() };
    let entry_fill_at = if params.paper_fill_latency_ms > 0 {
        Some(now + Duration::milliseconds(params.paper_fill_latency_ms))
    } else {
        None
    };
    // Stop-loss level: in slow-avg mode use an absolute bp offset so stop
    // semantics are consistent with entry; in EWMA mode keep the
    // sigma-based calculation.
    let stop_dev_bp = if is_slow_avg {
        mean_bp + deviation.signum() * params.pairs_entry_bp * 4.0
    } else if let Some(std_bp) = std_bp_opt {
        mean_bp + deviation.signum() * params.pairs_stop_sigma * std_bp
    } else {
        mean_bp
    };

    let pos = OpenPosition {
        id: pairs_core::pos_id::global().next(),
        strategy: Strategy::Pairs,
        base: base.to_string(),
        flipster_side: dir,
        flipster_entry: f_entry,
        binance_entry: b_entry,
        entry_time: now,
        size_usd,
        hard_timeout: now + Duration::milliseconds(params.pairs_max_hold_ms),
        entry_spread_bp: Some(diff_bp),
        pairs_mean_bp: Some(mean_bp),
        pairs_std_bp: std_bp_opt,
        stop_deviation_bp: Some(stop_dev_bp),
        pairs_exit_bp: if is_slow_avg { Some(params.pairs_exit_bp) } else { None },
        settle_time: None,
        flipster_rate_entry: None,
        binance_rate_entry: None,
        entry_fill_at,
        exit_fill_at: None,
    };
    debug!(
        variant = %params.account_id,
        strategy = "pairs",
        base = %base,
        dir,
        diff_bp = format!("{:+.2}", diff_bp),
        mean = format!("{:+.2}", mean_bp),
        dev = format!("{:+.2}", deviation),
        size_usd = format!("{:.0}", size_usd),
        mode = if is_slow_avg { "slow_avg" } else { "ewma" },
        "paper open"
    );
    let pos_id = pos.id;
    b.positions.push(pos);
    b.cooldowns.insert(
        ("pairs", base.to_string()),
        now + Duration::milliseconds(params.pairs_cooldown_ms),
    );
    // Emit entry signal for live executor sidecar.
    let side_s = if dir > 0 { "long" } else { "short" };
    // Carry the freshest BBO so the executor doesn't re-fetch from
    // QuestDB. `bq` is whichever venue is the hedge leg (binance or gate).
    let mut quotes = crate::signal_publisher::SignalQuotes::default();
    quotes.flipster_bid = Some(fq.bid);
    quotes.flipster_ask = Some(fq.ask);
    match params.hedge_venue {
        ExchangeName::Binance => {
            quotes.binance_bid = Some(bq.bid);
            quotes.binance_ask = Some(bq.ask);
        }
        ExchangeName::Gate => {
            quotes.gate_bid = Some(bq.bid);
            quotes.gate_ask = Some(bq.ask);
        }
        _ => {}
    }
    // ZMQ PUB first (non-blocking, <1ms) so live executor sees it with minimal lag.
    crate::coordinator::route_signal(
        &params.account_id, base, "entry", side_s,
        size_usd, f_entry, b_entry, pos_id, now,
        quotes,
    );
    if let Err(e) = _writer
        .write_trade_signal(
            &params.account_id,
            base,
            "entry",
            side_s,
            size_usd,
            f_entry,
            b_entry,
            pos_id,
            now,
        )
        .await
    {
        warn!(error = %e, "trade_signal entry write failed");
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Strategy A: funding
// -----------------------------------------------------------------------------

async fn funding_loop(
    book: Arc<Mutex<PaperBook>>,
    writer: IlpWriter,
    params: Params,
) {
    let mut iv = tokio::time::interval(std::time::Duration::from_secs(
        params.funding_scan_interval_sec,
    ));
    loop {
        iv.tick().await;
        if let Err(e) = funding_scan(&book, &writer, &params).await {
            warn!(error = %e, "funding_scan err");
        }
    }
}

async fn funding_scan(
    book: &Arc<Mutex<PaperBook>>,
    _writer: &IlpWriter,
    params: &Params,
) -> Result<()> {
    // Query QuestDB HTTP API for current funding snapshot.
    // symbol form on binance: "BTCUSDT"; on flipster: "BTCUSDT.PERP".
    let sql = "SELECT symbol, last(rate) rate, last(next_funding_time) next_ft \
               FROM funding_rate WHERE exchange='flipster' GROUP BY symbol";
    let flip_rows = query_qdb(sql).await?;
    let sql_b = "SELECT symbol, last(rate) rate FROM funding_rate \
                 WHERE exchange='binance' GROUP BY symbol";
    let bin_rows = query_qdb(sql_b).await?;

    let bin_map: HashMap<String, f64> = bin_rows
        .into_iter()
        .filter_map(|row| {
            let sym = row.first()?.as_str()?.to_string();
            let rate = row.get(1)?.as_f64()?;
            Some((sym, rate))
        })
        .collect();

    let now = Utc::now();
    let mut opened = 0usize;
    let mut b = book.lock().await;

    for row in flip_rows {
        let Some(sym_str) = row.first().and_then(|x| x.as_str()) else {
            continue;
        };
        let Some(rate_f) = row.get(1).and_then(|x| x.as_f64()) else {
            continue;
        };
        let Some(next_str) = row.get(2).and_then(|x| x.as_str()) else {
            continue;
        };
        // skip non-USDT quote (USD1 etc.)
        if !sym_str.ends_with("USDT.PERP") {
            continue;
        }
        let base = sym_str.trim_end_matches(".PERP").trim_end_matches("USDT");
        if base.is_empty() {
            continue;
        }
        let bin_sym = format!("{base}USDT");
        let Some(rate_b) = bin_map.get(&bin_sym).copied() else {
            continue;
        };
        let diff_bp = (rate_f - rate_b) * 10_000.0;
        if diff_bp.abs() < params.funding_trigger_bp {
            continue;
        }
        // must have live quotes on both sides
        let (fq, bq) = match (b.flipster_mids.get(base), b.binance_mids.get(base)) {
            (Some(f), Some(bin)) => (f.clone(), bin.clone()),
            _ => continue,
        };
        // funding arb direction: if flip_rate > bin_rate → short flipster / long binance → dir = -1
        // Cross the book accordingly.
        let dir = if diff_bp > 0.0 { -1 } else { 1 };
        let f_entry = if dir > 0 { fq.buy_at() } else { fq.sell_at() };
        let b_entry = if dir > 0 { bq.sell_at() } else { bq.buy_at() };
        // skip if already open on same base
        if b
            .positions
            .iter()
            .any(|p| matches!(p.strategy, Strategy::Funding) && p.base == base)
        {
            continue;
        }
        // capital cap
        if b.positions.len() >= params.max_concurrent_positions {
            break;
        }
        // parse next funding time (ISO string)
        let settle = match DateTime::parse_from_rfc3339(next_str) {
            Ok(t) => t.with_timezone(&Utc),
            Err(_) => {
                // QuestDB returns "2026-04-13T12:00:00.000000Z" — RFC3339 compatible
                continue;
            }
        };
        if settle <= now + Duration::seconds(30) {
            // too close to settlement or already past
            continue;
        }
        let pos = OpenPosition {
            id: pairs_core::pos_id::global().next(),
            strategy: Strategy::Funding,
            base: base.to_string(),
            flipster_side: dir,
            flipster_entry: f_entry,
            binance_entry: b_entry,
            entry_time: now,
            size_usd: params.size,
            hard_timeout: settle + Duration::seconds(params.funding_settle_buffer_sec),
            entry_spread_bp: None,
            pairs_mean_bp: None,
            pairs_std_bp: None,
            stop_deviation_bp: None,
            pairs_exit_bp: None,
            settle_time: Some(settle),
            flipster_rate_entry: Some(rate_f),
            binance_rate_entry: Some(rate_b),
            entry_fill_at: None,
            exit_fill_at: None,
        };
        info!(
            strategy = "funding",
            base = %base,
            dir,
            diff_bp = format!("{:+.3}", diff_bp),
            settle = %settle,
            "paper open"
        );
        b.positions.push(pos);
        opened += 1;
    }
    if opened > 0 {
        info!(opened, "funding scan: positions opened");
    }
    Ok(())
}

/// Minimal HTTP call to QuestDB's /exec endpoint. Honors `QUESTDB_HTTP_URL`
/// for remote deployments; defaults to local.
async fn query_qdb(sql: &str) -> Result<Vec<Vec<serde_json::Value>>> {
    let client = reqwest::Client::new();
    let base = std::env::var("QUESTDB_HTTP_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:9000".into());
    let url = format!(
        "{}/exec?query={}",
        base.trim_end_matches('/'),
        urlencoding::encode(sql)
    );
    let v: serde_json::Value = client.get(&url).send().await?.json().await?;
    let dataset = v
        .get("dataset")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(dataset
        .into_iter()
        .filter_map(|r| r.as_array().cloned())
        .collect())
}

// -----------------------------------------------------------------------------
// exit sweeper
// -----------------------------------------------------------------------------

async fn sweep_exits(book: &Arc<Mutex<PaperBook>>, writer: &IlpWriter, params: &Params) {
    let mut to_close: Vec<OpenPosition> = Vec::new();
    let mut b = book.lock().await;
    // Use the sim clock if we have one (backtest replay or live with at
    // least one tick received), otherwise fall back to wall clock. During
    // backtest the first few ticks may arrive before the sweep timer fires
    // — the Utc::now() fallback is fine in that cold-start window because
    // no positions exist yet.
    let now = b.sim_now.unwrap_or_else(Utc::now);
    // Snapshot current mids once for exit evaluation and for writes below.
    let mids_b: HashMap<String, Quote> = b.binance_mids.clone();
    let mids_f: HashMap<String, Quote> = b.flipster_mids.clone();

    let latency_ms = params.paper_fill_latency_ms;
    let mut keep: Vec<OpenPosition> = Vec::with_capacity(b.positions.len());
    for mut pos in b.positions.drain(..) {
        // --- Phase 1: finalize a pending paper entry if its fill_at arrived ---
        if let Some(fa) = pos.entry_fill_at {
            if now < fa {
                // Not yet filled — sit on it. Skip exit evaluation entirely.
                keep.push(pos);
                continue;
            }
            // Fill time reached: freeze entry prices from the latest quotes.
            match (mids_f.get(&pos.base), mids_b.get(&pos.base)) {
                (Some(f), Some(bin))
                    if (now - f.ts).num_milliseconds() <= params.max_mid_age_ms
                        && (now - bin.ts).num_milliseconds() <= params.max_mid_age_ms =>
                {
                    pos.flipster_entry = if pos.flipster_side > 0 { f.buy_at() } else { f.sell_at() };
                    pos.binance_entry  = if pos.flipster_side > 0 { bin.sell_at() } else { bin.buy_at() };
                    pos.entry_time = now;
                    pos.entry_fill_at = None;
                }
                _ => {
                    // Stale or missing quote at fill time — abandon the entry
                    // (never written to position_log, no risk taken).
                    warn!(
                        variant = %params.account_id,
                        base = %pos.base,
                        "stale quote at pending entry fill — dropping intent"
                    );
                    continue;
                }
            }
        }

        // --- Phase 2: if a paper exit is already scheduled, wait for its fill_at ---
        if let Some(fa) = pos.exit_fill_at {
            if now < fa {
                keep.push(pos);
                continue;
            }
            // exit fill reached — close it. The write loop below will
            // recompute f_exit / b_exit from the current mids snapshot.
            to_close.push(pos);
            continue;
        }

        // --- Phase 3: normal exit-condition evaluation ---
        let should_exit = match pos.strategy {
            Strategy::Latency => now >= pos.hard_timeout,
            Strategy::Funding => now >= pos.hard_timeout,
            Strategy::Pairs => {
                // Min-hold guard: refuse all exits (convergence + stop-loss) for
                // the first N ms after entry, so microstructure noise can't trigger
                // immediate close. Hard timeout still wins to bound risk.
                let held_ms = (now - pos.entry_time).num_milliseconds();
                if held_ms < params.pairs_min_hold_ms && now < pos.hard_timeout {
                    false
                } else if now >= pos.hard_timeout {
                    true
                } else if let (Some(f), Some(bin), Some(mean), Some(stop)) = (
                    mids_f.get(&pos.base),
                    mids_b.get(&pos.base),
                    pos.pairs_mean_bp,
                    pos.stop_deviation_bp,
                ) {
                    // Use mids for the signal check (same as entry).
                    let d = (f.mid() - bin.mid()) / bin.mid() * 10_000.0;
                    let dev_now = d - mean;
                    // Exit-band selection: slow-avg mode stores an absolute bp
                    // threshold in pairs_exit_bp; EWMA mode derives from std.
                    let exit_band = if let Some(exit_bp) = pos.pairs_exit_bp {
                        exit_bp
                    } else if let Some(std_bp) = pos.pairs_std_bp {
                        params.pairs_exit_sigma * std_bp
                    } else {
                        0.0
                    };
                    // Asymmetric exit (#6): on a "converged" signal, only
                    // close if the projected net pnl (using current mids as
                    // the exit prices) is actually positive. Otherwise keep
                    // holding until hard_timeout — better to wait for a real
                    // profitable exit than lock in the fee+spread cost now.
                    let converged = dev_now.abs() < exit_band;
                    let stopped = (stop - mean) * dev_now > 0.0
                        && dev_now.abs() >= (stop - mean).abs();
                    if stopped {
                        true
                    } else if converged {
                        if params.pairs_asymmetric_exit {
                            let f_exit_peek = if pos.flipster_side > 0 { f.sell_at() } else { f.buy_at() };
                            let b_exit_peek = if pos.flipster_side > 0 { bin.buy_at() } else { bin.sell_at() };
                            net_pnl_bp(&pos, f_exit_peek, b_exit_peek, params.fee_bp) > 0.0
                        } else {
                            true
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
        };
        if should_exit {
            // Paper fill latency applies to the pairs strategy only. For
            // latency/funding strategies we close immediately (their own
            // hold windows already dominate).
            if matches!(pos.strategy, Strategy::Pairs) && latency_ms > 0 {
                pos.exit_fill_at = Some(now + Duration::milliseconds(latency_ms));
                keep.push(pos);
            } else {
                to_close.push(pos);
            }
        } else {
            keep.push(pos);
        }
    }
    b.positions = keep;
    drop(b);

    for pos in to_close {
        let (Some(f), Some(bin)) = (mids_f.get(&pos.base), mids_b.get(&pos.base)) else {
            warn!(base = %pos.base, "no mids at exit time — skip close");
            continue;
        };
        if (now - f.ts).num_milliseconds() > params.max_mid_age_ms
            || (now - bin.ts).num_milliseconds() > params.max_mid_age_ms
        {
            warn!(
                base = %pos.base,
                strategy = pos.strategy.label(),
                "stale mid at exit — dropping position without writing"
            );
            continue;
        }
        // Close the delta-neutral pair: if long flipster → sell flipster at
        // its bid and buy back binance at its ask (and vice-versa). Cross
        // the spread on both legs symmetrically to the entry.
        let f_exit = if pos.flipster_side > 0 { f.sell_at() } else { f.buy_at() };
        let b_exit = if pos.flipster_side > 0 { bin.buy_at() } else { bin.sell_at() };
        let mut pnl = net_pnl_bp(&pos, f_exit, b_exit, params.fee_bp);
        // For funding strategy, fold in the captured funding rate differential.
        if let (Strategy::Funding, Some(rf), Some(rb)) = (
            pos.strategy,
            pos.flipster_rate_entry,
            pos.binance_rate_entry,
        ) {
            // dir=+1 (long flipster, short binance): collect -rf (if rf<0) + +rb (if rb>0)
            // dir=-1 (short flipster, long binance): collect +rf + -rb
            let funding_leg_bp = ((-(pos.flipster_side as f64) * rf)
                + ((pos.flipster_side as f64) * rb))
                * 10_000.0;
            pnl += funding_leg_bp;
        }
        // Distinguish the three exit paths for diagnostics.
        let reason = if now >= pos.hard_timeout {
            "timeout"
        } else if let (Some(mean), Some(stop)) = (pos.pairs_mean_bp, pos.stop_deviation_bp) {
            let d = (f.mid() - bin.mid()) / bin.mid() * 10_000.0;
            let dev_now = d - mean;
            if (stop - mean) * dev_now > 0.0 && dev_now.abs() >= (stop - mean).abs() {
                "stoploss"
            } else {
                "converged"
            }
        } else {
            "converged"
        };
        let side_s = if pos.flipster_side > 0 { "long" } else { "short" };
        // Update bankroll & per-symbol stats (Kelly feedback).
        let pnl_usd = pnl * pos.size_usd / 10_000.0;
        {
            let mut bk = book.lock().await;
            bk.bankroll_usd += pnl_usd;
            let (n, avg_bp) = {
                let entry = bk
                    .symbol_stats
                    .entry(pos.base.clone())
                    .or_insert((0, 0.0, 0.0));
                entry.0 += 1;
                entry.1 += pnl;
                entry.2 += pnl * pnl;
                (entry.0, entry.1 / entry.0 as f64)
            };
            // Auto-blacklist (#5): once a symbol accumulates enough closed
            // trades and its average bp is worse than the threshold, silence
            // it for `duration_ms`. Clearing its stats gives the symbol a
            // clean slate when the cooldown expires.
            if params.pairs_auto_blacklist_min_trades > 0
                && n >= params.pairs_auto_blacklist_min_trades
                && avg_bp < params.pairs_auto_blacklist_max_avg_bp
            {
                bk.auto_blacklist_until.insert(
                    pos.base.clone(),
                    now + Duration::milliseconds(params.pairs_auto_blacklist_duration_ms),
                );
                bk.symbol_stats.insert(pos.base.clone(), (0, 0.0, 0.0));
                warn!(
                    variant = %params.account_id,
                    base = %pos.base,
                    n,
                    avg_bp = format!("{:+.2}", avg_bp),
                    duration_ms = params.pairs_auto_blacklist_duration_ms,
                    "symbol auto-blacklisted"
                );
            }
        }
        if let Err(e) = writer
            .write_position(
                &params.account_id,
                &pos.base,
                side_s,
                pos.flipster_entry,
                f_exit,
                pos.size_usd,
                pnl,
                pos.entry_time,
                now,
                pos.strategy.label(),
                reason,
                &params.mode_tag,
            )
            .await
        {
            warn!(error = %e, "position write failed");
        }
        info!(
            variant = %params.account_id,
            strategy = pos.strategy.label(),
            base = %pos.base,
            reason,
            net_bp = format!("{:+.3}", pnl),
            size_usd = format!("{:.0}", pos.size_usd),
            pnl_usd = format!("{:+.3}", pnl_usd),
            "paper close"
        );
        // Emit exit signal for live executor sidecar.
        if matches!(pos.strategy, Strategy::Pairs) {
            let exit_side = if pos.flipster_side > 0 { "long" } else { "short" };
            // ZMQ PUB first. Exits never go through coordinator filtering.
            // BBO is left empty: the executor closes via reduce-only and
            // doesn't need a pre-pricing reference for exits.
            crate::coordinator::route_signal(
                &params.account_id, &pos.base, "exit", exit_side,
                pos.size_usd, f_exit, b_exit, pos.id, now,
                crate::signal_publisher::SignalQuotes::default(),
            );
            if let Err(e) = writer
                .write_trade_signal(
                    &params.account_id,
                    &pos.base,
                    "exit",
                    exit_side,
                    pos.size_usd,
                    f_exit,
                    b_exit,
                    pos.id,
                    now,
                )
                .await
            {
                warn!(error = %e, "trade_signal exit write failed");
            }
        }
    }
}

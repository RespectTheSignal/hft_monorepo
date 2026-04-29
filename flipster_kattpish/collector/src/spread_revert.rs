//! Strategy: Binance↔Flipster mid-gap mean reversion (Jay's spec).
//!
//! Hypothesis: the cross-venue basis (Flipster mid − Binance mid) oscillates
//! around a slow-moving local mean. When it deviates beyond
//! (cost + threshold) we open a position on Flipster expecting reversion.
//!
//! Entry test:
//!     edge_short = current_gap_bp - bf_avg_gap_bp
//!                  - flipster_avg_spread_bp
//!                  - binance_avg_spread_bp / 2.0
//!                  - flipster_fee_bp
//!                  - entry_threshold_bp
//!     If edge_short > 0  → SHORT Flipster (gap is too HIGH, expect to drop)
//!     edge_long = (bf_avg_gap_bp - current_gap_bp)
//!                  - (same costs)
//!     If edge_long > 0   → LONG Flipster (gap is too LOW, expect to rise)
//!
//! Windows (per Jay's spec):
//!   - bf_avg_gap_bp: 30 min window, sampled every 5 sec.
//!   - flipster_avg_spread_bp / binance_avg_spread_bp: 10 min window,
//!     sampled every 5 sec.
//! Both samples land at the same 5-sec cadence, so a single 30-min
//! ring-buffer is enough — short-window means use the most recent
//! `spread_window_s / 5` entries.
//!
//! Exit (per Jay's spec):
//!   (a) stop-loss: gap moved `stop_bp` adversely from entry
//!       (default 20 bp).
//!   (b) timeout: position has been open longer than `max_hold_s`
//!       (default 24 h). Logged as "timeout", separate from "stop".
//! Take-profit happens implicitly: as the gap converges toward
//! `bf_avg_gap`, the entry condition no longer fires — and an opposite-
//! side entry signal closes the position before opening the new one.
//!
//! Sizing: fixed `entry_size_usd` per trade, capped at `max_open_positions`
//! globally (independent slots, one per base — no doubling on same symbol).
//! Per-base re-entry is gated by `entry_cooldown_ms` (default 500 ms) so a
//! flapping signal can't fire two consecutive entries.
//!
//! QuestDB separation: writes to `position_log` with
//!   strategy = "spread_revert"
//!   account_id = "SPREAD_REVERT_v1" (or SR_ACCOUNT_ID env)
//! so SELECT WHERE strategy='spread_revert' isolates from gate_lead / pairs.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration as StdDuration;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use tokio::sync::{broadcast, Mutex};
use tracing::{info, warn};

use crate::ilp::IlpWriter;
use crate::model::{BookTick, ExchangeName};
use pairs_core::{base_of as pairs_core_base_of, single_leg_pnl_bp};

#[derive(Clone, Debug)]
pub struct SpreadRevertParams {
    pub account_id: String,
    /// USD notional per entry order (fixed-size).
    pub entry_size_usd: f64,
    /// Maximum cumulative concurrent positions (across all bases).
    /// Default 50 — per-base cap (`max_positions_per_base`) is the
    /// tighter constraint in practice; this is a safety ceiling so a
    /// runaway loop can't open hundreds of positions.
    pub max_open_positions: usize,
    /// Maximum simultaneous positions on the same base. Default 5 —
    /// allows stacking signals on volatile symbols while bounding the
    /// per-symbol exposure. Different bases stack independently.
    pub max_positions_per_base: usize,
    /// Long-window length for `bf_avg_gap_bp` (seconds). Default 30 min.
    pub gap_window_s: f64,
    /// Short-window length for the per-venue avg spread (seconds).
    /// Default 10 min. Must be ≤ gap_window_s — uses the most recent slice
    /// of the same ring buffer.
    pub spread_window_s: f64,
    /// Sample interval (ms) for the rolling buffer. We append a sample at
    /// most once per interval. Default 5000 (= 5 s, per spec).
    pub sample_interval_ms: i64,
    /// Minimum number of stored samples before entries are allowed.
    pub min_window_samples: usize,
    /// Extra bp required on top of measured costs to trigger entry.
    pub entry_threshold_bp: f64,
    /// Stop-loss: close when the gap moves `stop_bp` adversely from entry.
    /// Default 20 bp.
    pub stop_bp: f64,
    /// Hard max hold (seconds). Default 24 h. Past this the position is
    /// closed with reason="timeout", separate from price-stop.
    pub max_hold_s: f64,
    /// Per-base cooldown after entry (ms). Blocks a same-symbol entry
    /// from firing again within this window — implements the "500 ms
    /// dedup per symbol" requirement.
    pub entry_cooldown_ms: i64,
    /// Per-side Flipster taker fee (bp) used in the entry edge calc.
    pub flipster_fee_bp: f64,
    /// Backtest mode flag. When true, on_tick skips the wall-clock stale
    /// check so historical replay ticks aren't all dropped.
    pub backtest_mode: bool,
    /// Whitelist of bases. Empty = allow all (filtered by blacklist).
    pub whitelist: Vec<String>,
    pub blacklist: Vec<String>,
}

impl Default for SpreadRevertParams {
    fn default() -> Self {
        Self {
            account_id: "SPREAD_REVERT_v1".to_string(),
            entry_size_usd: 10.0,
            max_open_positions: 50,
            max_positions_per_base: 5,
            gap_window_s: 30.0 * 60.0,
            spread_window_s: 10.0 * 60.0,
            sample_interval_ms: 5_000,
            // 30 min / 5 sec = 360 samples; require at least 60
            // (= 5 minutes of history) before trading.
            min_window_samples: 60,
            entry_threshold_bp: 2.0,
            stop_bp: 20.0,
            max_hold_s: 86_400.0, // 24 h

            entry_cooldown_ms: 500,
            flipster_fee_bp: 0.425,
            backtest_mode: false,
            whitelist: Vec::new(),
            blacklist: vec![
                "M".into(),
                "BSB".into(),
                "SWARMS".into(),
                "ORCA".into(),
            ],
        }
    }
}

impl SpreadRevertParams {
    pub fn from_env() -> Self {
        let mut p = Self::default();
        if let Ok(v) = std::env::var("SR_ACCOUNT_ID") { p.account_id = v; }
        if let Some(v) = std::env::var("SR_SIZE_USD").ok().and_then(|s| s.parse().ok()) {
            p.entry_size_usd = v;
        }
        if let Some(v) = std::env::var("SR_MAX_POSITIONS").ok().and_then(|s| s.parse().ok()) {
            p.max_open_positions = v;
        }
        if let Some(v) = std::env::var("SR_MAX_POSITIONS_PER_BASE").ok().and_then(|s| s.parse().ok()) {
            p.max_positions_per_base = v;
        }
        if let Some(v) = std::env::var("SR_GAP_WINDOW_S").ok().and_then(|s| s.parse().ok()) {
            p.gap_window_s = v;
        }
        if let Some(v) = std::env::var("SR_SPREAD_WINDOW_S").ok().and_then(|s| s.parse().ok()) {
            p.spread_window_s = v;
        }
        if let Some(v) = std::env::var("SR_SAMPLE_INTERVAL_MS").ok().and_then(|s| s.parse().ok()) {
            p.sample_interval_ms = v;
        }
        if let Some(v) = std::env::var("SR_MIN_SAMPLES").ok().and_then(|s| s.parse().ok()) {
            p.min_window_samples = v;
        }
        if let Some(v) = std::env::var("SR_ENTRY_BP").ok().and_then(|s| s.parse().ok()) {
            p.entry_threshold_bp = v;
        }
        if let Some(v) = std::env::var("SR_STOP_BP").ok().and_then(|s| s.parse().ok()) {
            p.stop_bp = v;
        }
        if let Some(v) = std::env::var("SR_MAX_HOLD_S").ok().and_then(|s| s.parse().ok()) {
            p.max_hold_s = v;
        }
        if let Some(v) = std::env::var("SR_ENTRY_COOLDOWN_MS").ok().and_then(|s| s.parse().ok()) {
            p.entry_cooldown_ms = v;
        }
        if let Some(v) = std::env::var("SR_FEE_BP").ok().and_then(|s| s.parse().ok()) {
            p.flipster_fee_bp = v;
        }
        if let Ok(v) = std::env::var("SR_WHITELIST") {
            p.whitelist = v
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.trim().to_uppercase())
                .collect();
        }
        if let Ok(v) = std::env::var("SR_BLACKLIST") {
            p.blacklist = v
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.trim().to_uppercase())
                .collect();
        }
        p
    }
}

/// Rolling window sample.
#[derive(Clone, Copy, Debug)]
struct Sample {
    ts: DateTime<Utc>,
    gap_bp: f64,
    flipster_spread_bp: f64,
    binance_spread_bp: f64,
}

#[derive(Default)]
struct BaseState {
    binance_bid: f64,
    binance_ask: f64,
    binance_ts: Option<DateTime<Utc>>,
    flipster_bid: f64,
    flipster_ask: f64,
    flipster_ts: Option<DateTime<Utc>>,
    /// Rolling samples of (gap, flipster_spread_bp, binance_spread_bp),
    /// pushed at most once per `sample_interval_ms`. Length is bounded
    /// by `gap_window_s / sample_interval` (e.g. 360 entries at the
    /// 30 min × 5 sec defaults).
    history: VecDeque<Sample>,
    /// Most recent sample timestamp; gates the "≤ 1 sample / interval"
    /// throttling so a 1 ms tick burst doesn't fill the buffer.
    last_sample_at: Option<DateTime<Utc>>,
    /// Last successful entry timestamp on this base. Blocks a new entry
    /// for `entry_cooldown_ms` after firing, even after the position
    /// closed (handles the "500 ms per-symbol dedup" requirement).
    last_entry_at: Option<DateTime<Utc>>,
    /// Open positions on this base. Bounded by
    /// `params.max_positions_per_base`. Stacked entries are independent
    /// — each has its own entry_gap, deadline, and exit decision.
    open: Vec<OpenPos>,
}

#[derive(Debug, Clone)]
struct OpenPos {
    id: u64,
    side: i8, // +1 long flipster, -1 short flipster
    entry_ts: DateTime<Utc>,
    entry_price: f64,        // flipster fill price
    entry_gap_bp: f64,       // gap at entry (used for stop-loss)
    deadline: DateTime<Utc>,
}

struct CloseAction {
    base: String,
    pos: OpenPos,
    exit_price: f64,
    exit_gap_bp: f64,
    avg_gap_bp: f64,
    reason: &'static str,
    exit_ts: DateTime<Utc>,
}

struct OpenAction {
    base: String,
    pos_id: u64,
    side_s: &'static str,
    size_usd: f64,
    entry_price: f64,
    binance_mid: f64,
    flipster_bid: f64,
    flipster_ask: f64,
    binance_bid: f64,
    binance_ask: f64,
    ts: DateTime<Utc>,
}

pub async fn run(
    writer: IlpWriter,
    mut tick_rx: broadcast::Receiver<BookTick>,
    params: SpreadRevertParams,
) {
    info!(
        account_id = %params.account_id,
        size_usd = params.entry_size_usd,
        max_pos = params.max_open_positions,
        max_per_base = params.max_positions_per_base,
        gap_window_s = params.gap_window_s,
        spread_window_s = params.spread_window_s,
        sample_interval_ms = params.sample_interval_ms,
        entry_bp = params.entry_threshold_bp,
        stop_bp = params.stop_bp,
        max_hold_s = params.max_hold_s,
        cooldown_ms = params.entry_cooldown_ms,
        whitelist = params.whitelist.join(","),
        "[spread_revert] starting"
    );

    let state: Arc<Mutex<HashMap<String, BaseState>>> = Arc::new(Mutex::new(HashMap::new()));
    let open_count = Arc::new(AtomicUsize::new(0));

    // Background sweeper for time-based exits — fires every 200 ms.
    {
        let state = state.clone();
        let writer = writer.clone();
        let params = params.clone();
        let open_count = open_count.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(StdDuration::from_millis(200));
            loop {
                iv.tick().await;
                sweep_exits(&state, &writer, &params, &open_count).await;
            }
        });
    }

    loop {
        match tick_rx.recv().await {
            Ok(t) => {
                if let Err(e) = on_tick(&state, &writer, &t, &params, &open_count).await {
                    warn!(error = %e, "[spread_revert] on_tick err");
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(missed = n, "[spread_revert] tick lagged");
            }
            Err(_) => break,
        }
    }
}

async fn on_tick(
    state: &Arc<Mutex<HashMap<String, BaseState>>>,
    writer: &IlpWriter,
    tick: &BookTick,
    params: &SpreadRevertParams,
    open_count: &Arc<AtomicUsize>,
) -> Result<()> {
    let bid = tick.bid_price;
    let ask = tick.ask_price;
    if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask <= 0.0 {
        return Ok(());
    }
    let Some(base) = base_of(tick.exchange, &tick.symbol) else {
        return Ok(());
    };
    if !params.whitelist.is_empty() && !params.whitelist.contains(&base) {
        return Ok(());
    }
    if params.blacklist.contains(&base) {
        return Ok(());
    }
    if !params.backtest_mode {
        let wall_now = Utc::now();
        if (wall_now - tick.timestamp).num_milliseconds() > 1000 {
            return Ok(());
        }
    }
    let now = tick.timestamp;

    let mut open_acts: Vec<OpenAction> = Vec::new();
    let mut close_acts: Vec<CloseAction> = Vec::new();
    {
        let mut s = state.lock().await;
        let entry = s.entry(base.clone()).or_default();

        match tick.exchange {
            ExchangeName::Binance => {
                entry.binance_bid = bid;
                entry.binance_ask = ask;
                entry.binance_ts = Some(now);
            }
            ExchangeName::Flipster => {
                entry.flipster_bid = bid;
                entry.flipster_ask = ask;
                entry.flipster_ts = Some(now);
            }
            _ => return Ok(()),
        }

        // Both sides must be fresh (≤ 2s) to compute a meaningful gap.
        let b_ts = entry.binance_ts;
        let f_ts = entry.flipster_ts;
        let stale_b = b_ts.map(|t| (now - t).num_milliseconds() > 2000).unwrap_or(true);
        let stale_f = f_ts.map(|t| (now - t).num_milliseconds() > 2000).unwrap_or(true);
        if stale_b || stale_f {
            return Ok(());
        }
        if entry.binance_bid <= 0.0 || entry.binance_ask <= 0.0
            || entry.flipster_bid <= 0.0 || entry.flipster_ask <= 0.0
        {
            return Ok(());
        }

        let bin_mid = (entry.binance_bid + entry.binance_ask) * 0.5;
        let fli_mid = (entry.flipster_bid + entry.flipster_ask) * 0.5;
        if bin_mid <= 0.0 || fli_mid <= 0.0 {
            return Ok(());
        }
        let gap_bp = (fli_mid - bin_mid) / bin_mid * 1e4;
        let bin_spread_bp = (entry.binance_ask - entry.binance_bid) / bin_mid * 1e4;
        let fli_spread_bp = (entry.flipster_ask - entry.flipster_bid) / fli_mid * 1e4;

        // Throttled append: at most one sample per `sample_interval_ms`.
        // Without throttling the 30-min window would balloon to millions
        // of entries on busy symbols and the means would overweight the
        // recent burst.
        let should_sample = entry
            .last_sample_at
            .map(|t| (now - t).num_milliseconds() >= params.sample_interval_ms)
            .unwrap_or(true);
        if should_sample {
            entry.history.push_back(Sample {
                ts: now,
                gap_bp,
                flipster_spread_bp: fli_spread_bp,
                binance_spread_bp: bin_spread_bp,
            });
            entry.last_sample_at = Some(now);
            // Trim to the long window (gap_window_s); the spread mean
            // pulls from the recent slice of the same buffer.
            let cutoff = now - Duration::milliseconds((params.gap_window_s * 1000.0) as i64);
            while let Some(front) = entry.history.front() {
                if front.ts < cutoff {
                    entry.history.pop_front();
                } else {
                    break;
                }
            }
        }
        if entry.history.len() < params.min_window_samples {
            return Ok(());
        }

        // bf_avg_gap_bp: mean over the full (long) window.
        let mut sum_gap = 0.0;
        for h in entry.history.iter() {
            sum_gap += h.gap_bp;
        }
        let avg_gap = sum_gap / entry.history.len() as f64;

        // Per-venue avg_spread: mean over the recent `spread_window_s`
        // slice. We walk back from the tail until we hit the cutoff.
        let spread_cutoff =
            now - Duration::milliseconds((params.spread_window_s * 1000.0) as i64);
        let mut sum_fs = 0.0;
        let mut sum_bs = 0.0;
        let mut n_short: usize = 0;
        for h in entry.history.iter().rev() {
            if h.ts < spread_cutoff {
                break;
            }
            sum_fs += h.flipster_spread_bp;
            sum_bs += h.binance_spread_bp;
            n_short += 1;
        }
        if n_short == 0 {
            return Ok(());
        }
        let avg_fs = sum_fs / n_short as f64;
        let avg_bs = sum_bs / n_short as f64;

        // Exit check — every open position on this base evaluated
        // independently. stop-loss (price displacement) + timeout
        // (deadline). Reverse close handled in the entry branch below.
        let mut keep: Vec<OpenPos> = Vec::with_capacity(entry.open.len());
        for pos in entry.open.drain(..) {
            let mut exit_pick: Option<(&'static str, f64)> = None;
            let displacement = (gap_bp - pos.entry_gap_bp) * (pos.side as f64);
            if displacement <= -params.stop_bp {
                let exit_px = if pos.side == 1 { entry.flipster_bid } else { entry.flipster_ask };
                exit_pick = Some(("stop", exit_px));
            }
            if exit_pick.is_none() && now >= pos.deadline {
                let exit_px = if pos.side == 1 { entry.flipster_bid } else { entry.flipster_ask };
                exit_pick = Some(("timeout", exit_px));
            }
            match exit_pick {
                Some((reason, exit_price)) => {
                    close_acts.push(CloseAction {
                        base: base.clone(),
                        pos,
                        exit_price,
                        exit_gap_bp: gap_bp,
                        avg_gap_bp: avg_gap,
                        reason,
                        exit_ts: now,
                    });
                }
                None => keep.push(pos),
            }
        }
        entry.open = keep;

        // Entry / reverse-close decision.
        let cur_open_global = open_count.load(Ordering::Relaxed);
        let in_cooldown = entry
            .last_entry_at
            .map(|t| (now - t).num_milliseconds() < params.entry_cooldown_ms)
            .unwrap_or(false);
        let costs = avg_fs + avg_bs / 2.0 + params.flipster_fee_bp;
        let edge_short = (gap_bp - avg_gap) - costs - params.entry_threshold_bp;
        let edge_long = (avg_gap - gap_bp) - costs - params.entry_threshold_bp;
        let want_side: Option<i8> = if edge_short > 0.0 {
            Some(-1)
        } else if edge_long > 0.0 {
            Some(1)
        } else {
            None
        };

        // Reverse-side close: opposite-side entry signal closes every
        // existing position on this base (implicit take-profit per Jay's
        // spec). Equivalent-side signals stack instead of closing.
        if let Some(want) = want_side {
            let mut keep: Vec<OpenPos> = Vec::with_capacity(entry.open.len());
            for pos in entry.open.drain(..) {
                if pos.side != want {
                    let exit_px = if pos.side == 1 { entry.flipster_bid } else { entry.flipster_ask };
                    close_acts.push(CloseAction {
                        base: base.clone(),
                        pos,
                        exit_price: exit_px,
                        exit_gap_bp: gap_bp,
                        avg_gap_bp: avg_gap,
                        reason: "reverse",
                        exit_ts: now,
                    });
                } else {
                    keep.push(pos);
                }
            }
            entry.open = keep;
        }

        let per_base_room = entry.open.len() < params.max_positions_per_base;
        let global_room = cur_open_global < params.max_open_positions;
        if per_base_room && global_room && !in_cooldown {
            if let Some(side) = want_side {
                let prev = open_count.fetch_add(1, Ordering::Relaxed);
                if prev >= params.max_open_positions {
                    open_count.fetch_sub(1, Ordering::Relaxed);
                } else {
                    let entry_price = if side == 1 {
                        entry.flipster_ask
                    } else {
                        entry.flipster_bid
                    };
                    let pos_id = pairs_core::pos_id::global().next();
                    let pos = OpenPos {
                        id: pos_id,
                        side,
                        entry_ts: now,
                        entry_price,
                        entry_gap_bp: gap_bp,
                        deadline: now + Duration::milliseconds((params.max_hold_s * 1000.0) as i64),
                    };
                    entry.open.push(pos);
                    entry.last_entry_at = Some(now);
                    info!(
                        base = %base,
                        side = if side == 1 { "long" } else { "short" },
                        gap_bp = format!("{:+.2}", gap_bp),
                        avg_gap_bp = format!("{:+.2}", avg_gap),
                        edge_bp = format!("{:+.2}", if side == 1 { edge_long } else { edge_short }),
                        costs_bp = format!("{:.2}", costs),
                        n_long = entry.history.len(),
                        n_short = n_short,
                        per_base_n = entry.open.len(),
                        f_entry = entry_price,
                        pos_id,
                        "[spread_revert] OPEN"
                    );
                    open_acts.push(OpenAction {
                        base: base.clone(),
                        pos_id,
                        side_s: if side == 1 { "long" } else { "short" },
                        size_usd: params.entry_size_usd,
                        entry_price,
                        binance_mid: bin_mid,
                        flipster_bid: entry.flipster_bid,
                        flipster_ask: entry.flipster_ask,
                        binance_bid: entry.binance_bid,
                        binance_ask: entry.binance_ask,
                        ts: now,
                    });
                }
            }
        }
    }

    for o in open_acts {
        let mut quotes = crate::signal_publisher::SignalQuotes::default();
        quotes.flipster_bid = Some(o.flipster_bid);
        quotes.flipster_ask = Some(o.flipster_ask);
        quotes.binance_bid = Some(o.binance_bid);
        quotes.binance_ask = Some(o.binance_ask);
        crate::coordinator::route_signal(
            &params.account_id,
            &o.base,
            "entry",
            o.side_s,
            o.size_usd,
            o.entry_price,
            o.binance_mid,
            o.pos_id,
            o.ts,
            quotes,
        );
        if let Err(e) = writer
            .write_trade_signal(
                &params.account_id,
                &o.base,
                "entry",
                o.side_s,
                o.size_usd,
                o.entry_price,
                o.binance_mid,
                o.pos_id,
                o.ts,
            )
            .await
        {
            warn!(error = %e, "[spread_revert] entry signal write failed");
        }
    }
    for c in close_acts {
        log_close(c, writer, params, open_count).await;
    }
    Ok(())
}

async fn sweep_exits(
    state: &Arc<Mutex<HashMap<String, BaseState>>>,
    writer: &IlpWriter,
    params: &SpreadRevertParams,
    open_count: &Arc<AtomicUsize>,
) {
    let now = Utc::now();
    let mut closes: Vec<CloseAction> = Vec::new();
    {
        let mut s = state.lock().await;
        // Find bases that have at least one position past its deadline.
        let bases: Vec<String> = s
            .iter()
            .filter_map(|(k, v)| {
                if v.open.iter().any(|p| p.deadline <= now) {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        for base in bases {
            let Some(entry) = s.get_mut(&base) else { continue };
            let bin_mid = if entry.binance_bid > 0.0 && entry.binance_ask > 0.0 {
                (entry.binance_bid + entry.binance_ask) * 0.5
            } else {
                0.0
            };
            let fli_mid = if entry.flipster_bid > 0.0 && entry.flipster_ask > 0.0 {
                (entry.flipster_bid + entry.flipster_ask) * 0.5
            } else {
                0.0
            };
            let gap_bp = if bin_mid > 0.0 && fli_mid > 0.0 {
                Some((fli_mid - bin_mid) / bin_mid * 1e4)
            } else {
                None
            };
            // Drop expired positions only; keep the rest.
            let mut keep: Vec<OpenPos> = Vec::with_capacity(entry.open.len());
            for pos in entry.open.drain(..) {
                if pos.deadline > now {
                    keep.push(pos);
                    continue;
                }
                let exit_price = if entry.flipster_bid > 0.0 && entry.flipster_ask > 0.0 {
                    if pos.side == 1 { entry.flipster_bid } else { entry.flipster_ask }
                } else {
                    pos.entry_price
                };
                let exit_gap_bp = gap_bp.unwrap_or(pos.entry_gap_bp);
                closes.push(CloseAction {
                    base: base.clone(),
                    pos,
                    exit_price,
                    exit_gap_bp,
                    avg_gap_bp: pos_avg_or(&entry.history),
                    reason: "timeout",
                    exit_ts: now,
                });
            }
            entry.open = keep;
        }
    }
    for c in closes {
        log_close(c, writer, params, open_count).await;
    }
}

fn pos_avg_or(history: &VecDeque<Sample>) -> f64 {
    if history.is_empty() {
        return 0.0;
    }
    let n = history.len() as f64;
    history.iter().map(|h| h.gap_bp).sum::<f64>() / n
}

async fn log_close(
    c: CloseAction,
    writer: &IlpWriter,
    params: &SpreadRevertParams,
    open_count: &Arc<AtomicUsize>,
) {
    open_count.fetch_sub(1, Ordering::Relaxed);

    let pnl_bp_gross = single_leg_pnl_bp(c.pos.entry_price, c.exit_price, c.pos.side as i32);
    let net_bp = pnl_bp_gross - 2.0 * params.flipster_fee_bp;

    info!(
        base = %c.base,
        side = if c.pos.side == 1 { "long" } else { "short" },
        reason = c.reason,
        entry_gap_bp = format!("{:+.2}", c.pos.entry_gap_bp),
        exit_gap_bp = format!("{:+.2}", c.exit_gap_bp),
        avg_gap_bp = format!("{:+.2}", c.avg_gap_bp),
        f_entry = c.pos.entry_price,
        f_exit = c.exit_price,
        pnl_bp_gross = format!("{:+.2}", pnl_bp_gross),
        net_bp = format!("{:+.2}", net_bp),
        hold_ms = (c.exit_ts - c.pos.entry_ts).num_milliseconds(),
        "[spread_revert] CLOSE"
    );

    let side_s = if c.pos.side == 1 { "long" } else { "short" };
    if let Err(e) = writer
        .write_position(
            &params.account_id,
            &c.base,
            side_s,
            c.pos.entry_price,
            c.exit_price,
            params.entry_size_usd,
            net_bp,
            c.pos.entry_ts,
            c.exit_ts,
            "spread_revert",
            c.reason,
            "paper",
        )
        .await
    {
        warn!(error = %e, "[spread_revert] position_log write failed");
    }

    crate::coordinator::route_signal(
        &params.account_id,
        &c.base,
        "exit",
        side_s,
        params.entry_size_usd,
        c.exit_price,
        c.exit_price,
        c.pos.id,
        c.exit_ts,
        crate::signal_publisher::SignalQuotes::default(),
    );
    if let Err(e) = writer
        .write_trade_signal(
            &params.account_id,
            &c.base,
            "exit",
            side_s,
            params.entry_size_usd,
            c.exit_price,
            c.exit_price,
            c.pos.id,
            c.exit_ts,
        )
        .await
    {
        warn!(error = %e, "[spread_revert] exit signal write failed");
    }
}

fn base_of(exchange: ExchangeName, symbol: &str) -> Option<String> {
    pairs_core_base_of(exchange, symbol)
}

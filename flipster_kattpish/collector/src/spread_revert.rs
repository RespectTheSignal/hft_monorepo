//! Strategy: Binance↔Flipster mid-gap mean reversion.
//!
//! Hypothesis: the cross-venue basis (Flipster mid − Binance mid) oscillates
//! around a slow-moving local mean. When it deviates beyond
//! (cost + threshold) we open a position on Flipster expecting reversion,
//! and close when the gap returns near the mean OR an adverse stop hits.
//!
//! Entry test (per Jay's spec):
//!     edge_short = current_gap_bp - avg_gap_bp
//!                  - flipster_avg_spread_bp
//!                  - binance_avg_spread_bp / 2.0
//!                  - flipster_fee_bp
//!                  - entry_threshold_bp
//!     If edge_short > 0  → SHORT Flipster (gap is too HIGH, expect to drop)
//!     edge_long = (-current_gap_bp + avg_gap_bp)
//!                  - flipster_avg_spread_bp
//!                  - binance_avg_spread_bp / 2.0
//!                  - flipster_fee_bp
//!                  - entry_threshold_bp
//!     If edge_long > 0   → LONG Flipster (gap is too LOW, expect to rise)
//!
//! Exit:
//!   (a) stop-loss: gap moves further from mean by `stop_bp` (adverse)
//!   (b) mean revert: gap returns within `revert_band_bp` of avg
//!   (c) hard timeout `max_hold_s` (safety net)
//!
//! Sizing: fixed `entry_size_usd` per trade, capped at `max_open_positions`
//! globally (independent slots, one per base — no doubling on same symbol).
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
    pub max_open_positions: usize,
    /// Rolling window length for avg_gap / avg_spread (seconds).
    pub avg_window_s: f64,
    /// Minimum samples in the rolling window before entries are allowed.
    pub min_window_samples: usize,
    /// Extra bp required on top of measured costs to trigger entry.
    pub entry_threshold_bp: f64,
    /// Stop-loss: close when gap moves `stop_bp` further from avg vs entry.
    pub stop_bp: f64,
    /// Take-profit: close when |gap − avg| ≤ revert_band_bp.
    pub revert_band_bp: f64,
    /// Hard max hold (seconds) — safety net on top of stop / revert.
    pub max_hold_s: f64,
    /// Per-side Flipster taker fee (bp) used in the entry edge calc.
    pub flipster_fee_bp: f64,
    /// Whitelist of bases. Empty = allow all (filtered by blacklist).
    pub whitelist: Vec<String>,
    pub blacklist: Vec<String>,
}

impl Default for SpreadRevertParams {
    fn default() -> Self {
        Self {
            account_id: "SPREAD_REVERT_v1".to_string(),
            entry_size_usd: 10.0,
            max_open_positions: 5,
            avg_window_s: 60.0,
            min_window_samples: 30,
            entry_threshold_bp: 2.0,
            stop_bp: 10.0,
            revert_band_bp: 0.5,
            max_hold_s: 300.0,
            flipster_fee_bp: 0.425,
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
        if let Some(v) = std::env::var("SR_WINDOW_S").ok().and_then(|s| s.parse().ok()) {
            p.avg_window_s = v;
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
        if let Some(v) = std::env::var("SR_REVERT_BP").ok().and_then(|s| s.parse().ok()) {
            p.revert_band_bp = v;
        }
        if let Some(v) = std::env::var("SR_MAX_HOLD_S").ok().and_then(|s| s.parse().ok()) {
            p.max_hold_s = v;
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
    /// Rolling samples of (gap, flipster_spread_bp, binance_spread_bp).
    history: VecDeque<Sample>,
    open: Option<OpenPos>,
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
    flipster_spread_bp: f64,
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
        window_s = params.avg_window_s,
        entry_bp = params.entry_threshold_bp,
        stop_bp = params.stop_bp,
        revert_bp = params.revert_band_bp,
        max_hold_s = params.max_hold_s,
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
    let wall_now = Utc::now();
    if (wall_now - tick.timestamp).num_milliseconds() > 1000 {
        return Ok(());
    }
    let now = tick.timestamp;

    let mut open_act: Option<OpenAction> = None;
    let mut close_act: Option<CloseAction> = None;
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

        // Append + trim rolling window.
        entry.history.push_back(Sample {
            ts: now,
            gap_bp,
            flipster_spread_bp: fli_spread_bp,
            binance_spread_bp: bin_spread_bp,
        });
        let cutoff = now - Duration::milliseconds((params.avg_window_s * 1000.0) as i64);
        while let Some(front) = entry.history.front() {
            if front.ts < cutoff {
                entry.history.pop_front();
            } else {
                break;
            }
        }
        if entry.history.len() < params.min_window_samples {
            return Ok(());
        }
        let n = entry.history.len() as f64;
        let mut sum_gap = 0.0;
        let mut sum_fs = 0.0;
        let mut sum_bs = 0.0;
        for h in entry.history.iter() {
            sum_gap += h.gap_bp;
            sum_fs += h.flipster_spread_bp;
            sum_bs += h.binance_spread_bp;
        }
        let avg_gap = sum_gap / n;
        let avg_fs = sum_fs / n;
        let avg_bs = sum_bs / n;

        // Exit check first (open position).
        if let Some(pos) = entry.open.clone() {
            let mut exit_pick: Option<(&'static str, f64)> = None;
            // For LONG  (side=+1): favorable = gap rose  → displacement > 0
            // For SHORT (side=-1): favorable = gap fell  → displacement > 0
            // Unified: displacement = (gap - entry_gap) * side.
            // Stop fires when displacement ≤ -stop_bp (gap moved adversely).
            let displacement_from_entry = (gap_bp - pos.entry_gap_bp) * (pos.side as f64);
            if displacement_from_entry <= -params.stop_bp {
                let exit_px = if pos.side == 1 { entry.flipster_bid } else { entry.flipster_ask };
                exit_pick = Some(("stop", exit_px));
            }
            if exit_pick.is_none() && (gap_bp - avg_gap).abs() <= params.revert_band_bp {
                let exit_px = if pos.side == 1 { entry.flipster_bid } else { entry.flipster_ask };
                exit_pick = Some(("revert", exit_px));
            }
            if exit_pick.is_none() && now >= pos.deadline {
                let exit_px = if pos.side == 1 { entry.flipster_bid } else { entry.flipster_ask };
                exit_pick = Some(("timeout", exit_px));
            }
            if let Some((reason, exit_price)) = exit_pick {
                entry.open = None;
                close_act = Some(CloseAction {
                    base: base.clone(),
                    pos,
                    exit_price,
                    exit_gap_bp: gap_bp,
                    avg_gap_bp: avg_gap,
                    reason,
                    exit_ts: now,
                });
            }
        }

        // Entry check (only if no position on this base AND below global cap).
        if entry.open.is_none() && close_act.is_none() {
            let cur_open = open_count.load(Ordering::Relaxed);
            if cur_open < params.max_open_positions {
                let costs = avg_fs + avg_bs / 2.0 + params.flipster_fee_bp;
                let edge_short = (gap_bp - avg_gap) - costs - params.entry_threshold_bp;
                let edge_long = (avg_gap - gap_bp) - costs - params.entry_threshold_bp;
                let side: Option<i8> = if edge_short > 0.0 {
                    Some(-1)
                } else if edge_long > 0.0 {
                    Some(1)
                } else {
                    None
                };
                if let Some(side) = side {
                    // Reserve the slot atomically; if another base raced and
                    // filled the cap between load and CAS, abort.
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
                        entry.open = Some(pos);
                        info!(
                            base = %base,
                            side = if side == 1 { "long" } else { "short" },
                            gap_bp = format!("{:+.2}", gap_bp),
                            avg_gap_bp = format!("{:+.2}", avg_gap),
                            edge_bp = format!("{:+.2}", if side == 1 { edge_long } else { edge_short }),
                            costs_bp = format!("{:.2}", costs),
                            f_entry = entry_price,
                            pos_id,
                            "[spread_revert] OPEN"
                        );
                        open_act = Some(OpenAction {
                            base: base.clone(),
                            pos_id,
                            side_s: if side == 1 { "long" } else { "short" },
                            size_usd: params.entry_size_usd,
                            entry_price,
                            binance_mid: bin_mid,
                            flipster_spread_bp: fli_spread_bp,
                            ts: now,
                        });
                    }
                }
            }
        }
    }

    if let Some(o) = open_act {
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
            Some(o.flipster_spread_bp),
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
    if let Some(c) = close_act {
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
        let bases: Vec<String> = s
            .iter()
            .filter_map(|(k, v)| {
                v.open
                    .as_ref()
                    .and_then(|p| if p.deadline <= now { Some(k.clone()) } else { None })
            })
            .collect();
        for base in bases {
            let Some(entry) = s.get_mut(&base) else { continue };
            let Some(pos) = entry.open.clone() else { continue };
            let exit_price = if entry.flipster_bid > 0.0 && entry.flipster_ask > 0.0 {
                if pos.side == 1 { entry.flipster_bid } else { entry.flipster_ask }
            } else {
                pos.entry_price
            };
            let gap_bp = if entry.binance_bid > 0.0 && entry.flipster_bid > 0.0 {
                let bin_mid = (entry.binance_bid + entry.binance_ask) * 0.5;
                let fli_mid = (entry.flipster_bid + entry.flipster_ask) * 0.5;
                if bin_mid > 0.0 { (fli_mid - bin_mid) / bin_mid * 1e4 } else { pos.entry_gap_bp }
            } else {
                pos.entry_gap_bp
            };
            entry.open = None;
            closes.push(CloseAction {
                base,
                pos,
                exit_price,
                exit_gap_bp: gap_bp,
                avg_gap_bp: pos_avg_or(&entry.history),
                reason: "timeout",
                exit_ts: now,
            });
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
        None,
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

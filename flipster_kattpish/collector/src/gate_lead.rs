//! Strategy: <leader>→Flipster lead-lag (default leader = Binance, was
//! Gate). Binance has higher liquidity and faster price discovery, so the
//! lead-lag relationship is expected to be at least as strong.
//!
//! Hypothesis (verified on 2h backtest, 2026-04-27):
//!
//! When Gate's mid moves ≥10 bp in the trailing 3 s, Flipster follows in the
//! SAME direction within 2 s about 60-90% of the time on alts. The reverse
//! (Flipster leading Gate) does NOT hold — it's a one-way lead.
//!
//! Backtest @ 100 ms execution lag, 6 alts (TURTLE/MERL/AAVE/CFX/PNUT/PENGU):
//! - 1115 trades / 2 h
//! - 63% win rate
//! - +2.15 bp/trade net (after fees)
//! - +$1.20 / 2 h on $5 size
//!
//! This module runs the SAME detection logic and simulates trades as a paper
//! bot, writing to `position_log` with `strategy="gate_lead"`. It also emits
//! a ZMQ trade_signal so the existing live executor can pick it up.
//!
//! Detection: per-base rolling 3 s history of Gate mids. On each Gate tick,
//! compare current mid to the anchor 3 s ago. If |move| ≥ `min_move_bp` and
//! we're not in cooldown for that base, fire an entry.
//!
//! Paper exit: monitor Flipster mid. Take-profit when Flipster mid has moved
//! `exit_bp` in the predicted direction. Stop-loss when it has moved
//! `stop_bp` against. Otherwise time out at `hold_max_s`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use tokio::sync::{broadcast, Mutex};
use tracing::{info, warn};

use crate::ilp::IlpWriter;
use crate::model::{BookTick, ExchangeName};
use pairs_core::{base_of as pairs_core_base_of, single_leg_pnl_bp};

#[derive(Clone, Debug)]
pub struct GateLeadParams {
    pub account_id: String,
    /// Per-trade notional in USD.
    pub size_usd: f64,
    /// Minimum Gate mid move (bp) to trigger an entry.
    pub min_move_bp: f64,
    /// Lookback for the Gate move (seconds). Anchor = mid `anchor_s` ago.
    pub anchor_s: f64,
    /// Maximum hold duration after entry (seconds).
    pub hold_max_s: f64,
    /// Take-profit: close when Flipster mid moves this many bp in our favor.
    pub exit_bp: f64,
    /// Stop-loss: close when Flipster mid moves this many bp against.
    pub stop_bp: f64,
    /// After firing on a base, ignore further triggers for `cooldown_s` sec.
    pub cooldown_s: f64,
    /// Per-leg taker fee in bp (Flipster only — single-leg paper).
    pub fee_bp_per_side: f64,
    /// Backtest mode flag. When true, on_tick skips the wall-clock stale
    /// check so historical replay ticks aren't all dropped (their
    /// event_ts is hours/days behind wall_now by definition).
    pub backtest_mode: bool,
    /// Whitelist of bases to trade (empty = all bases).
    pub whitelist: Vec<String>,
    /// Blacklist of bases to never trade. Always applied (overrides
    /// whitelist). Use for symbols with confirmed anti-edge or excessive
    /// spread cost.
    pub blacklist: Vec<String>,
    /// Mode tag written to position_log.mode. Default "paper" for live
    /// shadow runs. Backtest replay sweeps override this with the
    /// `BACKTEST_TAG` value so summaries can group/filter cleanly.
    pub mode_tag: String,
}

impl Default for GateLeadParams {
    fn default() -> Self {
        Self {
            account_id: "BINANCE_LEAD_v1".to_string(),
            size_usd: 5.0,
            min_move_bp: 20.0,
            anchor_s: 3.0,
            hold_max_s: 2.0,
            exit_bp: 5.0,
            stop_bp: 8.0,
            cooldown_s: 3.0,
            fee_bp_per_side: 0.85,
            backtest_mode: false,
            // 31 symbols screened from 418 Binance∩Flipster candidates
            // (2026-04-28). STRICT pass: n≥30, win%≥65, avg_bp≥3 with
            // best (min_bp, anchor) chosen per-symbol but global default
            // here is min_move_bp=20 (most pass at 25_3, some at 10/15
            // which we still accept at 20). Tick-size safety filter
            // applied (min_bp ≥ 5 × tick_bp).
            //
            // Replaces the 36-symbol Gate-screened list — most overlap
            // (BEAT, GRIFFAIN, JCT, PENGU, UAI, ENSO, POPCAT, CYS),
            // some ejected (GWEI, NOT, SONIC, etc — weak in Binance),
            // some added (D, ON, BOB, FIGHT, INX, BAS, AAVE, MASK,
            // PENDLE, BRETT, INTC, etc).
            whitelist: vec![
                "AAVE".into(),
                "BAS".into(),
                "BEAT".into(),
                "BOB".into(),
                "BRETT".into(),
                "CGPT".into(),
                "CROSS".into(),
                "CYS".into(),
                "D".into(),
                "ENSO".into(),
                "FIGHT".into(),
                "FLUID".into(),
                "GENIUS".into(),
                "GRIFFAIN".into(),
                "GUA".into(),
                "H".into(),
                "INTC".into(),
                "INX".into(),
                "JCT".into(),
                "KITE".into(),
                "MAGMA".into(),
                "MASK".into(),
                "ON".into(),
                "OPEN".into(),
                "PENDLE".into(),
                "PENGU".into(),
                "POPCAT".into(),
                "SNDK".into(),
                "TAG".into(),
                "TURTLE".into(),
                "UAI".into(),
            ],
            // Confirmed losers from live runs:
            //   M      — anti-edge: Binance moves invert on Flipster (1/15 win, -8.3 bp)
            //   BSB    — excessive spread (~5 bp entry slip eats the edge)
            //   SWARMS — 0/8 win in v20 ($-0.33), execution path consistently
            //            adverse despite TP signal
            //   ORCA   — 4/11 win in v20 ($-0.36), highest-loss-volume symbol
            blacklist: vec!["M".into(), "BSB".into(), "SWARMS".into(), "ORCA".into()],
            mode_tag: "paper".to_string(),
        }
    }
}

impl GateLeadParams {
    pub fn from_env() -> Self {
        let mut p = Self::default();
        if let Ok(v) = std::env::var("GL_ACCOUNT_ID") {
            p.account_id = v;
        }
        if let Some(v) = std::env::var("GL_SIZE_USD")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.size_usd = v;
        }
        if let Some(v) = std::env::var("GL_MIN_MOVE_BP")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.min_move_bp = v;
        }
        if let Some(v) = std::env::var("GL_ANCHOR_S")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.anchor_s = v;
        }
        if let Some(v) = std::env::var("GL_HOLD_MAX_S")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.hold_max_s = v;
        }
        if let Some(v) = std::env::var("GL_EXIT_BP")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.exit_bp = v;
        }
        if let Some(v) = std::env::var("GL_STOP_BP")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.stop_bp = v;
        }
        if let Some(v) = std::env::var("GL_COOLDOWN_S")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.cooldown_s = v;
        }
        if let Ok(v) = std::env::var("GL_WHITELIST") {
            p.whitelist = v
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.trim().to_uppercase())
                .collect();
        }
        if let Ok(v) = std::env::var("GL_BLACKLIST") {
            p.blacklist = v
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.trim().to_uppercase())
                .collect();
        }
        p
    }
}

/// Per-base state.
#[derive(Default)]
struct BaseState {
    gate_mids: VecDeque<(DateTime<Utc>, f64)>,
    /// Rolling Flipster mid history. Used by the velocity filter:
    /// if Flipster's already moving in the signal direction at signal
    /// time, our fill (300ms+ proxy RTT) lands after the lag is gone.
    flip_mids: VecDeque<(DateTime<Utc>, f64)>,
    flipster_bid: f64,
    flipster_ask: f64,
    flipster_ts: Option<DateTime<Utc>>,
    cooldown_until: Option<DateTime<Utc>>,
    open: Option<OpenPos>,
    /// Whipsaw guard: when the last trade closed via stop, hold off
    /// re-entry for an extended cooldown. Flash-spike chops (ALCH
    /// 13:04:11 → 13:04:15 = +50bp / -50bp / +50bp in 4s) had two
    /// stop-outs in a row; without this guard we'd keep diving in.
    last_stop_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct OpenPos {
    id: u64,
    side: i8, // +1 long, -1 short
    entry_ts: DateTime<Utc>,
    entry_price: f64,
    /// Flipster mid at signal time (used for exit-bp comparison).
    ref_mid: f64,
    deadline: DateTime<Utc>,
    gate_move_bp: f64,
}

pub async fn run(
    writer: IlpWriter,
    mut tick_rx: broadcast::Receiver<BookTick>,
    params: GateLeadParams,
) {
    info!(
        account_id = %params.account_id,
        min_move_bp = params.min_move_bp,
        anchor_s = params.anchor_s,
        hold_max_s = params.hold_max_s,
        exit_bp = params.exit_bp,
        stop_bp = params.stop_bp,
        whitelist = params.whitelist.join(","),
        "[gate_lead] starting"
    );

    // Per-base state, keyed by upper-case base symbol (e.g. "TURTLE").
    let state: Arc<Mutex<HashMap<String, BaseState>>> = Arc::new(Mutex::new(HashMap::new()));

    // Background sweeper for time-based exits — fires every 100 ms.
    {
        let state = state.clone();
        let writer = writer.clone();
        let params = params.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(StdDuration::from_millis(100));
            loop {
                iv.tick().await;
                sweep_exits(&state, &writer, &params).await;
            }
        });
    }

    // Tick ingest loop.
    loop {
        match tick_rx.recv().await {
            Ok(t) => {
                if let Err(e) = on_tick(&state, &writer, &t, &params).await {
                    warn!(error = %e, "[gate_lead] on_tick err");
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(missed = n, "[gate_lead] tick lagged");
            }
            Err(_) => break,
        }
    }
}

/// Result of mutating an open position inside the lock — picked up by the
/// caller and dispatched to writer/publisher AFTER the lock is released so
/// async I/O doesn't starve other tasks.
struct CloseAction {
    base: String,
    pos: OpenPos,
    exit_price: f64,
    reason: &'static str,
    exit_ts: DateTime<Utc>,
}

struct OpenAction {
    base: String,
    pos_id: u64,
    side_s: &'static str,
    size_usd: f64,
    ref_mid: f64,
    gate_mid: f64,
    flipster_bid: Option<f64>,
    flipster_ask: Option<f64>,
    /// Anchor venue (Binance) BBO at decision time. `Option` because the
    /// strategy fires from a Binance mid event, not a BBO snapshot — bid/
    /// ask are what we have stored in BaseState, which may have been
    /// updated independently from the trigger tick.
    binance_bid: Option<f64>,
    binance_ask: Option<f64>,
    ts: DateTime<Utc>,
}

async fn on_tick(
    state: &Arc<Mutex<HashMap<String, BaseState>>>,
    writer: &IlpWriter,
    tick: &BookTick,
    params: &GateLeadParams,
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
    if !params.whitelist.is_empty() && !params.whitelist.contains(&base) {
        return Ok(());
    }
    if params.blacklist.contains(&base) {
        return Ok(());
    }
    // Drop stale ticks: if event time is more than 1s behind wall clock,
    // the upstream pipeline (data_publisher → ZMQ → us) has fallen
    // behind. Acting on stale data leads to insta-timeout: pos.deadline
    // (event_ts + hold_max_s) is already past by the time we open. After
    // this filter, event_ts ≈ wall_now, so the rest of the strategy can
    // continue using event timestamps for rolling-window math.
    if !params.backtest_mode {
        let wall_now = Utc::now();
        if (wall_now - tick.timestamp).num_milliseconds() > 1000 {
            return Ok(());
        }
    }
    let now = tick.timestamp;

    let mut open_act: Option<OpenAction> = None;
    let mut close_act: Option<CloseAction> = None;
    {
        let mut s = state.lock().await;
        let entry = s.entry(base.clone()).or_default();

        match tick.exchange {
            // Leader exchange (Binance) updates rolling history + may trigger
            // entry. Was Gate originally; switched to Binance because it has
            // tighter price discovery and the same ~36 alts are available.
            ExchangeName::Binance => {
                entry.gate_mids.push_back((now, mid));
                // Keep `2 * anchor_s` of history so the anchor lookup always
                // has at least one tick old enough (when ticks are sparse, the
                // tighter buffer used to drop the only candidate).
                let cutoff = now - Duration::seconds((params.anchor_s * 2.0) as i64);
                while let Some((t, _)) = entry.gate_mids.front() {
                    if *t < cutoff {
                        entry.gate_mids.pop_front();
                    } else {
                        break;
                    }
                }

                // Cooldown / open check — never re-enter while one is open or
                // we're cooling down on this base.
                if entry.open.is_some() {
                    return Ok(());
                }
                if let Some(cd) = entry.cooldown_until {
                    if now < cd {
                        return Ok(());
                    }
                }
                // Whipsaw cooldown: after a stop-out, hold the symbol
                // off for `GL_STOP_COOLDOWN_S` (default 30s). Stops
                // typically signal the venue is mid-spike — re-entering
                // immediately catches the second leg of the whipsaw.
                let stop_cd_s: f64 = std::env::var("GL_STOP_COOLDOWN_S")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(30.0);
                if let Some(stop_at) = entry.last_stop_at {
                    let elapsed_s = (now - stop_at).num_milliseconds() as f64 / 1000.0;
                    if elapsed_s < stop_cd_s {
                        info!(
                            base = %base,
                            elapsed_s = format!("{:.1}", elapsed_s),
                            stop_cd_s,
                            "[gate_lead] SKIP — whipsaw stop-cooldown"
                        );
                        return Ok(());
                    }
                }

                // Anchor = median of ticks within ±half_window of
                // (now - anchor_s). Single-tick anchors were getting
                // fooled by transient spikes — e.g. SOMI 11:17:28 took
                // a 1ms spike to 0.1905 as the anchor while the price
                // was actually oscillating around 0.1898, fabricating a
                // -36bp move. The median over ~1s window suppresses
                // outlier ticks while still tracking real moves at the
                // sub-second level.
                //
                // Window is configurable via GL_ANCHOR_WINDOW_MS
                // (default 1000ms = ±500ms). Setting it to 0 falls
                // back to the original single-tick behavior.
                let target = now - Duration::milliseconds((params.anchor_s * 1000.0) as i64);
                let win_ms: i64 = std::env::var("GL_ANCHOR_WINDOW_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1000);
                let half = Duration::milliseconds(win_ms / 2);
                let lo = target - half;
                let hi = target + half;
                let mut window: Vec<f64> = entry
                    .gate_mids
                    .iter()
                    .filter(|(t, _)| *t >= lo && *t <= hi)
                    .map(|(_, m)| *m)
                    .collect();
                let anchor = if !window.is_empty() {
                    window.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    window[window.len() / 2]
                } else {
                    // Window empty (warmup or sparse ticks) — fall back
                    // to closest-tick logic from before.
                    let closest = entry
                        .gate_mids
                        .iter()
                        .find(|(t, _)| *t > target)
                        .map(|(_, m)| *m)
                        .or_else(|| entry.gate_mids.front().map(|(_, m)| *m));
                    let Some(c) = closest else { return Ok(()); };
                    c
                };
                if anchor <= 0.0 {
                    return Ok(());
                }
                let move_bp = (mid - anchor) / anchor * 1e4;
                if move_bp.abs() < params.min_move_bp {
                    return Ok(());
                }

                // Need a fresh Flipster quote to actually trade against.
                let f_ts = entry.flipster_ts;
                let stale = f_ts
                    .map(|t| (now - t).num_milliseconds() > 2000)
                    .unwrap_or(true);
                if stale || entry.flipster_bid <= 0.0 || entry.flipster_ask <= 0.0 {
                    return Ok(());
                }

                let side: i8 = if move_bp > 0.0 { 1 } else { -1 };

                // Lag filter: only enter when Flipster is BEHIND Binance in
                // the move's direction (i.e. there's still lag to capture).
                // Without this we kept entering after Flipster had already
                // overshot Binance, immediately reverting against us
                // (e.g. GWEI 10:46:10 -9.55bp, 10:46:02 -16.47bp). Compute
                // signed lag in bp where positive = Flipster is in the
                // direction the strategy wants to ride; negative = Flipster
                // already past Binance, lag inverted, do not trade.
                let flip_mid = (entry.flipster_bid + entry.flipster_ask) * 0.5;
                let raw_lag_bp = (mid - flip_mid) / flip_mid * 1e4;
                let lag_bp = raw_lag_bp * (side as f64); // sign-aligned to trade direction
                let min_lag_bp: f64 = std::env::var("GL_MIN_LAG_BP")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                if lag_bp < min_lag_bp {
                    info!(
                        base = %base,
                        side = if side == 1 { "long" } else { "short" },
                        gate_move_bp = format!("{:+.1}", move_bp),
                        flip_lag_bp = format!("{:+.1}", lag_bp),
                        min_lag_bp,
                        "[gate_lead] SKIP — Flipster already ahead of Binance"
                    );
                    return Ok(());
                }

                // Velocity filter: if Flipster's already moving in the
                // signal direction over the last GL_FLIP_VEL_WINDOW_MS,
                // it's catching up — by the time our order lands the lag
                // will be gone. Compare current mid to mid `vel_window`
                // ago. If Flipster move (signed by side) > max_vel_bp,
                // skip. Default vel_window=500ms, max_vel=8bp.
                let vel_window_ms: i64 = std::env::var("GL_FLIP_VEL_WINDOW_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(500);
                let max_vel_bp: f64 = std::env::var("GL_FLIP_VEL_MAX_BP")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(8.0);
                let flip_anchor_ts = now - Duration::milliseconds(vel_window_ms);
                let flip_anchor_mid = entry.flip_mids
                    .iter()
                    .find(|(t, _)| *t > flip_anchor_ts)
                    .map(|(_, m)| *m)
                    .or_else(|| entry.flip_mids.front().map(|(_, m)| *m));
                if let Some(fa) = flip_anchor_mid {
                    if fa > 0.0 {
                        let raw_flip_move = (flip_mid - fa) / fa * 1e4;
                        let flip_move_bp = raw_flip_move * (side as f64);
                        if flip_move_bp > max_vel_bp {
                            info!(
                                base = %base,
                                side = if side == 1 { "long" } else { "short" },
                                gate_move_bp = format!("{:+.1}", move_bp),
                                flip_vel_bp = format!("{:+.1}", flip_move_bp),
                                max_vel_bp,
                                vel_window_ms,
                                "[gate_lead] SKIP — Flipster already catching up"
                            );
                            return Ok(());
                        }
                    }
                }

                // Whipsaw filter A — BIN retrace from peak within anchor window.
                // Catches DOLO 14:18 / TAG 14:20:55 pattern: BIN dipped briefly,
                // partially bounced back, and the dip-then-bounce got read as a
                // valid signal. We check whether BIN reached a more-extreme
                // aligned move at any time within [anchor_target, now] and the
                // current value has retraced from that peak by ≥ N bp. If yes,
                // the move is already reversing → skip.
                let bin_retrace_bp: f64 = std::env::var("GL_BIN_RETRACE_BP")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                if bin_retrace_bp > 0.0 {
                    let mut peak_aligned = move_bp.abs();
                    for (t, m) in entry.gate_mids.iter() {
                        if *t < target { continue; }
                        let aligned = (m - anchor) / anchor * 1e4 * (side as f64);
                        if aligned > peak_aligned { peak_aligned = aligned; }
                    }
                    let retrace = peak_aligned - move_bp.abs();
                    if retrace > bin_retrace_bp {
                        info!(
                            base = %base,
                            side = if side == 1 { "long" } else { "short" },
                            gate_move_bp = format!("{:+.1}", move_bp),
                            peak_bp = format!("{:+.1}", peak_aligned),
                            retrace_bp = format!("{:+.1}", retrace),
                            bin_retrace_bp,
                            "[gate_lead] SKIP — BIN already retraced from peak"
                        );
                        return Ok(());
                    }
                }

                // Whipsaw filter B — pre-anchor revert. The anchor is N seconds
                // back, but if BIN was at the *current* level *before* the
                // anchor, the apparent move is just a return to baseline rather
                // than a fresh lead-move. Check ticks in
                // [anchor_target - pre_window, anchor_target]: if any of them
                // sit within `pre_revert_bp` of `mid` (signed), this looks like
                // a round-trip dip.
                let pre_revert_bp: f64 = std::env::var("GL_BIN_PRE_REVERT_BP")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0);
                let pre_window_ms: i64 = std::env::var("GL_BIN_PRE_WINDOW_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(2000);
                if pre_revert_bp > 0.0 {
                    let pre_lo = target - Duration::milliseconds(pre_window_ms);
                    let mut prior_at_current = false;
                    for (t, m) in entry.gate_mids.iter() {
                        if *t < pre_lo || *t >= target { continue; }
                        let aligned_to_now = (mid - m) / m * 1e4 * (side as f64);
                        if aligned_to_now.abs() <= pre_revert_bp {
                            prior_at_current = true;
                            break;
                        }
                    }
                    if prior_at_current {
                        info!(
                            base = %base,
                            side = if side == 1 { "long" } else { "short" },
                            gate_move_bp = format!("{:+.1}", move_bp),
                            pre_window_ms,
                            pre_revert_bp,
                            "[gate_lead] SKIP — BIN was at current level before anchor (round-trip)"
                        );
                        return Ok(());
                    }
                }

                // Long: pay ask. Short: pay bid.
                let entry_price = if side == 1 {
                    entry.flipster_ask
                } else {
                    entry.flipster_bid
                };
                let ref_mid = (entry.flipster_bid + entry.flipster_ask) * 0.5;
                let pos_id = pairs_core::pos_id::global().next();
                let pos = OpenPos {
                    id: pos_id,
                    side,
                    entry_ts: now,
                    entry_price,
                    ref_mid,
                    deadline: now + Duration::milliseconds((params.hold_max_s * 1000.0) as i64),
                    gate_move_bp: move_bp,
                };
                entry.open = Some(pos);
                entry.cooldown_until =
                    Some(now + Duration::milliseconds((params.cooldown_s * 1000.0) as i64));

                info!(
                    base = %base,
                    side = if side == 1 { "long" } else { "short" },
                    gate_move_bp = format!("{:+.1}", move_bp),
                    f_entry = entry_price,
                    pos_id,
                    "[gate_lead] OPEN"
                );
                // Defer the write_trade_signal + publish to AFTER we drop the
                // lock — async I/O in here would starve other tick handlers.
                // BBO snapshot for the executor: Flipster comes from
                // BaseState (known fresh — we just gated on staleness above);
                // Binance comes from the trigger tick we're currently
                // dispatching, which is the latest BBO by definition.
                open_act = Some(OpenAction {
                    base: base.clone(),
                    pos_id,
                    side_s: if side == 1 { "long" } else { "short" },
                    size_usd: params.size_usd,
                    ref_mid,
                    gate_mid: mid,
                    flipster_bid: Some(entry.flipster_bid),
                    flipster_ask: Some(entry.flipster_ask),
                    binance_bid: Some(bid),
                    binance_ask: Some(ask),
                    ts: now,
                });
            }
            // Flipster updates the latest quote + may trigger exit.
            e if matches!(e, ExchangeName::Flipster) => {
                entry.flipster_bid = bid;
                entry.flipster_ask = ask;
                entry.flipster_ts = Some(now);

                // Track Flipster mid history for the velocity filter.
                // Keep ~3s of history (matches anchor_s window).
                let flip_mid = (bid + ask) * 0.5;
                if flip_mid > 0.0 {
                    entry.flip_mids.push_back((now, flip_mid));
                    let cutoff = now - Duration::seconds(3);
                    while let Some((t, _)) = entry.flip_mids.front() {
                        if *t < cutoff { entry.flip_mids.pop_front(); } else { break; }
                    }
                }

                // Check exit on open position. Three triggers (in order):
                // (a) Flipster mid moved exit_bp in our favor (TP)
                // (b) Flipster mid moved stop_bp against us (stop)
                // (c) deadline passed (timeout) — checked here too so we don't
                //     rely solely on the 100ms sweeper which can fall behind
                //     under tick load.
                if let Some(pos) = entry.open.clone() {
                    let mut exit_pick: Option<(&'static str, f64)> = None;
                    if pos.ref_mid > 0.0 {
                        let move_signed =
                            ((mid - pos.ref_mid) / pos.ref_mid * 1e4) * (pos.side as f64);
                        if move_signed >= params.exit_bp {
                            exit_pick = Some(("tp", if pos.side == 1 { bid } else { ask }));
                        } else if move_signed <= -params.stop_bp {
                            // Stop grace — within the first GL_STOP_GRACE_MS of
                            // entry, ignore stop unless the move is past the
                            // hard cap. Catches GWEI 14:19:20-style whipsaws
                            // where entry slip causes a momentary spike (10ms
                            // post-entry the mid was -19bp, then 600ms later
                            // the venue actually crashed +57bp in our favor;
                            // tight stop locked us out of the real move).
                            let grace_ms: i64 = std::env::var("GL_STOP_GRACE_MS")
                                .ok()
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(0);
                            let hard_bp: f64 = std::env::var("GL_STOP_HARD_BP")
                                .ok()
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(100.0);
                            let elapsed_ms = (now - pos.entry_ts).num_milliseconds();
                            let in_grace = grace_ms > 0 && elapsed_ms < grace_ms;
                            let catastrophic = move_signed <= -hard_bp;
                            if !in_grace || catastrophic {
                                exit_pick = Some(("stop", if pos.side == 1 { bid } else { ask }));
                            }
                            // else: ride the spike; another tick will re-check.
                        }
                    }
                    if exit_pick.is_none() && now >= pos.deadline {
                        exit_pick = Some(("timeout", if pos.side == 1 { bid } else { ask }));
                    }
                    if let Some((reason, exit_price)) = exit_pick {
                        entry.open = None;
                        if reason == "stop" {
                            entry.last_stop_at = Some(now);
                        }
                        close_act = Some(CloseAction {
                            base: base.clone(),
                            pos,
                            exit_price,
                            reason,
                            exit_ts: now,
                        });
                    }
                }
            }
            _ => {}
        }
    } // release state mutex before any async I/O

    if let Some(o) = open_act {
        crate::coordinator::route_signal(
            &params.account_id,
            &o.base,
            "entry",
            o.side_s,
            o.size_usd,
            o.ref_mid,
            o.gate_mid,
            o.pos_id,
            o.ts,
            crate::signal_publisher::SignalQuotes {
                flipster_bid: o.flipster_bid,
                flipster_ask: o.flipster_ask,
                binance_bid: o.binance_bid,
                binance_ask: o.binance_ask,
                ..Default::default()
            },
        );
        if let Err(e) = writer
            .write_trade_signal(
                &params.account_id,
                &o.base,
                "entry",
                o.side_s,
                o.size_usd,
                o.ref_mid,
                o.gate_mid,
                o.pos_id,
                o.ts,
            )
            .await
        {
            warn!(error = %e, "[gate_lead] entry signal write failed");
        }
    }
    if let Some(c) = close_act {
        log_close(c, writer, params).await;
    }
    Ok(())
}

async fn sweep_exits(
    state: &Arc<Mutex<HashMap<String, BaseState>>>,
    writer: &IlpWriter,
    params: &GateLeadParams,
) {
    let now = Utc::now();
    let mut closes: Vec<CloseAction> = Vec::new();
    {
        let mut s = state.lock().await;
        let bases: Vec<String> = s
            .iter()
            .filter_map(|(k, v)| {
                v.open.as_ref().and_then(|p| {
                    if p.deadline <= now {
                        Some(k.clone())
                    } else {
                        None
                    }
                })
            })
            .collect();
        for base in bases {
            let Some(entry) = s.get_mut(&base) else {
                continue;
            };
            let Some(pos) = entry.open.clone() else {
                continue;
            };
            let exit_price = if entry.flipster_bid > 0.0 && entry.flipster_ask > 0.0 {
                if pos.side == 1 {
                    entry.flipster_bid
                } else {
                    entry.flipster_ask
                }
            } else {
                pos.ref_mid
            };
            let reason = if entry.flipster_bid > 0.0 && entry.flipster_ask > 0.0 {
                "timeout"
            } else {
                "no_quote"
            };
            entry.open = None;
            closes.push(CloseAction {
                base,
                pos,
                exit_price,
                reason,
                exit_ts: now,
            });
        }
    }
    for c in closes {
        log_close(c, writer, params).await;
    }
}

/// Persist close + emit ZMQ. Caller must have already cleared `entry.open`
/// inside the lock to release it before this point.
async fn log_close(c: CloseAction, writer: &IlpWriter, params: &GateLeadParams) {
    let pnl_bp = single_leg_pnl_bp(c.pos.entry_price, c.exit_price, c.pos.side as i32);
    let net_bp = pnl_bp - 2.0 * params.fee_bp_per_side;

    info!(
        base = %c.base,
        side = if c.pos.side == 1 { "long" } else { "short" },
        reason = c.reason,
        gate_move_bp = format!("{:+.1}", c.pos.gate_move_bp),
        f_entry = c.pos.entry_price,
        f_exit = c.exit_price,
        pnl_bp = format!("{:+.2}", pnl_bp),
        net_bp = format!("{:+.2}", net_bp),
        hold_ms = (c.exit_ts - c.pos.entry_ts).num_milliseconds(),
        "[gate_lead] CLOSE"
    );

    let side_s = if c.pos.side == 1 { "long" } else { "short" };
    if let Err(e) = writer
        .write_position(
            &params.account_id,
            &c.base,
            side_s,
            c.pos.entry_price,
            c.exit_price,
            params.size_usd,
            net_bp,
            c.pos.entry_ts,
            c.exit_ts,
            "gate_lead",
            c.reason,
            &params.mode_tag,
        )
        .await
    {
        warn!(error = %e, "[gate_lead] position_log write failed");
    }

    crate::coordinator::route_signal(
        &params.account_id,
        &c.base,
        "exit",
        side_s,
        params.size_usd,
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
            params.size_usd,
            c.exit_price,
            c.exit_price,
            c.pos.id,
            c.exit_ts,
        )
        .await
    {
        warn!(error = %e, "[gate_lead] exit signal write failed");
    }
}

/// Local thunk over `pairs_core::base_of`. See that function for normalization
/// rules — handles `BEAT_USDT` / `BEATUSDT` / `BTCUSDT.PERP` consistently.
fn base_of(exchange: ExchangeName, symbol: &str) -> Option<String> {
    pairs_core_base_of(exchange, symbol)
}

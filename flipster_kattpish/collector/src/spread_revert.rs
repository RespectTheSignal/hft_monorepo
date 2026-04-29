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
//!     If edge_short > 0  → SHORT Flipster
//!     edge_long = (bf_avg_gap_bp - current_gap_bp) - (same costs)
//!     If edge_long > 0   → LONG Flipster
//!
//! Baseline source: every 5 s `baseline_writer` queries QuestDB for the
//! 30-min `bf_avg_gap_bp` and 10-min per-venue `avg_spread_bp` per base
//! (matched-bucket INNER JOIN — see baseline_writer.rs). The result lives
//! in `SharedBaselines = Arc<RwLock<HashMap<base, BaselinePoint>>>`. We
//! read this map on every tick — no local rolling-window duplication.
//!
//! Exit:
//!   (a) stop-loss: gap moved `stop_bp` adversely from entry (default 20 bp)
//!   (b) timeout: position older than `max_hold_s` (default 24 h)
//!   (c) reverse: opposite-side entry signal closes existing positions
//!       (implicit take-profit per Jay's spec)
//!
//! Sizing: fixed `entry_size_usd` per trade, capped at `max_open_positions`
//! globally and `max_positions_per_base` per symbol. Per-base re-entry is
//! gated by `entry_cooldown_ms` (default 500 ms).
//!
//! QuestDB separation: writes to `position_log` with
//!   strategy = "spread_revert"
//!   account_id = "SPREAD_REVERT_v1" (or SR_ACCOUNT_ID env)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use tokio::sync::{broadcast, Mutex};
use tracing::{info, warn};

use crate::baseline_writer::{BaselinePoint, SharedBaselines};
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
    /// Maximum simultaneous positions on the same base.
    pub max_positions_per_base: usize,
    /// Extra bp required on top of measured costs to trigger entry.
    pub entry_threshold_bp: f64,
    /// Stop-loss: close when the gap moves `stop_bp` adversely from entry.
    pub stop_bp: f64,
    /// Hard max hold (seconds). Past this the position closes with
    /// reason="timeout", separate from price-stop.
    pub max_hold_s: f64,
    /// Per-base cooldown after entry (ms). Blocks a same-symbol entry
    /// from firing again within this window.
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
            entry_threshold_bp: 2.0,
            stop_bp: 20.0,
            max_hold_s: 86_400.0, // 24 h
            entry_cooldown_ms: 500,
            flipster_fee_bp: 0.425,
            backtest_mode: false,
            whitelist: Vec::new(),
            blacklist: vec!["M".into(), "BSB".into(), "SWARMS".into(), "ORCA".into()],
        }
    }
}

impl SpreadRevertParams {
    pub fn from_env() -> Self {
        let mut p = Self::default();
        if let Ok(v) = std::env::var("SR_ACCOUNT_ID") {
            p.account_id = v;
        }
        if let Some(v) = std::env::var("SR_SIZE_USD")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.entry_size_usd = v;
        }
        if let Some(v) = std::env::var("SR_MAX_POSITIONS")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.max_open_positions = v;
        }
        if let Some(v) = std::env::var("SR_MAX_POSITIONS_PER_BASE")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.max_positions_per_base = v;
        }
        if let Some(v) = std::env::var("SR_ENTRY_BP")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.entry_threshold_bp = v;
        }
        if let Some(v) = std::env::var("SR_STOP_BP")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.stop_bp = v;
        }
        if let Some(v) = std::env::var("SR_MAX_HOLD_S")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            p.max_hold_s = v;
        }
        if let Some(v) = std::env::var("SR_ENTRY_COOLDOWN_MS")
            .ok()
            .and_then(|s| s.parse().ok())
        {
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

#[derive(Default)]
struct BaseState {
    binance_bid: f64,
    binance_ask: f64,
    binance_ts: Option<DateTime<Utc>>,
    flipster_bid: f64,
    flipster_ask: f64,
    flipster_ts: Option<DateTime<Utc>>,
    /// Last successful entry timestamp on this base. Blocks a new entry
    /// for `entry_cooldown_ms` after firing.
    last_entry_at: Option<DateTime<Utc>>,
    /// Open positions on this base. Bounded by
    /// `params.max_positions_per_base`.
    open: Vec<OpenPos>,
}

#[derive(Debug, Clone)]
struct OpenPos {
    id: u64,
    side: i8, // +1 long flipster, -1 short flipster
    entry_ts: DateTime<Utc>,
    entry_price: f64,  // flipster fill price
    entry_gap_bp: f64, // gap at entry (used for stop-loss)
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
    baselines: SharedBaselines,
) {
    info!(
        account_id = %params.account_id,
        size_usd = params.entry_size_usd,
        max_pos = params.max_open_positions,
        max_per_base = params.max_positions_per_base,
        entry_bp = params.entry_threshold_bp,
        stop_bp = params.stop_bp,
        max_hold_s = params.max_hold_s,
        cooldown_ms = params.entry_cooldown_ms,
        whitelist = params.whitelist.join(","),
        "[spread_revert] starting (baseline-driven)"
    );

    let state: Arc<Mutex<HashMap<String, BaseState>>> = Arc::new(Mutex::new(HashMap::new()));

    // Wait up to 30 s for baseline_writer's first cycle so the strategy
    // doesn't miss its earliest entry signals on cold start.
    {
        let mut waited = 0u64;
        while baselines.read().await.is_empty() && waited < 30 {
            tokio::time::sleep(StdDuration::from_secs(1)).await;
            waited += 1;
        }
        let n = baselines.read().await.len();
        if n == 0 {
            warn!("[spread_revert] baselines still empty after 30 s — entries will idle until populated");
        } else {
            info!(n_bases = n, "[spread_revert] baselines ready");
        }
    }

    // Background sweeper for time-based exits — fires every 200 ms.
    {
        let state = state.clone();
        let writer = writer.clone();
        let params = params.clone();
        let baselines = baselines.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(StdDuration::from_millis(200));
            loop {
                iv.tick().await;
                sweep_exits(&state, &writer, &params, &baselines).await;
            }
        });
    }

    loop {
        match tick_rx.recv().await {
            Ok(t) => {
                if let Err(e) = on_tick(&state, &writer, &t, &params, &baselines).await {
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
    baselines: &SharedBaselines,
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

    // Read the latest baseline for this base BEFORE taking the state
    // lock — keeps the per-tick critical section short.
    let baseline: Option<BaselinePoint> = baselines.read().await.get(&base).cloned();

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
        let stale_b = entry
            .binance_ts
            .map(|t| (now - t).num_milliseconds() > 2000)
            .unwrap_or(true);
        let stale_f = entry
            .flipster_ts
            .map(|t| (now - t).num_milliseconds() > 2000)
            .unwrap_or(true);
        if stale_b || stale_f {
            return Ok(());
        }
        if entry.binance_bid <= 0.0
            || entry.binance_ask <= 0.0
            || entry.flipster_bid <= 0.0
            || entry.flipster_ask <= 0.0
        {
            return Ok(());
        }

        let bin_mid = (entry.binance_bid + entry.binance_ask) * 0.5;
        let fli_mid = (entry.flipster_bid + entry.flipster_ask) * 0.5;
        if bin_mid <= 0.0 || fli_mid <= 0.0 {
            return Ok(());
        }
        let gap_bp = (fli_mid - bin_mid) / bin_mid * 1e4;

        // Exit check — every open position evaluated independently
        // (stop / timeout). Reverse-close handled below.
        let mut keep: Vec<OpenPos> = Vec::with_capacity(entry.open.len());
        for pos in entry.open.drain(..) {
            let mut exit_pick: Option<(&'static str, f64)> = None;
            let loss = (pos.entry_price - fli_mid) * (pos.side as f64) / pos.entry_price * 1e4;
            if loss > params.stop_bp {
                let exit_px = if pos.side == 1 {
                    entry.flipster_bid
                } else {
                    entry.flipster_ask
                };
                exit_pick = Some(("stop", exit_px));
            }
            if exit_pick.is_none() && now >= pos.deadline {
                let exit_px = if pos.side == 1 {
                    entry.flipster_bid
                } else {
                    entry.flipster_ask
                };
                exit_pick = Some(("timeout", exit_px));
            }
            match exit_pick {
                Some((reason, exit_price)) => {
                    let avg_gap_bp = baseline
                        .as_ref()
                        .map(|b| b.bf_avg_gap_bp)
                        .unwrap_or(pos.entry_gap_bp);
                    close_acts.push(CloseAction {
                        base: base.clone(),
                        pos,
                        exit_price,
                        exit_gap_bp: gap_bp,
                        avg_gap_bp,
                        reason,
                        exit_ts: now,
                    });
                }
                None => keep.push(pos),
            }
        }
        entry.open = keep;

        // Entry decision needs a baseline. If baseline_writer hasn't
        // produced data for this base yet, skip the entry path but
        // leave the stop/timeout exits we already evaluated above.
        let Some(bp) = baseline.as_ref() else {
            warn!(base = %base, "baseline_writer: no baseline found");
            return Ok(());
        };
        if !bp.bf_avg_gap_bp.is_finite()
            || !bp.flipster_avg_spread_bp.is_finite()
            || !bp.binance_avg_spread_bp.is_finite()
        {
            warn!(base = %base, "baseline_writer: invalid baseline values");
            return Ok(());
        }
        let avg_gap = bp.bf_avg_gap_bp;
        let avg_fs = bp.flipster_avg_spread_bp;
        let avg_bs = bp.binance_avg_spread_bp;

        // Global open count derived from state (single source of truth).
        // Cheap — ~400 bases × Vec.len(), microseconds.
        let cur_open_global: usize = s.values().map(|v| v.open.len()).sum();
        // Re-borrow `entry` (the previous .or_default() borrow ended above
        // at the s.values() iteration; HashMap doesn't let us hold both).
        let entry = s.entry(base.clone()).or_default();
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

        // Reverse-close: opposite-side signal closes existing positions
        // on this base. Same-side stacks instead.
        if let Some(want) = want_side {
            let mut keep: Vec<OpenPos> = Vec::with_capacity(entry.open.len());
            for pos in entry.open.drain(..) {
                if pos.side != want {
                    let exit_px = if pos.side == 1 {
                        entry.flipster_bid
                    } else {
                        entry.flipster_ask
                    };
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
                // No fetch_add reservation needed — we hold the state
                // mutex, so the count snapshot above is accurate.
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
        log_close(c, writer, params).await;
    }
    Ok(())
}

async fn sweep_exits(
    state: &Arc<Mutex<HashMap<String, BaseState>>>,
    writer: &IlpWriter,
    params: &SpreadRevertParams,
    baselines: &SharedBaselines,
) {
    let now = Utc::now();
    let mut closes: Vec<CloseAction> = Vec::new();
    // Snapshot the baseline map once for this sweep.
    let bl_snap = baselines.read().await.clone();
    {
        let mut s = state.lock().await;
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
            let Some(entry) = s.get_mut(&base) else {
                continue;
            };
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
            let avg_gap_bp = bl_snap.get(&base).map(|b| b.bf_avg_gap_bp).unwrap_or(0.0);
            let mut keep: Vec<OpenPos> = Vec::with_capacity(entry.open.len());
            for pos in entry.open.drain(..) {
                if pos.deadline > now {
                    keep.push(pos);
                    continue;
                }
                let exit_price = if entry.flipster_bid > 0.0 && entry.flipster_ask > 0.0 {
                    if pos.side == 1 {
                        entry.flipster_bid
                    } else {
                        entry.flipster_ask
                    }
                } else {
                    pos.entry_price
                };
                let exit_gap_bp = gap_bp.unwrap_or(pos.entry_gap_bp);
                closes.push(CloseAction {
                    base: base.clone(),
                    pos,
                    exit_price,
                    exit_gap_bp,
                    avg_gap_bp,
                    reason: "timeout",
                    exit_ts: now,
                });
            }
            entry.open = keep;
        }
    }
    for c in closes {
        log_close(c, writer, params).await;
    }
}

async fn log_close(c: CloseAction, writer: &IlpWriter, params: &SpreadRevertParams) {
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

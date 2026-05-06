//! Single-leg BingX executor.
//!
//! Subscribes to the same ZMQ `signal` topic the Flipster executor uses
//! (filter: `account_id == BX_LIVE_v1` by default), and routes
//! entry/exit events to BingX market orders via `bingx_helpers::BingxClient`.
//!
//! Compared to the Flipster `executor.rs` (~2200 lines of dual-leg
//! Flipster+Gate hedging), this is intentionally minimal:
//!
//! - One leg only (BingX). No hedge.
//! - Market orders only. (Limit IOC is a future improvement.)
//! - In-memory `positionId` tracking by base symbol. On entry response we
//!   capture the BingX `positionId`; on exit we close 100% via that id.
//! - Risk gates: dry-run flag, max daily loss, max open positions.
//! - Cookie + JWT auth, NO API key/secret. Rotate via `dump_cookies.py`
//!   + JWT refresh from a logged-in browser session.
//!
//! Env (all required unless noted):
//!
//! | var                   | meaning                                        |
//! |-----------------------|-----------------------------------------------|
//! | BINGX_USER_ID         | numeric account id                            |
//! | BINGX_JWT             | bearer token                                  |
//! | BINGX_VARIANT         | account_id filter (default BX_LIVE_v1)        |
//! | BINGX_SIZE_USD        | per-trade notional (default $20)              |
//! | BINGX_MAX_OPEN        | max simultaneous open positions (default 5)   |
//! | BINGX_DAILY_LOSS_USD  | kill switch (default -50)                     |
//! | BINGX_DRY_RUN         | "1" → log only, no API call                   |
//! | SIGNAL_PUB_ADDR       | ZMQ SUB endpoint (default ipc:///tmp/...sock) |
//! | BINGX_COOKIES_PATH    | optional override                             |

use std::collections::{HashMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

#[path = "../bingx_helpers.rs"]
mod bingx_helpers;
#[path = "../bingx_official.rs"]
mod bingx_official;
#[path = "../fill_publisher.rs"]
mod fill_publisher;

use bingx_helpers::{coin_qty_for_usd, BingxClient, BingxIdentity, Side};
use bingx_official::BingxOfficialClient;

const DEFAULT_VARIANT: &str = "BX_LIVE_v1";
const DEFAULT_SIZE_USD: f64 = 20.0;
const DEFAULT_MAX_OPEN: usize = 5;
const DEFAULT_DAILY_LOSS_LIMIT_USD: f64 = -50.0;

/// Mirrors `pairs_core::TradeSignal`. Re-declared here so this binary doesn't
/// pull in the full pairs_core dependency tree (we only need a few fields).
#[derive(Debug, Clone, Deserialize)]
struct TradeSignal {
    account_id: String,
    base: String,
    /// "entry" | "exit"
    action: String,
    /// "long" | "short"
    side: String,
    #[serde(default)]
    size_usd: f64,
    /// Trigger price (last venue mid at signal time). We send this as
    /// `tradePrice` to BingX for slippage protection / display.
    #[serde(default)]
    flipster_price: f64,
    #[serde(default)]
    position_id: i64,
    timestamp: String,
    /// Best bid at signal time (api side). When present, executor places a
    /// MAKER limit entry at this price for LONGs (and `flipster_ask` for
    /// SHORTs) instead of a taker market open. Saves ~1bp/side fee.
    #[serde(default)]
    flipster_bid: Option<f64>,
    #[serde(default)]
    flipster_ask: Option<f64>,
}

/// Per-base running state.
struct OpenPosition {
    /// BingX-assigned positionId, returned in the open response.
    position_id: String,
    /// Side as we opened it.
    side: Side,
    /// Notional we sent.
    notional_usdt: f64,
    /// Avg fill price.
    entry_price: f64,
    /// Wall-clock open time.
    opened_at: chrono::DateTime<chrono::Utc>,
}

struct State {
    /// `base` → open position.
    open: HashMap<String, OpenPosition>,
    /// (base, position_id) we've already processed — dedup against ZMQ replays.
    seen_entries: HashSet<(String, i64)>,
    seen_exits: HashSet<(String, i64)>,
    /// Running PnL since process start, in USD. For the kill switch.
    realized_pnl_usd: f64,
    /// Trades actually placed (including dry-run).
    n_trades: u64,
}

impl State {
    fn new() -> Self {
        Self {
            open: HashMap::new(),
            seen_entries: HashSet::new(),
            seen_exits: HashSet::new(),
            realized_pnl_usd: 0.0,
            n_trades: 0,
        }
    }
}

#[derive(Clone)]
struct Cfg {
    variant: String,
    size_usd: f64,
    max_open: usize,
    daily_loss_limit_usd: f64,
    dry_run: bool,
}

impl Cfg {
    fn from_env() -> Self {
        Self {
            variant: env::var("BINGX_VARIANT").unwrap_or_else(|_| DEFAULT_VARIANT.into()),
            size_usd: env::var("BINGX_SIZE_USD")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_SIZE_USD),
            max_open: env::var("BINGX_MAX_OPEN")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_MAX_OPEN),
            daily_loss_limit_usd: env::var("BINGX_DAILY_LOSS_USD")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_DAILY_LOSS_LIMIT_USD),
            dry_run: env::var("BINGX_DRY_RUN").map(|v| v == "1").unwrap_or(false),
        }
    }
}

fn load_bingx_cookies() -> HashMap<String, String> {
    let path = env::var("BINGX_COOKIES_PATH")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config/flipster_kattpish/cookies.json"))
                .ok()
        });
    let Some(path) = path else { return HashMap::new(); };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let Some(obj) = v.get("bingx").and_then(|x| x.as_object()) else {
        return HashMap::new();
    };
    obj.iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}

fn base_to_bingx_symbol(base: &str) -> String {
    format!("{base}-USDT")
}

fn signal_age_ms(timestamp: &str) -> f64 {
    let parsed = chrono::DateTime::parse_from_rfc3339(timestamp).ok();
    let Some(ts) = parsed else { return 0.0; };
    (chrono::Utc::now() - ts.with_timezone(&chrono::Utc))
        .num_milliseconds() as f64
}

/// Age threshold in ms; signals older than this at receive time are dropped.
/// 500ms was too tight given our positionId resolution overhead (100-300ms)
/// + collector→ZMQ→executor queue. 1500ms still fits inside the strategy's
/// 2s hold cap, so we don't hold past the signal's intended lifetime.
const MAX_ENTRY_SIGNAL_AGE_MS: f64 = 1500.0;

async fn handle_entry(
    cfg: &Cfg,
    client: &BingxClient,
    official: &Arc<BingxOfficialClient>,
    state: &Arc<Mutex<State>>,
    ev: TradeSignal,
) -> Result<()> {
    let symbol = base_to_bingx_symbol(&ev.base);
    let side = match ev.side.as_str() {
        "long" => Side::Bid,
        "short" => Side::Ask,
        other => return Err(anyhow!("bad side {other}")),
    };

    // Risk gates inside the lock.
    {
        let s = state.lock().await;
        if s.realized_pnl_usd <= cfg.daily_loss_limit_usd {
            warn!(
                pnl = s.realized_pnl_usd,
                limit = cfg.daily_loss_limit_usd,
                "[bingx] kill switch — daily loss exceeded, skipping entry"
            );
            return Ok(());
        }
        if s.open.len() >= cfg.max_open {
            warn!(
                base = %ev.base,
                open = s.open.len(),
                max = cfg.max_open,
                "[bingx] max open reached, skipping entry"
            );
            return Ok(());
        }
        if s.open.contains_key(&ev.base) {
            // Already in a position on this base — don't stack.
            return Ok(());
        }
    }

    let last = if ev.flipster_price > 0.0 {
        ev.flipster_price
    } else {
        // Without a fresh price BingX will reject the order. Surface as error.
        return Err(anyhow!("no flipster_price on signal"));
    };

    info!(
        base = %ev.base, side = %ev.side, size_usd = cfg.size_usd, last,
        dry = cfg.dry_run, "[bingx] ENTRY"
    );

    if cfg.dry_run {
        let mut s = state.lock().await;
        s.n_trades += 1;
        let pid = format!("dry_{}", s.n_trades);
        s.open.insert(
            ev.base.clone(),
            OpenPosition {
                position_id: pid,
                side,
                notional_usdt: cfg.size_usd,
                entry_price: last,
                opened_at: chrono::Utc::now(),
            },
        );
        return Ok(());
    }

    let coin_qty = coin_qty_for_usd(cfg.size_usd, last);
    if coin_qty <= 0.0 {
        return Err(anyhow!("computed coin_qty <= 0 (size={}, last={})", cfg.size_usd, last));
    }

    // ENTRY mode: market is the default for this lead-lag strategy because
    // maker (limit at our bid/ask) had a structural 0% fill rate in the
    // 2026-05-06 test. The signal premise — "BingX will follow Binance's
    // recent move in the SAME direction" — means by the time the maker
    // limit lands, BingX is already moving AWAY from our resting price, so
    // no taker arrives to fill us. Market entry pays 1.6 bp taker fee +
    // ~half-spread crossing, which the fill-feedback `ref_mid` retune
    // (collector-side) makes the strategy aware of for accurate TP timing.
    //
    // BINGX_ENTRY_MODE=maker keeps the limit path available for symbols
    // where we eventually want to test it (top-of-book majors with tight
    // spreads + slow moves).
    let entry_mode = std::env::var("BINGX_ENTRY_MODE").unwrap_or_else(|_| "market".into());
    let resp = if entry_mode == "maker" {
        let maker_price = match side {
            Side::Bid => ev.flipster_bid.filter(|p| *p > 0.0).unwrap_or(last),
            Side::Ask => ev.flipster_ask.filter(|p| *p > 0.0).unwrap_or(last),
        };
        client
            .place_limit_open(&symbol, side, coin_qty, maker_price, false)
            .await?
    } else {
        client.place_market_open(&symbol, side, coin_qty, last).await?
    };
    let fill = bingx_helpers::extract_fill(&resp);

    // BingX returns immediate fills inline, otherwise the order is
    // resting. Wait briefly + check positions; cancel if not filled.
    let order_id_for_cancel = fill.order_id.clone();
    let initial_filled = fill.cum_filled_volume.unwrap_or(0.0) > 0.0;
    // BingX returns avg_filled_price=0 (or None) for orders that rest before
    // fill. Capture the actual fill price from /positions when we confirm
    // fill via polling; otherwise NaN/inf PnL math downstream.
    let mut resolved_entry_price: Option<f64> = None;
    let mut resolved_position_id: Option<String> = None;
    if !initial_filled {
        // Limit is resting. Wait up to 600ms for it to take.
        let mut filled = false;
        for attempt in 0..3u32 {
            tokio::time::sleep(std::time::Duration::from_millis(150 + 100 * attempt as u64))
                .await;
            if let Ok(positions) = official.get_positions_for(&symbol).await {
                if let Some(p) = positions
                    .iter()
                    .find(|p| p.position_amt.parse::<f64>().unwrap_or(0.0).abs() > 1e-12)
                {
                    filled = true;
                    resolved_entry_price = p.avg_price.parse::<f64>().ok();
                    resolved_position_id = Some(p.position_id.clone());
                    break;
                }
            }
        }
        if !filled {
            // Cancel the resting order via the official API and skip.
            if let Some(oid) = order_id_for_cancel {
                let _ = official.cancel_order(&symbol, &oid).await;
            }
            info!(
                base = %ev.base, side = %ev.side, last,
                "[bingx] limit OPEN unfilled — cancelled, signal skipped"
            );
            return Ok(());
        }
    }

    // Register the position IMMEDIATELY (positionId blank) so any exit
    // signal that arrives before positionId resolution can find the row.
    // The exit handler tolerates a blank position_id by re-querying
    // /positions just-in-time before close.
    let order_id = fill.order_id.clone().unwrap_or_default();
    // Pick the best entry_price source: the immediate fill response (market
    // or instant-cross limit), then the /positions avgPrice (resting limit),
    // and last the signal price as a fallback so PnL logging never NaN.
    let entry_price = fill
        .avg_filled_price
        .filter(|p| *p > 0.0)
        .or(resolved_entry_price)
        .unwrap_or(last);

    {
        let mut s = state.lock().await;
        s.n_trades += 1;
        s.open.insert(
            ev.base.clone(),
            OpenPosition {
                // Limit-fill path already resolved the positionId during the
                // fill-confirmation poll; reuse it. Market path leaves it
                // blank for the background resolver below.
                position_id: resolved_position_id.clone().unwrap_or_default(),
                side,
                notional_usdt: cfg.size_usd,
                entry_price,
                opened_at: chrono::Utc::now(),
            },
        );
        info!(
            base = %ev.base, order_id = %order_id,
            coin_qty, entry = entry_price,
            position_id = %resolved_position_id.clone().unwrap_or_default(),
            n_trades = s.n_trades,
            "[bingx] ENTRY filled"
        );
    }
    // Publish the actual fill back to the collector. The strategy uses this
    // to retune `ref_mid` to our REAL entry price (maker-side bid/ask)
    // instead of the signal-time mid — without it, TP fires too late on
    // wide-spread alts because mid-based exit thresholds ignore the
    // half-spread head start the maker entry already gave us.
    fill_publisher::publish_fill(
        &ev.account_id,
        ev.position_id,
        &ev.base,
        "entry",
        &ev.side,
        cfg.size_usd,
        entry_price,
        0.0,
        0.0,
        0.0,
        last,
        0.0,
        0,
        chrono::Utc::now(),
    );
    // If we already have positionId (limit path), skip the background resolver.
    if resolved_position_id.is_some() {
        return Ok(());
    }

    // Resolve positionId in the background. By the time exit fires (1-3s
    // later), the row has typically been populated.
    let base = ev.base.clone();
    let symbol_for_lookup = symbol.clone();
    let official_bg = official.clone();
    let state_bg = state.clone();
    tokio::spawn(async move {
        for attempt in 0..4u32 {
            tokio::time::sleep(std::time::Duration::from_millis(120 + 100 * attempt as u64))
                .await;
            // Stop trying if exit already cleared the position.
            {
                let s = state_bg.lock().await;
                if !s.open.contains_key(&base) {
                    return;
                }
            }
            match official_bg.get_positions_for(&symbol_for_lookup).await {
                Ok(positions) => {
                    if let Some(p) = positions
                        .iter()
                        .find(|p| p.position_amt.parse::<f64>().unwrap_or(0.0).abs() > 1e-12)
                    {
                        let mut s = state_bg.lock().await;
                        if let Some(op) = s.open.get_mut(&base) {
                            op.position_id = p.position_id.clone();
                            tracing::debug!(
                                base = %base, attempt,
                                position_id = %p.position_id,
                                "[bingx] positionId resolved"
                            );
                        }
                        return;
                    }
                }
                Err(e) => tracing::warn!(attempt, error = %e, "[bingx] positionId lookup err"),
            }
        }
        tracing::warn!(base = %base, "[bingx] positionId still missing after 4 attempts");
    });

    Ok(())
}

/// Look up positionId on demand (used in exit path when the background
/// resolver hasn't completed yet).
async fn resolve_position_id_now(
    official: &BingxOfficialClient,
    symbol: &str,
) -> Option<String> {
    match official.get_positions_for(symbol).await {
        Ok(positions) => positions
            .iter()
            .find(|p| p.position_amt.parse::<f64>().unwrap_or(0.0).abs() > 1e-12)
            .map(|p| p.position_id.clone()),
        Err(e) => {
            tracing::warn!(symbol, error = %e, "[bingx] positionId resolve_now err");
            None
        }
    }
}

async fn handle_exit(
    cfg: &Cfg,
    client: &BingxClient,
    official: &BingxOfficialClient,
    state: &Arc<Mutex<State>>,
    ev: TradeSignal,
) -> Result<()> {
    // Race: paper TP can fire <100 ms after the entry signal, before the
    // executor's market open returns + registers the position in state.
    // Without retry, the exit gets dropped → real position stranded with
    // no close (FORM 04:42:03 case 2026-05-06: entry @0.2679 lived for 10
    // min before manual reconciliation).
    //
    // Strategy `GL_HOLD_MAX_S=2s` is the absolute upper bound on a paper
    // position; allow up to 1.8 s for the entry-in-flight to land before
    // truly giving up. Polls every 100 ms.
    let mut pos_opt: Option<OpenPosition> = None;
    for attempt in 0..18u32 {
        {
            let mut s = state.lock().await;
            if let Some(p) = s.open.remove(&ev.base) {
                pos_opt = Some(p);
                break;
            }
        }
        if attempt == 0 {
            tracing::debug!(base = %ev.base, "[bingx] exit waiting for entry-in-flight");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let Some(pos) = pos_opt else {
        warn!(
            base = %ev.base,
            "[bingx] exit but no open position after 1.8s wait — likely never filled or already closed"
        );
        return Ok(());
    };

    let symbol = base_to_bingx_symbol(&ev.base);
    let opposite = match pos.side {
        Side::Bid => Side::Ask,
        Side::Ask => Side::Bid,
    };
    let last = if ev.flipster_price > 0.0 {
        ev.flipster_price
    } else {
        pos.entry_price
    };

    info!(
        base = %ev.base, dry = cfg.dry_run,
        held_ms = (chrono::Utc::now() - pos.opened_at).num_milliseconds(),
        entry = pos.entry_price, exit = last,
        "[bingx] EXIT"
    );

    if cfg.dry_run {
        let bp = match pos.side {
            Side::Bid => (last - pos.entry_price) / pos.entry_price * 10_000.0,
            Side::Ask => (pos.entry_price - last) / pos.entry_price * 10_000.0,
        };
        let pnl_usd = bp * pos.notional_usdt / 10_000.0;
        let mut s = state.lock().await;
        s.realized_pnl_usd += pnl_usd;
        info!(
            base = %ev.base, pnl_bp = format!("{bp:+.2}"),
            pnl_usd = format!("{pnl_usd:+.4}"),
            cum_pnl_usd = format!("{:+.2}", s.realized_pnl_usd),
            "[bingx] EXIT (dry)"
        );
        return Ok(());
    }

    // Use the cached positionId. If empty (background resolver hasn't
    // landed yet), block on a fresh /positions call.
    let pid = if pos.position_id.is_empty() {
        match resolve_position_id_now(official, &symbol).await {
            Some(p) => p,
            None => {
                warn!(base = %ev.base, "[bingx] cannot resolve positionId at exit; abort close");
                return Err(anyhow!("positionId unresolved for {}", ev.base));
            }
        }
    } else {
        pos.position_id.clone()
    };

    // EXIT mode B: ALWAYS market close. Tested 2026-05-05 with limit-then-
    // fallback path — limit closes only filled 53% (8/17 hit market fallback)
    // and the fallback path took massive losses on volatile spikes (worst
    // -254bp on a LAB pump). Going straight to market trades 1bp of maker
    // savings (close goes from 0.64bp maker → 1.6bp taker = +0.96bp cost)
    // for a much tighter loss tail. Net entry-maker / exit-taker fee budget
    // ~2.24 bp/RT, vs 1.28 bp/RT pure maker. Backtest avg edge +5bp easily
    // covers this.
    let resp = client
        .close_market_full(&symbol, opposite, Some(&pid), last)
        .await?;
    let fill = bingx_helpers::extract_fill(&resp);
    let raw_exit = fill.avg_filled_price.unwrap_or(0.0);
    // Sanity-check the exit price. If the response had no avg fill (or it's
    // zero / non-finite), fall back to the strategy's last quote rather
    // than 0.0 — using 0 produces -10000bp garbage and breaks the kill
    // switch via NaN/Inf accumulation.
    let exit_price = if raw_exit.is_finite() && raw_exit > 0.0 {
        raw_exit
    } else {
        warn!(
            base = %ev.base,
            "[bingx] close response missing avgFilledPrice; using signal last={last} for accounting"
        );
        last
    };

    let bp = if pos.entry_price > 0.0 && exit_price > 0.0 {
        match pos.side {
            Side::Bid => (exit_price - pos.entry_price) / pos.entry_price * 10_000.0,
            Side::Ask => (pos.entry_price - exit_price) / pos.entry_price * 10_000.0,
        }
    } else {
        warn!(
            base = %ev.base, entry = pos.entry_price, exit = exit_price,
            "[bingx] cannot compute bp: bad prices, recording 0"
        );
        0.0
    };
    // Net = gross - 2 * taker fee. BingX taker (Banner-corrected) ~1.6 bp.
    let net_bp = if bp.is_finite() { bp - 3.2 } else { 0.0 };
    let pnl_usd = net_bp * pos.notional_usdt / 10_000.0;

    let mut s = state.lock().await;
    // Final guard: clamp NaN/Inf to 0 so the kill switch comparison stays meaningful.
    if pnl_usd.is_finite() {
        s.realized_pnl_usd += pnl_usd;
    } else {
        warn!(base = %ev.base, "[bingx] non-finite pnl_usd; not adding to cum");
    }
    info!(
        base = %ev.base,
        order_id = %fill.order_id.unwrap_or_default(),
        entry = pos.entry_price, exit = exit_price,
        gross_bp = format!("{bp:+.2}"), net_bp = format!("{net_bp:+.2}"),
        pnl_usd = format!("{pnl_usd:+.4}"),
        cum_pnl_usd = format!("{:+.2}", s.realized_pnl_usd),
        "[bingx] EXIT filled"
    );
    drop(s);
    fill_publisher::publish_fill(
        &ev.account_id,
        ev.position_id,
        &ev.base,
        "exit",
        &ev.side,
        pos.notional_usdt,
        exit_price,
        0.0,
        0.0,
        0.0,
        last,
        0.0,
        0,
        chrono::Utc::now(),
    );
    Ok(())
}

fn run_subscriber(
    variant: String,
    tx: tokio::sync::mpsc::UnboundedSender<TradeSignal>,
) -> Result<()> {
    let addr = env::var("SIGNAL_PUB_ADDR")
        .unwrap_or_else(|_| "ipc:///tmp/flipster_kattpish_signal.sock".to_string());
    info!(%addr, %variant, "[bingx] zmq SUB connecting");
    let ctx = zmq::Context::new();
    let socket = ctx.socket(zmq::SUB)?;
    socket.connect(&addr)?;
    socket.set_subscribe(b"")?;
    let needle = format!("\"account_id\":\"{}\"", variant);
    loop {
        let msg = match socket.recv_bytes(0) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "[bingx] zmq recv err");
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
        };
        // Cheap pre-filter — drop frames not for our variant.
        if !needs_match(&msg, &needle) {
            continue;
        }
        let ev: TradeSignal = match serde_json::from_slice(&msg) {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "[bingx] deserialize err");
                continue;
            }
        };
        if ev.account_id != variant {
            continue;
        }
        if tx.send(ev).is_err() {
            break;
        }
    }
    Ok(())
}

fn needs_match(payload: &[u8], needle: &str) -> bool {
    if let Ok(s) = std::str::from_utf8(payload) {
        s.contains(needle)
    } else {
        false
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Cfg::from_env();
    info!(
        variant = %cfg.variant, size_usd = cfg.size_usd,
        max_open = cfg.max_open, daily_loss = cfg.daily_loss_limit_usd,
        dry = cfg.dry_run,
        "[bingx] executor starting"
    );

    // Bind the fill_publisher PUB socket so the collector's bingx_lead can
    // see real fill prices and retune its ref_mid (option B fix for the
    // "TP doesn't fire on profit" issue — strategy was using signal-time
    // mid, ignoring the half-spread head start that maker entry gives us).
    if let Err(e) = fill_publisher::init() {
        warn!(error = %e, "[bingx] fill_publisher init failed (continuing without)");
    }

    let user_id = env::var("BINGX_USER_ID")
        .map_err(|_| anyhow!("set BINGX_USER_ID env"))?;
    let jwt = env::var("BINGX_JWT").map_err(|_| anyhow!("set BINGX_JWT env"))?;
    let api_key = env::var("BINGX_API_KEY")
        .map_err(|_| anyhow!("set BINGX_API_KEY env (for position lookup)"))?;
    let api_secret = env::var("BINGX_API_SECRET")
        .map_err(|_| anyhow!("set BINGX_API_SECRET env"))?;

    let id = BingxIdentity::defaults_with_jwt(user_id, jwt);
    let cookies = load_bingx_cookies();
    let client = BingxClient::new(id, cookies)?;
    let client = Arc::new(client);
    let official = Arc::new(BingxOfficialClient::new(api_key, api_secret)?);

    // Sanity check at startup — fail fast if the API key isn't valid.
    match official.get_balance().await {
        Ok(b) => info!(
            available = %b.available_margin,
            equity = %b.equity,
            "[bingx] API key OK; balance loaded"
        ),
        Err(e) => return Err(anyhow!("BingX official API auth failed: {e}")),
    }

    let state = Arc::new(Mutex::new(State::new()));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TradeSignal>();
    let variant = cfg.variant.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = run_subscriber(variant, tx) {
            warn!(error = %e, "[bingx] subscriber crashed");
        }
    });

    while let Some(ev) = rx.recv().await {
        let cfg = cfg.clone();
        let client = client.clone();
        let official = official.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let res = match ev.action.as_str() {
                "entry" => {
                    if signal_age_ms(&ev.timestamp) > MAX_ENTRY_SIGNAL_AGE_MS {
                        info!(
                            base = %ev.base, age = signal_age_ms(&ev.timestamp),
                            "[bingx] ENTRY-DROP — stale signal"
                        );
                        return;
                    }
                    let key = (ev.base.clone(), ev.position_id);
                    {
                        let mut s = state.lock().await;
                        if !s.seen_entries.insert(key) {
                            return;
                        }
                    }
                    handle_entry(&cfg, &client, &official, &state, ev).await
                }
                "exit" => {
                    let key = (ev.base.clone(), ev.position_id);
                    {
                        let mut s = state.lock().await;
                        if !s.seen_exits.insert(key) {
                            return;
                        }
                    }
                    handle_exit(&cfg, &client, &official, &state, ev).await
                }
                other => {
                    warn!(action = %other, "[bingx] unknown action");
                    Ok(())
                }
            };
            if let Err(e) = res {
                warn!(error = %e, "[bingx] handler err");
            }
        });
    }
    Ok(())
}

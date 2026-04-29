//! Live executor — orchestrates trade_signal → Flipster + Gate orders.
//!
//! Imperative style matches the Python original (`scripts/live_executor.py`)
//! so behavior diffs against that reference are easy to spot. Each
//! `on_entry` / `on_exit` runs in its own `tokio::spawn` task; the only
//! shared state is the Arc'd clients + the `LivePosition` map + the
//! `seen_*` dedup sets + running stats.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Utc};
use dashmap::{DashMap, DashSet};
use tokio::sync::Mutex;

use flipster_client::{
    FlipsterClient, MarginType as FMarginType, OrderParams as FOrderParams,
    OrderType as FOrderType, Side as FSide,
};
use gate_client::{GateClient, OrderParams as GParams, OrderType as GType, Side as GSide, TimeInForce as GTif};

use crate::config::*;
use crate::flipster_helpers::{extract_fill as flip_extract_fill, limit_ioc_entry};
use crate::flipster_ws::SharedState as FlipsterWsState;
use crate::gate_contracts::{self, ContractMap};
use crate::gate_helpers::{extract_fill as gate_extract_fill, extract_order_id, extract_status};
use crate::trade_log::{TradeLog, TradeRecord};
use crate::zmq_sub::TradeSignal;

/// Decision-logic filters (chop_detect, dyn_filter, adjusted_size) moved
/// into `collector::coordinator` in Phase 3b. Default = bypassed in the
/// executor — the coordinator owns those decisions now. Set
/// `EXEC_LEGACY_FILTERS=1` to restore the old executor-side behavior as a
/// fallback (e.g. while validating the new collector path or rolling back).
///
/// Freshness gates that *do* still belong here regardless — stale signal
/// age and the QuestDB mid recheck — are unchanged.
fn legacy_filters_enabled() -> bool {
    std::env::var("EXEC_LEGACY_FILTERS").as_deref() == Ok("1")
}

/// Per-position bookkeeping used to compute exit PnL/slippage and write
/// the JSONL trade record. Mirrors Python `LivePosition`.
#[derive(Debug, Clone)]
pub struct LivePosition {
    pub position_id: i64,
    pub base: String,
    pub flipster_side: String,
    pub size_usd: f64,
    pub entry_time: String,
    pub flipster_entry_price: f64,
    pub flipster_size: f64,
    pub flipster_slot: u32,
    pub gate_entry_price: f64,
    pub gate_size: i64,
    pub gate_contract: String,
    pub entry_spread_bp: f64,
    pub entry_epoch: f64,
    pub peak_captured_bp: f64,
    pub f_entry_slip_bp: f64,
    pub g_entry_slip_bp: f64,
    pub signal_lag_ms_entry: f64,
    pub paper_f_entry: f64,
    pub paper_g_entry: f64,
    /// If pre-placed maker exit LIMIT exists, its order_id + price.
    pub exit_order_id: Option<String>,
    pub exit_limit_price: f64,
}

#[derive(Default)]
pub struct Stats {
    pub trade_count: AtomicU64,
    pub win_count: AtomicU64,
    pub total_pnl_micros: std::sync::atomic::AtomicI64, // PnL × 1e6 to keep atomic-i64 friendly
}

impl Stats {
    pub fn add(&self, net_usd: f64) {
        self.trade_count.fetch_add(1, Ordering::Relaxed);
        if net_usd > 0.0 {
            self.win_count.fetch_add(1, Ordering::Relaxed);
        }
        let micros = (net_usd * 1_000_000.0).round() as i64;
        self.total_pnl_micros.fetch_add(micros, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> (u64, u64, f64) {
        let n = self.trade_count.load(Ordering::Relaxed);
        let w = self.win_count.load(Ordering::Relaxed);
        let p = self.total_pnl_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        (n, w, p)
    }
    pub fn print(&self) {
        let (n, w, p) = self.snapshot();
        let pct = if n > 0 { w as f64 / n as f64 * 100.0 } else { 0.0 };
        tracing::info!(
            "[STATS] trades={} wins={} win%={:.1} total_pnl=${:+.4}",
            n, w, pct, p
        );
    }
}

pub struct Executor {
    pub variant: String,
    pub size_usd: f64,
    pub dry_run: bool,
    pub flipster_only: bool,
    /// Margin mode tag forwarded to Flipster on every entry order.
    /// "Cross" or "Isolated".
    pub margin: String,
    /// Use the multi-position endpoint (POST /api/v2/trade/positions/{symbol})
    /// for entries instead of the one-way endpoint. Each fill returns its
    /// own slot, so same-symbol stacking works. Account-level trade-mode
    /// must already be MULTIPLE_POSITIONS — set via web UI or
    /// --trade-mode CLI flag.
    pub multiple_positions: bool,
    pub blacklist: HashSet<String>,
    pub whitelist: HashSet<String>,

    pub flipster: Arc<FlipsterClient>,
    pub gate: Arc<GateClient>,
    pub gate_contracts: ContractMap,
    pub gate_leverage: u32,
    pub gate_leverage_set: Mutex<HashSet<String>>,
    pub flip_state: FlipsterWsState,
    pub qdb_http: reqwest::Client,

    pub open_positions: DashMap<(String, i64), LivePosition>,
    pub seen_entries: DashSet<(String, i64)>,
    pub seen_exits: DashSet<(String, i64)>,
    pub stats: Stats,
    pub trade_log: TradeLog,

    pub shutting_down: AtomicBool,
    /// Per-symbol "unmatched WS position first seen" — orphan-abort sweeper.
    pub unmatched_first_seen: DashMap<String, std::time::Instant>,
    /// Per-order_id "first seen open" — stale-order sweeper.
    pub order_first_seen: DashMap<String, std::time::Instant>,
    /// Last signal per base — used to detect side-flip chop and skip the
    /// reversed entry. Key=base, value=(side, timestamp).
    pub last_signal: DashMap<String, (String, std::time::Instant)>,
    pub sym_stats: Arc<crate::symbol_stats::SymbolStatsStore>,
    pub flip_contracts: crate::flipster_contracts::ContractMap,
}

impl Executor {
    /// Place a Flipster entry order. Routes through the multi-position
    /// `place_order` endpoint when the executor is in MULTIPLE_POSITIONS
    /// mode (each fill returns its own slot, allowing same-symbol stacks);
    /// otherwise uses the legacy `place_order_oneway` endpoint that nets
    /// into slot 0.
    ///
    /// The full set of arguments mirrors place_order_oneway so callers
    /// can switch over with no other changes; reduce_only / order_type /
    /// post_only are honored by both paths.
    #[allow(clippy::too_many_arguments)]
    pub async fn flipster_place(
        &self,
        symbol: &str,
        side: &str,
        amount_usd: f64,
        ref_price: f64,
        leverage: u32,
        reduce_only: bool,
        order_type: &str,
        post_only: bool,
    ) -> Result<serde_json::Value, flipster_client::FlipsterError> {
        if self.multiple_positions && !reduce_only {
            // Multi-position entry: POST /api/v2/trade/positions/{symbol}.
            // Flipster auto-allocates a fresh slot per call so stacking
            // works. reduce_only entries are physically nonsense (you
            // can only reduce an existing position) — fall through to
            // the one-way path for those, where slot=0 is fine.
            //
            // post_only is not exposed on this endpoint; it's a v1
            // limitation on the multi-position path. Pass-through hint
            // only — callers needing strict postOnly should set
            // multiple_positions=false.
            let _ = post_only;
            let f_side = if side.eq_ignore_ascii_case("long") {
                FSide::Long
            } else {
                FSide::Short
            };
            let f_margin = if self.margin.eq_ignore_ascii_case("isolated") {
                FMarginType::Isolated
            } else {
                FMarginType::Cross
            };
            let f_otype = match order_type {
                "ORDER_TYPE_LIMIT" => FOrderType::Limit,
                _ => FOrderType::Market,
            };
            let params = FOrderParams::builder()
                .side(f_side)
                .price(ref_price)
                .amount(amount_usd)
                .leverage(leverage)
                .margin_type(f_margin)
                .order_type(f_otype)
                .build();
            let resp = self.flipster.place_order(symbol, params).await?;
            return Ok(resp.raw);
        }
        self.flipster
            .place_order_oneway_with_margin(
                symbol, side, amount_usd, ref_price, leverage, reduce_only,
                order_type, post_only, &self.margin,
            )
            .await
    }

    pub fn dispatch(self: &Arc<Self>, ev: TradeSignal) {
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.handle_event(ev).await {
                tracing::warn!(error = %e, "[dispatch] error");
            }
        });
    }

    async fn handle_event(self: Arc<Self>, ev: TradeSignal) -> Result<()> {
        match ev.action.as_str() {
            "entry" => {
                if !self.seen_entries.insert((ev.base.clone(), ev.position_id)) {
                    return Ok(());
                }
                if !self.whitelist.is_empty() && !self.whitelist.contains(&ev.base) {
                    return Ok(());
                }
                if self.blacklist.contains(&ev.base) {
                    return Ok(());
                }
                // Drop entries whose signal age exceeds threshold — by
                // then Gate has typically drifted 30+ bp toward fair,
                // leaving no edge worth crossing the book for.
                let age_ms = signal_age_ms(&ev.timestamp);
                if age_ms > MAX_ENTRY_SIGNAL_AGE_MS {
                    tracing::info!(
                        "[ENTRY-DROP] {} | side={} | age={:.0}ms > {:.0}ms (queue stale, skip)",
                        ev.base, ev.side, age_ms, MAX_ENTRY_SIGNAL_AGE_MS
                    );
                    return Ok(());
                }
                self.on_entry(ev, age_ms).await
            }
            "exit" => {
                if !self.seen_exits.insert((ev.base.clone(), ev.position_id)) {
                    return Ok(());
                }
                self.on_exit(ev).await
            }
            other => {
                tracing::warn!(action = %other, "unknown action");
                Ok(())
            }
        }
    }

    async fn on_entry(self: Arc<Self>, ev: TradeSignal, signal_lag_ms: f64) -> Result<()> {
        let entry_t0 = Instant::now();
        let flipster_sym = format!("{}USDT.PERP", ev.base);
        let gate_sym = format!("{}_USDT", ev.base);
        // Per-symbol dynamic sizing. Owned by `coordinator::route_signal`
        // in Phase 3b — the size on the wire is already adjusted. Restore
        // executor-side adjustment only when EXEC_LEGACY_FILTERS=1.
        let dyn_size_enabled = std::env::var("GL_DYN_SIZE")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .map(|v| v != 0)
            .unwrap_or(true);
        let mut size = if dyn_size_enabled && legacy_filters_enabled() {
            self.sym_stats.adjusted_size(&ev.base, self.size_usd)
        } else {
            // Trust the coordinator's adjusted size (passed via the signal).
            ev.size_usd
        };

        tracing::info!(
            "[ENTRY] {} | side={} | ${:.0} | lag={:.0}ms | paper f={} g={} pos_id={}",
            ev.base, ev.side, size, signal_lag_ms,
            ev.flipster_price, ev.gate_price, ev.position_id,
        );

        if self.dry_run {
            tracing::info!("  [DRY RUN] skipping real orders");
            self.open_positions.insert(
                (ev.base.clone(), ev.position_id),
                LivePosition {
                    position_id: ev.position_id,
                    base: ev.base.clone(),
                    flipster_side: ev.side.clone(),
                    size_usd: size,
                    entry_time: ev.timestamp.clone(),
                    flipster_entry_price: 0.0,
                    flipster_size: 0.0,
                    flipster_slot: 0,
                    gate_entry_price: 0.0,
                    gate_size: 0,
                    gate_contract: gate_sym.clone(),
                    entry_spread_bp: 0.0,
                    entry_epoch: epoch_now(),
                    peak_captured_bp: 0.0,
                    f_entry_slip_bp: 0.0,
                    g_entry_slip_bp: 0.0,
                    signal_lag_ms_entry: signal_lag_ms,
                    paper_f_entry: ev.flipster_price,
                    paper_g_entry: ev.gate_price,
                    exit_order_id: None,
                    exit_limit_price: 0.0,
                },
            );
            return Ok(());
        }

        // BBO snapshot from the signal itself — no QuestDB round-trip.
        // Pairs of (bid, ask, spread_bp) for whichever venues the
        // strategy filled in. Any None below means the venue wasn't
        // attached to the signal; downstream order placement falls back
        // to the mid (`ev.flipster_price` / `ev.gate_price`).
        let current_bid_ask: Option<(f64, f64)> = match (ev.flipster_bid, ev.flipster_ask) {
            (Some(b), Some(a)) if b > 0.0 && a > 0.0 && a >= b => Some((b, a)),
            _ => None,
        };
        let current_spread_bp: Option<f64> = current_bid_ask.map(|(b, a)| {
            let m = 0.5 * (b + a);
            if m > 0.0 { (a - b) / m * 1e4 } else { 0.0 }
        });
        // Decision-rule filters (min_spread, edge_recheck) live in the
        // collector's coordinator now; the signal we just received has
        // already been gated on those. The executor only needs the BBO
        // for LIMIT pricing, which is below.

        // Decision filters (chop_detect, sym_stats.check_entry,
        // adjusted_size, min_spread, edge_recheck, gate slip cap) all
        // live in collector::coordinator now. Set EXEC_LEGACY_FILTERS=1
        // to restore the executor-side chop / dyn-filter chain as a
        // rollback knob (kept for one rollout cycle; may be removed once
        // coordinator parity is confirmed).
        if legacy_filters_enabled() {
            let now_inst = std::time::Instant::now();
            let entry = self.last_signal.get(&ev.base).map(|v| v.value().clone());
            if let Some((prev_side, prev_ts)) = entry {
                let age_s = now_inst.duration_since(prev_ts).as_secs_f64();
                if prev_side != ev.side && age_s < CHOP_DETECT_WINDOW_S {
                    tracing::info!(
                        base = %ev.base, prev_side = %prev_side, age_s = age_s,
                        "  [LEGACY] chop skip {}s",
                        CHOP_DETECT_WINDOW_S
                    );
                    self.last_signal
                        .insert(ev.base.clone(), (ev.side.clone(), now_inst));
                    return Ok(());
                }
            }
            self.last_signal
                .insert(ev.base.clone(), (ev.side.clone(), now_inst));

            use crate::symbol_stats::FilterDecision;
            match self.sym_stats.check_entry(&ev.base, current_spread_bp) {
                FilterDecision::Pass => {}
                FilterDecision::Probe => {
                    tracing::info!(base = %ev.base, "  [LEGACY] dyn-filter probe");
                }
                FilterDecision::Skip(reason) => {
                    tracing::info!(
                        base = %ev.base,
                        reason = %reason.as_str(),
                        "  [LEGACY] dyn-filter skip"
                    );
                    return Ok(());
                }
            }
        }

        // Match Flipster notional to Gate's discrete contract notional so
        // the hedge ratio is 1:1. Skipped in flipster_only mode (no Gate
        // hedge to size against).
        if !self.flipster_only {
            let spec = self
                .gate_contracts
                .get(&gate_sym)
                .copied()
                .unwrap_or(crate::gate_contracts::ContractSpec {
                    multiplier: 1.0,
                    order_size_min: 1,
                });
            let gate_contracts_pre =
                gate_contracts::size_from_usd(&self.gate_contracts, &gate_sym, size, ev.gate_price);
            let adjusted = (gate_contracts_pre as f64) * spec.multiplier * ev.gate_price;
            if adjusted < 3.0 || adjusted > size * 2.5 {
                tracing::info!(
                    "  [SKIP] hedge mismatch — gate notional=${:.2} vs target=${}",
                    adjusted, size
                );
                return Ok(());
            }
            size = adjusted;
        }

        // Flipster entry — LIMIT-IOC. Use the BBO carried on the signal:
        // collector publishes the freshest snapshot it observed. Falls
        // back to the signal mid if BBO is missing (legacy publisher).
        let f_limit_price = match current_bid_ask {
            Some((bid, ask)) => if ev.side == "long" { ask } else { bid },
            None => ev.flipster_price,
        };
        // Hybrid entry: big lead-exchange moves use MARKET (guaranteed
        // fill), smaller moves use LIMIT-IOC (price control). The lead
        // move size is approximated by the gap between gate_price (lead)
        // and flipster_price (laggard) at signal time — Binance has
        // already moved, Flipster hasn't followed yet, so the gap ≈ how
        // far we expect Flipster to catch up.
        let lead_move_bp = if ev.flipster_price > 0.0 && ev.gate_price > 0.0 {
            let gap = (ev.gate_price - ev.flipster_price) / ev.flipster_price * 1e4;
            // For long (Binance up): gap should be positive. For short: negative.
            // Always compare absolute size.
            gap.abs()
        } else {
            0.0
        };
        let market_threshold_bp: f64 = std::env::var("GL_MARKET_ENTRY_BP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30.0);
        // In multi-position mode the LIMIT-IOC helper isn't slot-aware
        // (it goes through the one-way endpoint). Until the helper is
        // ported, force MARKET entries so each one definitely lands on
        // its own slot via the multi-position endpoint.
        let use_market_entry = lead_move_bp > market_threshold_bp || self.multiple_positions;

        let f_max_lev = self.flip_contracts.max_leverage(&flipster_sym);
        let f_t0 = Instant::now();
        let f_result_opt = if use_market_entry {
            tracing::info!(
                "  [MARKET-ENTRY] big move (lead={:.1}bp > {}bp threshold) lev={}x",
                lead_move_bp, market_threshold_bp, f_max_lev
            );
            match self
                .flipster_place(
                    &flipster_sym,
                    &ev.side,
                    size,
                    f_limit_price,
                    f_max_lev,
                    false,
                    "ORDER_TYPE_MARKET",
                    false,
                )
                .await
            {
                Ok(r) => {
                    let (avg, sz, _) = flip_extract_fill(&r);
                    if avg > 0.0 && sz > 0.0 {
                        Some(r)
                    } else {
                        // Some odd path — fall back via WS check.
                        if self.flip_state.read().await.has_position(&flipster_sym, 0) {
                            Some(serde_json::json!({
                                "position": {
                                    "symbol": flipster_sym,
                                    "size": size / f_limit_price.max(1e-9),
                                    "avgPrice": f_limit_price,
                                    "slot": 0,
                                },
                                "order": r.get("order").cloned().unwrap_or(serde_json::Value::Null),
                            }))
                        } else {
                            None
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "  [MARKET-ENTRY] FAILED");
                    return Ok(());
                }
            }
        } else {
            match limit_ioc_entry(
                &self.flipster,
                &self.flip_state,
                &flipster_sym,
                &ev.side,
                size,
                f_limit_price,
                f_max_lev,
                FLIP_LIMIT_WAIT_S,
                &self.margin,
            )
            .await
            {
                Ok((Some(v), _)) => Some(v),
                Ok((None, _)) => None,
                Err(e) => {
                    tracing::warn!(error = %e, "  [FLIPSTER-LIMIT] FAILED");
                    return Ok(());
                }
            }
        };
        let f_dt_ms = f_t0.elapsed().as_secs_f64() * 1000.0;
        let Some(f_result) = f_result_opt else {
            tracing::info!(
                "  [FLIPSTER] not filled ({:.0}ms) — abort, no Gate, no revert",
                f_dt_ms
            );
            return Ok(());
        };
        let (f_pre_avg, f_pre_size, f_pre_slot) = flip_extract_fill(&f_result);
        let entry_path = if use_market_entry { "MARKET" } else { "LIMIT" };
        tracing::info!(
            "  [FLIPSTER-{}] OK ({:.0}ms) size=${:.2} fill={}",
            entry_path, f_dt_ms, size, f_pre_avg
        );

        // Single-leg mode: skip Gate placement entirely. Used for the
        // gate_lead strategy where the bet is directional Flipster
        // following Gate's already-emitted move.
        if self.flipster_only {
            let f_entry_slip = if f_pre_avg > 0.0 && ev.flipster_price > 0.0 {
                let raw = (f_pre_avg - ev.flipster_price) / ev.flipster_price * 1e4;
                if ev.side == "long" { raw } else { -raw }
            } else {
                0.0
            };
            tracing::info!(
                "  [SLIP-IN] flipster={:+.2}bp (gate skipped — flipster_only)",
                f_entry_slip
            );

            // Pre-place a maker-only EXIT LIMIT at our TP target, so when
            // Flipster catches up to the lead price we exit at exactly that
            // price (no slippage, possibly maker rebate). On 2s timeout
            // or stop, on_exit cancels this and falls back to MARKET.
            // Set GL_MAKER_EXIT=0 to disable.
            let maker_exit_enabled = std::env::var("GL_MAKER_EXIT")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .map(|v| v != 0)
                .unwrap_or(true);
            let mut exit_order_id: Option<String> = None;
            let mut exit_limit_price: f64 = 0.0;
            if maker_exit_enabled && f_pre_avg > 0.0 {
                // Tick-valid pricing: prefer the bid/ask we just read from
                // QuestDB (always populated for active symbols), fall back
                // to the WS state's view. Both come from Flipster itself
                // so the values are tick-aligned.
                //
                // Long exit = SELL: place at ask + (ask - bid).  Sits one
                // level above current ask.  Filled when buyers walk up.
                // Short exit = BUY:  place at bid - (ask - bid).
                let ba = if current_bid_ask.is_some() {
                    current_bid_ask
                } else {
                    self.flip_state.read().await.bid_ask(&flipster_sym, 0)
                };
                let exit_px = match ba {
                    Some((bid, ask)) => {
                        let step = (ask - bid).max(ask * 0.00005);
                        let raw = if ev.side == "long" {
                            ask + step
                        } else {
                            bid - step
                        };
                        // Cap to a sane TP envelope from entry to avoid
                        // ridiculous prices when bid/ask is stale.
                        const TP_MAX_BP: f64 = 25.0;
                        let entry_px = f_pre_avg;
                        let bound_max = if ev.side == "long" {
                            entry_px * (1.0 + TP_MAX_BP / 1e4)
                        } else {
                            entry_px * (1.0 - TP_MAX_BP / 1e4)
                        };
                        let bound_min = if ev.side == "long" {
                            entry_px
                        } else {
                            entry_px
                        };
                        if ev.side == "long" {
                            raw.clamp(bound_min, bound_max)
                        } else {
                            raw.clamp(bound_max, bound_min)
                        }
                    }
                    None => 0.0, // skip maker exit if no WS quote yet
                };
                if exit_px > 0.0 {
                    let exit_side = if ev.side == "long" { "short" } else { "long" };
                    let resp = self
                        .flipster_place(
                            &flipster_sym,
                            exit_side,
                            size,
                            exit_px,
                            f_max_lev,
                            true,
                            "ORDER_TYPE_LIMIT",
                            true,
                        )
                        .await;
                    match resp {
                        Ok(r) => {
                            let oid = r
                                .get("order")
                                .and_then(|o| o.get("orderId"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            if oid.is_some() {
                                tracing::info!(
                                    "  [MAKER-EXIT] placed @ {:.6} order_id={:?}",
                                    exit_px, oid
                                );
                                exit_limit_price = exit_px;
                                exit_order_id = oid;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "  [MAKER-EXIT] place failed (will fallback to MARKET on exit)");
                        }
                    }
                }
            }

            self.open_positions.insert(
                (ev.base.clone(), ev.position_id),
                LivePosition {
                    position_id: ev.position_id,
                    base: ev.base.clone(),
                    flipster_side: ev.side.clone(),
                    size_usd: size,
                    entry_time: ev.timestamp.clone(),
                    flipster_entry_price: f_pre_avg,
                    flipster_size: f_pre_size,
                    flipster_slot: f_pre_slot.unwrap_or(0),
                    gate_entry_price: 0.0,
                    gate_size: 0,
                    gate_contract: gate_sym.clone(),
                    entry_spread_bp: 0.0,
                    entry_epoch: epoch_now(),
                    peak_captured_bp: 0.0,
                    f_entry_slip_bp: f_entry_slip,
                    g_entry_slip_bp: 0.0,
                    signal_lag_ms_entry: signal_lag_ms,
                    paper_f_entry: ev.flipster_price,
                    paper_g_entry: ev.gate_price,
                    exit_order_id,
                    exit_limit_price,
                },
            );
            tracing::info!(
                "  [TRACK] flipster_entry={} size={} slot={} (single-leg)",
                f_pre_avg, f_pre_size, f_pre_slot.unwrap_or(0)
            );
            crate::fill_publisher::publish_fill(
                &self.variant,
                ev.position_id,
                &ev.base,
                "entry",
                &ev.side,
                size,
                f_pre_avg,
                0.0,
                f_entry_slip,
                0.0,
                ev.flipster_price,
                ev.gate_price,
                signal_lag_ms as i64,
                chrono::Utc::now(),
            );
            return Ok(());
        }

        // Gate hedge — IOC at the BBO carried on the signal. The
        // collector publishes the freshest snapshot; the slip cap that
        // used to live here is now part of the coordinator's filter
        // chain (or simply absent, since the signal price already
        // reflects publish-time market state).
        let g_side = if ev.side == "long" { GSide::Short } else { GSide::Long };
        let g_size = gate_contracts::size_from_usd(&self.gate_contracts, &gate_sym, size, ev.gate_price);
        let g_limit_price = match (ev.gate_bid, ev.gate_ask) {
            (Some(b), Some(a)) if b > 0.0 && a > 0.0 && a >= b => {
                if g_side == GSide::Short { b } else { a }
            }
            _ => ev.gate_price,
        };

        // Set leverage on first encounter of each contract.
        {
            let mut set = self.gate_leverage_set.lock().await;
            if !set.contains(&gate_sym) {
                match self.gate.set_leverage(&gate_sym, 0, self.gate_leverage).await {
                    Ok(v) => {
                        let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("");
                        tracing::info!(
                            "  [LEV] {} → cross {}x: {}",
                            gate_sym, self.gate_leverage, msg
                        );
                        set.insert(gate_sym.clone());
                    }
                    Err(e) => tracing::warn!(error = %e, "  [LEV] set_leverage failed"),
                }
            }
        }

        let g_t0 = Instant::now();
        let g_result = match self
            .gate
            .place_order(
                &gate_sym,
                GParams {
                    side: g_side,
                    size: g_size,
                    order_type: GType::Limit,
                    price: Some(g_limit_price),
                    tif: GTif::Ioc,
                    reduce_only: false,
                    text: None,
                },
            )
            .await
        {
            Ok(r) if r.ok => r,
            Ok(r) => {
                tracing::warn!(
                    "  [GATE] not ok: status={} msg={} label={}",
                    r.status, r.message(), r.label()
                );
                self.safety_revert_and_log(&ev, &flipster_sym, &gate_sym, &f_result, size, signal_lag_ms).await;
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(error = %e, "  [GATE] place_order err");
                self.safety_revert_and_log(&ev, &flipster_sym, &gate_sym, &f_result, size, signal_lag_ms).await;
                return Ok(());
            }
        };

        // Final fill resolution. IOC: extract directly. (POC/GTC paths
        // intentionally not ported — they were retired in the Python.)
        let (g_fill_price, g_filled_size) = gate_extract_fill(&g_result);
        let _order_id = extract_order_id(&g_result);
        let _ = extract_status(&g_result);
        if g_fill_price <= 0.0 || g_filled_size <= 0 {
            tracing::warn!("  [GATE-IOC] not filled");
            self.safety_revert_and_log(&ev, &flipster_sym, &gate_sym, &f_result, size, signal_lag_ms).await;
            return Ok(());
        }
        let g_dt_ms = g_t0.elapsed().as_secs_f64() * 1000.0;
        let total_dt_ms = entry_t0.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(
            "  [GATE-IOC] filled={} @{} ({:.0}ms) | total_entry={:.0}ms sig→done={:.0}ms",
            g_filled_size, g_fill_price, g_dt_ms, total_dt_ms, signal_lag_ms + total_dt_ms
        );

        // Track the position.
        let (f_avg, f_size, f_slot) = flip_extract_fill(&f_result);
        let entry_sprd_bp = if f_avg > 0.0 && g_fill_price > 0.0 {
            (f_avg - g_fill_price) / g_fill_price * 1e4
        } else {
            0.0
        };
        let f_entry_slip = if f_avg > 0.0 && ev.flipster_price > 0.0 {
            let raw = (f_avg - ev.flipster_price) / ev.flipster_price * 1e4;
            if ev.side == "long" { raw } else { -raw }
        } else {
            0.0
        };
        let g_entry_slip = if g_fill_price > 0.0 && ev.gate_price > 0.0 {
            let raw = (g_fill_price - ev.gate_price) / ev.gate_price * 1e4;
            if ev.side == "long" { -raw } else { raw }
        } else {
            0.0
        };
        tracing::info!(
            "  [SLIP-IN] flipster={:+.2}bp gate={:+.2}bp",
            f_entry_slip, g_entry_slip
        );
        self.open_positions.insert(
            (ev.base.clone(), ev.position_id),
            LivePosition {
                position_id: ev.position_id,
                base: ev.base.clone(),
                flipster_side: ev.side.clone(),
                size_usd: size,
                entry_time: ev.timestamp.clone(),
                flipster_entry_price: f_avg,
                flipster_size: f_size,
                flipster_slot: f_slot.unwrap_or(0),
                gate_entry_price: g_fill_price,
                gate_size: if g_filled_size > 0 { g_filled_size } else { g_size },
                gate_contract: gate_sym.clone(),
                entry_spread_bp: entry_sprd_bp,
                entry_epoch: epoch_now(),
                peak_captured_bp: 0.0,
                f_entry_slip_bp: f_entry_slip,
                g_entry_slip_bp: g_entry_slip,
                signal_lag_ms_entry: signal_lag_ms,
                paper_f_entry: ev.flipster_price,
                paper_g_entry: ev.gate_price,
                exit_order_id: None,
                exit_limit_price: 0.0,
            },
        );
        tracing::info!(
            "  [TRACK] flipster_entry={} size={} slot={} | gate_entry={} contracts={} | entry_sprd={:+.1}bp",
            f_avg, f_size, f_slot.unwrap_or(0), g_fill_price, g_filled_size, entry_sprd_bp
        );
        crate::fill_publisher::publish_fill(
            &self.variant,
            ev.position_id,
            &ev.base,
            "entry",
            &ev.side,
            size,
            f_avg,
            g_fill_price,
            f_entry_slip,
            g_entry_slip,
            ev.flipster_price,
            ev.gate_price,
            signal_lag_ms as i64,
            chrono::Utc::now(),
        );
        Ok(())
    }

    async fn safety_revert_and_log(
        self: &Arc<Self>,
        ev: &TradeSignal,
        flipster_sym: &str,
        gate_sym: &str,
        f_result: &serde_json::Value,
        size: f64,
        _signal_lag_ms: f64,
    ) {
        tracing::info!("  [SAFETY] Undoing Flipster side to stay flat...");
        let (f_open_avg, _, _) = flip_extract_fill(f_result);
        let close_side = if ev.side == "long" { "short" } else { "long" };
        let mut f_close_avg = 0.0;
        let lev = self.flip_contracts.max_leverage(flipster_sym);
        match self
            .flipster_place(
                flipster_sym,
                close_side,
                size * 1.5,
                ev.flipster_price,
                lev,
                true,
                "ORDER_TYPE_MARKET",
                false,
            )
            .await
        {
            Ok(rev) => {
                let (avg, _, _) = flip_extract_fill(&rev);
                f_close_avg = avg;
                tracing::info!("  [SAFETY] Flipster reverted close={}", f_close_avg);
            }
            Err(e) => tracing::error!(
                error = %e,
                symbol = %flipster_sym,
                "  [SAFETY] !!! revert failed — MANUAL INTERVENTION NEEDED !!!"
            ),
        }

        // Record SAFETY revert PnL — open at LIMIT, close at MARKET.
        let f_pnl_usd = if f_open_avg > 0.0 && f_close_avg > 0.0 {
            let dir = if ev.side == "long" { 1.0 } else { -1.0 };
            let f_pnl_bp = (f_close_avg - f_open_avg) / f_open_avg * 1e4 * dir;
            f_pnl_bp * size / 1e4
        } else {
            0.0
        };
        let approx_fee = size * TAKER_FEE_FRAC * 2.0;
        let net = f_pnl_usd - approx_fee;
        let rec = TradeRecord {
            ts_close: now_iso(),
            exit_reason: "safety_revert".to_string(),
            entry_spread_bp: None,
            peak_captured_bp: None,
            f_entry_slip_bp: None,
            g_entry_slip_bp: None,
            f_exit_slip_bp: None,
            g_exit_slip_bp: None,
            signal_lag_ms: None,
            paper_f_entry: None,
            paper_g_entry: None,
            paper_f_exit: None,
            paper_g_exit: None,
            ts_entry: ev.timestamp.clone(),
            pos_id: ev.position_id,
            base: ev.base.clone(),
            flipster_side: ev.side.clone(),
            size_usd: size,
            flipster_entry: f_open_avg,
            flipster_exit: f_close_avg,
            gate_entry: 0.0,
            gate_exit: 0.0,
            gate_size: 0,
            gate_contract: gate_sym.to_string(),
            f_pnl_usd: round6(f_pnl_usd),
            g_pnl_usd: 0.0,
            net_pnl_usd: round6(f_pnl_usd),
            approx_fee_usd: round6(approx_fee),
            net_after_fees_usd: round6(net),
        };
        if let Err(e) = self.trade_log.append(&rec).await {
            tracing::warn!(error = %e, "  [SAFETY-PNL] log err");
        }
        self.stats.add(net);
        tracing::info!(
            "  [SAFETY-PNL] f_pnl=${:+.4} fee=${:.4} net=${:+.4}",
            f_pnl_usd, approx_fee, net
        );
        self.stats.print();
    }

    async fn on_exit(self: Arc<Self>, ev: TradeSignal) -> Result<()> {
        let pos = self
            .open_positions
            .remove(&(ev.base.clone(), ev.position_id))
            .map(|(_, v)| v);
        tracing::info!(
            "[EXIT] {} | side={} | pos_id={} | paper f={} g={}",
            ev.base, ev.side, ev.position_id, ev.flipster_price, ev.gate_price
        );

        let Some(pos) = pos else {
            tracing::info!("  (position not tracked — skipped or from before startup)");
            return Ok(());
        };
        if self.dry_run {
            tracing::info!("  [DRY RUN] skipping real close");
            self.stats.add(0.0);
            return Ok(());
        }

        let flipster_sym = format!("{}USDT.PERP", ev.base);
        let gate_sym = format!("{}_USDT", ev.base);

        let close_side = if ev.side == "long" { "short" } else { "long" };
        let ref_px = if ev.flipster_price > 0.0 {
            ev.flipster_price
        } else {
            pos.flipster_entry_price
        };
        let mut f_close_avg = 0.0;

        // If we pre-placed a maker exit LIMIT, try cancelling first. The
        // cancel response tells us whether it was already filled.
        let mut maker_filled = false;
        if let Some(oid) = pos.exit_order_id.as_ref() {
            match self.flipster.cancel_order(&flipster_sym, oid).await {
                Ok(_) => {
                    // Cancel succeeded → LIMIT was still open, not filled.
                    tracing::info!("  [MAKER-EXIT] cancelled (will MARKET close)");
                }
                Err(e) => {
                    let msg = format!("{e}");
                    // "Order not found" / "already filled" / similar errors
                    // mean the LIMIT was filled before we cancelled.
                    if msg.contains("ORDER_NOT_FOUND")
                        || msg.contains("AlreadyFilled")
                        || msg.contains("already filled")
                        || msg.contains("OrderNotFound")
                        || msg.contains("404")
                    {
                        maker_filled = true;
                        f_close_avg = pos.exit_limit_price;
                        tracing::info!(
                            "  [MAKER-EXIT] FILLED @ {:.6} (slip=0)",
                            pos.exit_limit_price
                        );
                    } else {
                        tracing::warn!(error = %e, "  [MAKER-EXIT] cancel err");
                    }
                }
            }
            // Brief pause so WS state catches up to cancel/fill.
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }

        // Confirm via WS state — if position is now zero, we don't need to
        // MARKET close (catches the cancel-said-not-found-but-actually
        // filled race). Otherwise MARKET reduceOnly.
        let ws_size = self
            .flip_state
            .read()
            .await
            .position_size(&flipster_sym, pos.flipster_slot)
            .abs();
        if !maker_filled && ws_size == 0.0 && pos.exit_order_id.is_some() {
            // WS confirms position closed — maker LIMIT actually filled
            // even though cancel call returned non-error. Trust WS.
            maker_filled = true;
            f_close_avg = pos.exit_limit_price;
            tracing::info!(
                "  [MAKER-EXIT] WS shows pos=0 → assume filled @ {:.6}",
                pos.exit_limit_price
            );
        }

        if !maker_filled {
            let lev = self.flip_contracts.max_leverage(&flipster_sym);
            match self
                .flipster_place(
                    &flipster_sym,
                    close_side,
                    pos.size_usd * 1.5,
                    ref_px,
                    lev,
                    true,
                    "ORDER_TYPE_MARKET",
                    false,
                )
                .await
            {
                Ok(r) => {
                    let (avg, _, _) = flip_extract_fill(&r);
                    f_close_avg = avg;
                    tracing::info!("  [FLIPSTER CLOSE] OK @ {}", f_close_avg);
                }
                Err(e) => tracing::error!(
                    error = %e,
                    "  [FLIPSTER CLOSE] FAILED — MANUAL CLOSE may be needed"
                ),
            }
        }

        // Gate close: reduce-only IOC opposite direction. Skipped in
        // single-leg mode (gate_lead never opens a Gate leg).
        let close_size_signed = if ev.side == "long" { pos.gate_size } else { -pos.gate_size };
        let mut g_close_avg = 0.0;
        if !self.flipster_only && pos.gate_size != 0 {
            match self
                .gate
                .close_position(&gate_sym, close_size_signed, None)
                .await
            {
                Ok(r) => {
                    let (avg, _) = gate_extract_fill(&r);
                    g_close_avg = avg;
                    tracing::info!(
                        "  [GATE CLOSE] ok={} label={} size={} fill={}",
                        r.ok, r.label(), close_size_signed, g_close_avg
                    );
                }
                Err(e) => tracing::error!(error = %e, "  [GATE CLOSE] FAILED"),
            }
        }

        // PnL.
        let f_dir = if ev.side == "long" { 1.0 } else { -1.0 };
        let f_pnl_bp = if pos.flipster_entry_price > 0.0 && f_close_avg > 0.0 {
            (f_close_avg - pos.flipster_entry_price) / pos.flipster_entry_price * 1e4 * f_dir
        } else {
            0.0
        };
        let f_pnl_usd = f_pnl_bp * pos.size_usd / 1e4;
        let g_dir = if ev.side == "long" { -1.0 } else { 1.0 };
        let spec = self
            .gate_contracts
            .get(&pos.gate_contract)
            .copied()
            .unwrap_or(crate::gate_contracts::ContractSpec {
                multiplier: 1.0,
                order_size_min: 1,
            });
        let g_notional = (pos.gate_size as f64) * spec.multiplier * pos.gate_entry_price;
        let g_pnl_bp = if pos.gate_entry_price > 0.0 && g_close_avg > 0.0 {
            (g_close_avg - pos.gate_entry_price) / pos.gate_entry_price * 1e4 * g_dir
        } else {
            0.0
        };
        let g_pnl_usd = g_pnl_bp * g_notional / 1e4;
        let net_pnl_usd = f_pnl_usd + g_pnl_usd;
        let approx_fee = (pos.size_usd + g_notional) * TAKER_FEE_FRAC * 2.0;
        let net_after_fees = net_pnl_usd - approx_fee;

        // Slippage at exit.
        let f_exit_slip = if f_close_avg > 0.0 && ev.flipster_price > 0.0 {
            let raw = (f_close_avg - ev.flipster_price) / ev.flipster_price * 1e4;
            if ev.side == "long" { -raw } else { raw }
        } else {
            0.0
        };
        let g_exit_slip = if g_close_avg > 0.0 && ev.gate_price > 0.0 {
            let raw = (g_close_avg - ev.gate_price) / ev.gate_price * 1e4;
            if ev.side == "long" { raw } else { -raw }
        } else {
            0.0
        };

        self.stats.add(net_after_fees);
        tracing::info!(
            "  [PNL] flipster=${:+.4} gate=${:+.4} net=${:+.4} (-fees ${:.4}) → ${:+.4}",
            f_pnl_usd, g_pnl_usd, net_pnl_usd, approx_fee, net_after_fees
        );
        self.stats.print();

        // Update dynamic filter + sizing stats with this trade's metrics.
        let pnl_bp_for_dyn = if pos.size_usd > 0.0 {
            net_after_fees / pos.size_usd * 1e4
        } else {
            0.0
        };
        let paper_bp_for_dyn = if pos.paper_f_entry > 0.0 && ev.flipster_price > 0.0 {
            let raw = (ev.flipster_price - pos.paper_f_entry) / pos.paper_f_entry * 1e4;
            if ev.side == "long" { raw } else { -raw }
        } else {
            0.0
        };
        // Total slippage = entry slip + exit slip (both in our direction
        // expressed as cost, i.e. positive = unfavorable).
        let slip_bp_total = pos.f_entry_slip_bp.max(0.0) + f_exit_slip.max(0.0);
        self.sym_stats.record_trade(
            &pos.base,
            pnl_bp_for_dyn,
            paper_bp_for_dyn,
            slip_bp_total,
        );

        let exit_reason = match ev.timestamp.as_str() {
            "shutdown" => "shutdown".to_string(),
            "take_profit" => "take_profit".to_string(),
            _ => "paper_signal".to_string(),
        };
        let ts_close_iso = if matches!(ev.timestamp.as_str(), "shutdown" | "take_profit") {
            now_iso()
        } else {
            ev.timestamp.clone()
        };

        let rec = TradeRecord {
            ts_close: ts_close_iso,
            exit_reason,
            entry_spread_bp: Some(round2(pos.entry_spread_bp)),
            peak_captured_bp: Some(round2(pos.peak_captured_bp)),
            f_entry_slip_bp: Some(round2(pos.f_entry_slip_bp)),
            g_entry_slip_bp: Some(round2(pos.g_entry_slip_bp)),
            f_exit_slip_bp: Some(round2(f_exit_slip)),
            g_exit_slip_bp: Some(round2(g_exit_slip)),
            signal_lag_ms: Some(pos.signal_lag_ms_entry.round()),
            paper_f_entry: Some(pos.paper_f_entry),
            paper_g_entry: Some(pos.paper_g_entry),
            paper_f_exit: Some(ev.flipster_price),
            paper_g_exit: Some(ev.gate_price),
            ts_entry: pos.entry_time.clone(),
            pos_id: pos.position_id,
            base: pos.base.clone(),
            flipster_side: pos.flipster_side.clone(),
            size_usd: pos.size_usd,
            flipster_entry: pos.flipster_entry_price,
            flipster_exit: f_close_avg,
            gate_entry: pos.gate_entry_price,
            gate_exit: g_close_avg,
            gate_size: pos.gate_size,
            gate_contract: pos.gate_contract.clone(),
            f_pnl_usd: round6(f_pnl_usd),
            g_pnl_usd: round6(g_pnl_usd),
            net_pnl_usd: round6(net_pnl_usd),
            approx_fee_usd: round6(approx_fee),
            net_after_fees_usd: round6(net_after_fees),
        };
        if let Err(e) = self.trade_log.append(&rec).await {
            tracing::warn!(error = %e, "  [TRACK] log write failed");
        }
        crate::fill_publisher::publish_fill(
            &self.variant,
            pos.position_id,
            &pos.base,
            "exit",
            &pos.flipster_side,
            pos.size_usd,
            f_close_avg,
            g_close_avg,
            f_exit_slip,
            g_exit_slip,
            ev.flipster_price,
            ev.gate_price,
            0,
            chrono::Utc::now(),
        );
        Ok(())
    }

    /// Sweep tracked positions whose age exceeds `threshold_s` and force-close
    /// them. Catches orphans from the race where collector emits EXIT before
    /// executor's LIMIT fill confirms — by the time we register TRACK, the
    /// matching exit signal is already in the past and lost. Without this
    /// sweeper, those positions stay open forever.
    pub async fn sweep_stale_positions(self: Arc<Self>, threshold_s: f64) {
        let now = epoch_now();
        let total = self.open_positions.len();
        let stale_keys: Vec<(String, i64)> = self
            .open_positions
            .iter()
            .filter(|kv| now - kv.value().entry_epoch > threshold_s)
            .map(|kv| kv.key().clone())
            .collect();
        if !stale_keys.is_empty() {
            tracing::info!(
                tracked = total,
                stale = stale_keys.len(),
                "[SWEEP-STALE] found stale positions"
            );
        }
        for key in stale_keys {
            if let Some((_, pos)) = self.open_positions.remove(&key) {
                let exec = self.clone();
                tokio::spawn(async move { exec.force_close(pos).await });
            }
        }
    }

    /// Cancel any open orders we see in WS state that have been open for
    /// longer than `threshold_s`. Catches stuck entry LIMITs whose cancel
    /// API call failed silently. Throttled to 1/tick like the position
    /// sweeper to stay under CF 1015.
    pub async fn sweep_stale_orders(self: Arc<Self>, threshold_s: f64) {
        let ws_orders = self.flip_state.read().await.iter_open_orders();
        if ws_orders.is_empty() {
            self.order_first_seen.clear();
            return;
        }
        let now = std::time::Instant::now();
        let mut still_open: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut acted = false;
        for (oid, sym) in ws_orders {
            still_open.insert(oid.clone());
            let age_s = {
                let first = self
                    .order_first_seen
                    .entry(oid.clone())
                    .or_insert(now);
                now.duration_since(*first.value()).as_secs_f64()
            };
            if age_s < threshold_s || acted {
                continue;
            }
            acted = true;
            self.order_first_seen.remove(&oid);
            tracing::warn!(
                order_id = %oid, symbol = %sym, age_s = age_s,
                "[STALE-ORDER] cancelling"
            );
            let flipster = self.flipster.clone();
            let oid_c = oid.clone();
            let sym_c = sym.clone();
            tokio::spawn(async move {
                match flipster.cancel_order(&sym_c, &oid_c).await {
                    Ok(_) => tracing::info!(
                        order_id = %oid_c, "[STALE-ORDER] cancel OK"
                    ),
                    Err(e) => tracing::warn!(
                        error = %e, order_id = %oid_c,
                        "[STALE-ORDER] cancel err"
                    ),
                }
            });
        }
        self.order_first_seen.retain(|k, _| still_open.contains(k));
    }

    /// Sweep positions/orders that exist on Flipster (per WS state) but
    /// have no matching tracked entry. Two cases:
    ///   1. size != 0 but untracked → real orphan position, MARKET reduceOnly.
    ///   2. size == 0 but margin > 0 → stuck open order reserving margin,
    ///      cancel any open orders for the symbol.
    /// Throttled: only one action per call to avoid CF 1015 rate-limit
    /// bursts when many orphans accumulate at once.
    pub async fn sweep_abort_orphans(self: Arc<Self>, threshold_s: f64) {
        let (ws_positions, ws_orders) = {
            let s = self.flip_state.read().await;
            (s.iter_open_positions(), s.iter_open_orders())
        };
        if !ws_positions.is_empty() || !ws_orders.is_empty() {
            tracing::debug!(
                ws_pos = ws_positions.len(),
                ws_ord = ws_orders.len(),
                "[ORPHAN-SWEEP] tick"
            );
        }
        if ws_positions.is_empty() {
            self.unmatched_first_seen.clear();
            return;
        }
        let tracked_pos: std::collections::HashSet<(String, i8)> = self
            .open_positions
            .iter()
            .map(|kv| {
                let p = kv.value();
                let sym = format!("{}USDT.PERP", p.base);
                let sign = if p.flipster_side == "long" { 1i8 } else { -1i8 };
                (sym, sign)
            })
            .collect();
        let tracked_syms: std::collections::HashSet<String> = self
            .open_positions
            .iter()
            .map(|kv| format!("{}USDT.PERP", kv.value().base))
            .collect();

        let now_inst = std::time::Instant::now();
        let mut still_unmatched: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut acted_this_tick = false; // throttle: one action per tick
        for (sym, slot, signed_size, margin, mid_px) in ws_positions {
            let key = format!("{sym}/{slot}");
            let sign: i8 = if signed_size >= 0.0 { 1 } else { -1 };
            if signed_size.abs() > 0.0 && tracked_pos.contains(&(sym.clone(), sign)) {
                continue;
            }
            if signed_size.abs() == 0.0 && margin > 0.0 && tracked_syms.contains(&sym) {
                continue;
            }

            still_unmatched.insert(key.clone());
            let age_s = {
                let first = self
                    .unmatched_first_seen
                    .entry(key.clone())
                    .or_insert(now_inst);
                now_inst.duration_since(*first.value()).as_secs_f64()
            }; // RefMut dropped here
            tracing::debug!(
                key = %key, age_s = age_s, threshold = threshold_s,
                size = signed_size, margin = margin,
                "[ORPHAN-SWEEP] unmatched"
            );
            if age_s < threshold_s {
                continue;
            }
            if acted_this_tick {
                // Defer to next sweep tick (500ms later) to avoid CF 1015.
                continue;
            }
            acted_this_tick = true;
            self.unmatched_first_seen.remove(&key);
            let age = age_s;

            if signed_size.abs() > 0.0 {
                // Real position with no tracking → reduceOnly MARKET close.
                let close_side = if sign > 0 { "short" } else { "long" };
                let abs_size = signed_size.abs();
                tracing::warn!(
                    symbol = %sym, slot = slot, size = signed_size, age_s = age,
                    "[ORPHAN-SWEEP] untracked position — force closing"
                );
                let flipster = self.flipster.clone();
                let sym_clone = sym.clone();
                let side_clone = close_side.to_string();
                let margin_mode = self.margin.clone();
                // Notional in USD = size * mid_px. Pass 1.5x as overshoot;
                // reduceOnly caps at current position. Without a sane
                // reference price Flipster rejects MARKET with InvalidPrice.
                let notional_usd = abs_size * mid_px * 1.5;
                let ref_px = mid_px;
                let lev = self.flip_contracts.max_leverage(&sym);
                tokio::spawn(async move {
                    match flipster
                        .place_order_oneway_with_margin(
                            &sym_clone, &side_clone, notional_usd, ref_px, lev,
                            true, "ORDER_TYPE_MARKET", false, &margin_mode,
                        )
                        .await
                    {
                        Ok(_) => tracing::info!(symbol = %sym_clone, "[ORPHAN-SWEEP] OK"),
                        Err(e) => tracing::error!(
                            error = %e, symbol = %sym_clone,
                            "[ORPHAN-SWEEP] FAILED"
                        ),
                    }
                });
            } else if margin > 0.0 {
                // size=0 but margin reserved → stuck open order. Cancel any
                // open orders matching this symbol.
                let matching_orders: Vec<(String, String)> = ws_orders
                    .iter()
                    .filter(|(_, osym)| osym == &sym)
                    .cloned()
                    .collect();
                if matching_orders.is_empty() {
                    tracing::warn!(
                        symbol = %sym, margin = margin, age_s = age,
                        "[ORPHAN-SWEEP] margin reserved with no order match — \
                         WS state may be stale (not actionable)"
                    );
                    continue;
                }
                tracing::warn!(
                    symbol = %sym, margin = margin, age_s = age,
                    n_orders = matching_orders.len(),
                    "[ORPHAN-SWEEP] stuck order reserving margin — canceling"
                );
                for (oid, osym) in matching_orders {
                    let flipster = self.flipster.clone();
                    tokio::spawn(async move {
                        match flipster.cancel_order(&osym, &oid).await {
                            Ok(_) => tracing::info!(
                                order_id = %oid, symbol = %osym,
                                "[ORPHAN-SWEEP] cancel OK"
                            ),
                            Err(e) => tracing::warn!(
                                error = %e, order_id = %oid, symbol = %osym,
                                "[ORPHAN-SWEEP] cancel err"
                            ),
                        }
                    });
                }
            }
        }
        self.unmatched_first_seen
            .retain(|k, _| still_unmatched.contains(k));
    }

    async fn force_close(self: Arc<Self>, pos: LivePosition) {
        let age_s = epoch_now() - pos.entry_epoch;
        tracing::warn!(
            position_id = pos.position_id,
            base = %pos.base,
            side = %pos.flipster_side,
            age_s = age_s,
            "[FORCE-CLOSE] stale position — sweeper closing"
        );
        let flipster_sym = format!("{}USDT.PERP", pos.base);
        // Cancel maker exit LIMIT first if present, to avoid double-close.
        if let Some(oid) = pos.exit_order_id.as_ref() {
            let _ = self.flipster.cancel_order(&flipster_sym, oid).await;
        }
        let close_side = if pos.flipster_side == "long" {
            "short"
        } else {
            "long"
        };
        let ref_px = pos.flipster_entry_price;
        let lev = self.flip_contracts.max_leverage(&flipster_sym);
        match self
            .flipster
            .place_order_oneway_with_margin(
                &flipster_sym,
                close_side,
                pos.size_usd * 1.5,
                ref_px,
                lev,
                true,
                "ORDER_TYPE_MARKET",
                false,
                &self.margin,
            )
            .await
        {
            Ok(r) => {
                let (avg, _, _) = flip_extract_fill(&r);
                tracing::info!(
                    position_id = pos.position_id,
                    base = %pos.base,
                    close_avg = avg,
                    "[FORCE-CLOSE] OK"
                );
            }
            Err(e) => tracing::error!(
                error = %e,
                position_id = pos.position_id,
                base = %pos.base,
                "[FORCE-CLOSE] FAILED — manual intervention may be needed"
            ),
        }
        if !self.flipster_only && pos.gate_size != 0 {
            let close_size_signed = if pos.flipster_side == "long" {
                pos.gate_size
            } else {
                -pos.gate_size
            };
            if let Err(e) = self
                .gate
                .close_position(&pos.gate_contract, close_size_signed, None)
                .await
            {
                tracing::error!(error = %e, "[FORCE-CLOSE GATE] FAILED");
            }
        }
    }
}

fn signal_age_ms(ts: &str) -> f64 {
    let parsed: Option<DateTime<Utc>> = ts
        .replace('Z', "+00:00")
        .parse::<DateTime<chrono::FixedOffset>>()
        .ok()
        .map(|d| d.with_timezone(&Utc));
    let Some(t) = parsed else { return 0.0 };
    (Utc::now() - t).num_milliseconds() as f64
}

fn now_iso() -> String {
    Utc::now()
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn epoch_now() -> f64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    d.as_secs_f64()
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

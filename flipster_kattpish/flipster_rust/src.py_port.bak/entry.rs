// Entry flow — submit maker LIMIT @ web top-of-book, poll for fill,
// cancel on cross_gone / converge. Mirrors Python _open_position.

use crate::data_cache::DataCache;
use crate::exec_log::ExecLogger;
use crate::order_manager::{FlipsterOrderManager, OrderRequest, OrderType};
use crate::params::StrategyParams;
use crate::position_state::{OpenPosition, PositionManager};
use crate::signal::SignalResult;
use crate::symbols::{self, SymbolRegistry};
use crate::types::Side;
use anyhow::Result;
use chrono::Utc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

pub enum EntryOutcome {
    Filled(OpenPosition),
    /// Cancelled (order did not fill) — reason string for logging.
    Cancelled(&'static str),
    /// Order submission failed.
    SubmitFailed(String),
}

pub async fn open_position(
    om: &FlipsterOrderManager,
    pos_mgr: &PositionManager,
    cache: &DataCache,
    reg: &SymbolRegistry,
    exec_log: Option<&Arc<ExecLogger>>,
    sym: &str,
    sig: &SignalResult,
    params: &StrategyParams,
) -> EntryOutcome {
    let side = match sig.side {
        Some(s) => s,
        None => return EntryOutcome::Cancelled("no_side"),
    };
    let raw_price = match sig.maker_price {
        Some(p) => p,
        None => return EntryOutcome::Cancelled("no_maker_price"),
    };
    let api_tp = sig.api_tp_target.unwrap_or(0.0);

    pos_mgr.record_trade_attempt(Utc::now().timestamp_millis());

    let tick = reg.tick(sym);
    let limit_price = symbols::tick_round(
        tick,
        raw_price,
        if side == Side::Buy { "down" } else { "up" },
    )
    .unwrap_or(raw_price);
    let leverage = reg.max_leverage(sym);

    // Snapshot entry_mid_api for logging (use api mid at submit time).
    let entry_mid_api = cache
        .get_api_bookticker(sym)
        .map(|b| b.mid())
        .unwrap_or(raw_price);

    let req = OrderRequest {
        symbol: sym.to_string(),
        side,
        price: limit_price,
        amount_usd: params.amount_usd,
        order_type: OrderType::Limit,
        post_only: true,
        reduce_only: false,
        leverage,
    };

    let submit_ms = Utc::now().timestamp_millis();
    let resp = match om.submit_order(&req).await {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            warn!("[ORDER_ERR] {} {}: {}", sym, side.as_str(), truncate(&msg, 120));
            return EntryOutcome::SubmitFailed(msg);
        }
    };

    // Snapshot web best for exec_log
    let (web_bid, web_ask) = cache
        .get_web_bookticker(sym)
        .map(|b| (b.bid_price, b.ask_price))
        .unwrap_or((0.0, 0.0));
    let (api_bid, api_ask) = cache
        .get_api_bookticker(sym)
        .map(|b| (b.bid_price, b.ask_price))
        .unwrap_or((0.0, 0.0));

    // Immediate fill (rare — post_only accepted & the price matched on arrival).
    if resp.filled_size > 0.0 {
        let fill = resp.avg_fill;
        let slip_bp = compute_slip_bp(side, limit_price, fill);
        let pos = build_position(sym, side, params.amount_usd, entry_mid_api, fill, submit_ms, api_tp, reg);
        info!(
            "[OPEN:MAKER_STALE] {} {} avg={:.6} tp={:.6} (cross={:.1}bp)  open={}",
            sym, side.as_str(), pos.entry_fill_price, pos.tp_target_price, sig.cross_bp,
            pos_mgr.num_open() + 1,
        );
        pos_mgr.insert(pos.clone());
        pos_mgr.opens.fetch_add(1, Ordering::Relaxed);
        if let Some(log) = exec_log {
            log.open_event(
                sym, side.as_str(), "MAKER_STALE",
                limit_price, fill, slip_bp,
                api_bid, api_ask, web_bid, web_ask,
                true,
            );
        }
        return EntryOutcome::Filled(pos);
    }

    // Poll for fill via private WS or cancel on edge gone.
    let order_id = match resp.order_id.clone() {
        Some(oid) => oid,
        None => {
            warn!("[ORDER_ERR] {} no orderId in response", sym);
            return EntryOutcome::SubmitFailed("no_order_id".into());
        }
    };
    let deadline = submit_ms + params.entry_timeout_ms;
    let poll_interval = Duration::from_millis(params.entry_poll_ms as u64);
    let mut cancel_reason: &'static str = "timeout";
    let mut filled_via_tracker: Option<(f64, f64)> = None;

    while Utc::now().timestamp_millis() < deadline {
        tokio::time::sleep(poll_interval).await;

        // (a) Check private WS tracker for fill on this symbol.
        if let Some((tr_side, tr_qty, tr_entry)) = om.state().position_side_qty_entry(sym) {
            let want_long = side == Side::Buy;
            let tr_long = tr_side == "Long";
            if want_long == tr_long && tr_qty > 0.0 && tr_entry > 0.0 {
                filled_via_tracker = Some((tr_entry, tr_qty));
                info!("[LATE_FILL] {} {} @{:.6} qty={:.6}", sym, side.as_str(), tr_entry, tr_qty);
                break;
            }
        }

        // (b) Check api-web gap convergence → cancel.
        let api = match cache.get_api_bookticker(sym) {
            Some(b) => b,
            None => continue,
        };
        let web = match cache.get_web_bookticker(sym) {
            Some(b) => b,
            None => continue,
        };
        if api.mid() <= 0.0 || web.mid() <= 0.0 {
            continue;
        }
        let diff_bp = (api.mid() - web.mid()) / web.mid() * 1.0e4;

        if side == Side::Buy {
            // cross gone: api_bid no longer above web_ask
            if api.bid_price <= web.ask_price {
                cancel_reason = "cross_gone";
                break;
            }
            if diff_bp < params.entry_converge_bp {
                cancel_reason = "converged";
                break;
            }
        } else {
            if api.ask_price >= web.bid_price {
                cancel_reason = "cross_gone";
                break;
            }
            if diff_bp > -params.entry_converge_bp {
                cancel_reason = "converged";
                break;
            }
        }
    }

    if let Some((entry_price, _qty)) = filled_via_tracker {
        let pos = build_position(
            sym, side, params.amount_usd, entry_mid_api,
            entry_price, submit_ms, api_tp, reg,
        );
        pos_mgr.insert(pos.clone());
        pos_mgr.opens.fetch_add(1, Ordering::Relaxed);
        let slip_bp = compute_slip_bp(side, limit_price, entry_price);
        if let Some(log) = exec_log {
            log.open_event(
                sym, side.as_str(), "MAKER_STALE",
                limit_price, entry_price, slip_bp,
                api_bid, api_ask, web_bid, web_ask,
                true,
            );
        }
        return EntryOutcome::Filled(pos);
    }

    // Cancel the resting order.
    if let Err(e) = om.cancel_order(sym, &order_id).await {
        warn!("[CANCEL_ERR] {} {}: {}", sym, order_id, truncate(&e.to_string(), 100));
    }
    let age_ms = Utc::now().timestamp_millis() - submit_ms;
    info!(
        "[ENTRY_CANCEL:{}] {} {} after {}ms",
        cancel_reason, sym, side.as_str(), age_ms
    );
    EntryOutcome::Cancelled(cancel_reason)
}

fn compute_slip_bp(side: Side, intended: f64, actual: f64) -> f64 {
    if intended <= 0.0 {
        return 0.0;
    }
    let raw = (actual - intended) / intended * 1.0e4;
    match side {
        Side::Buy => raw,   // positive = paid more than intended (adverse)
        Side::Sell => -raw, // positive = received less than intended (adverse)
    }
}

fn build_position(
    sym: &str,
    side: Side,
    size_usd: f64,
    entry_mid_api: f64,
    entry_fill: f64,
    entry_time_ms: i64,
    api_tp: f64,
    reg: &SymbolRegistry,
) -> OpenPosition {
    let tick = reg.tick(sym);
    let tp_dir = if side == Side::Buy { "up" } else { "down" };
    let tp_target_price = symbols::tick_round(tick, api_tp, tp_dir).unwrap_or(api_tp);
    OpenPosition {
        sym: sym.to_string(),
        side,
        size_usd,
        entry_mid_api,
        entry_fill_price: entry_fill,
        entry_time_ms,
        tp_target_price,
        tp_order_id: None,
        stop_confirm_count: 0,
    }
}

/// Place post-only reduce-only TP bracket at pos.tp_target_price.
pub async fn place_tp_bracket(
    om: &FlipsterOrderManager,
    pos_mgr: &PositionManager,
    pos: &OpenPosition,
    reg: &SymbolRegistry,
) {
    if pos.tp_target_price <= 0.0 {
        return;
    }
    let opp_side = if pos.side == Side::Buy { Side::Sell } else { Side::Buy };
    let leverage = reg.max_leverage(&pos.sym);
    let req = OrderRequest {
        symbol: pos.sym.clone(),
        side: opp_side,
        price: pos.tp_target_price,
        // Double size_usd to ensure the reduce_only bracket can close the full position
        // (Python _submit_one_way_order passes pos.size_usd * 2 for bracket).
        amount_usd: pos.size_usd * 2.0,
        order_type: OrderType::Limit,
        post_only: true,
        reduce_only: true,
        leverage,
    };
    match om.submit_order(&req).await {
        Ok(resp) => {
            if let Some(oid) = resp.order_id {
                pos_mgr.update_tp_order_id(&pos.sym, oid.clone());
                info!(
                    "[TP_BRACKET] {} {} LIMIT @ {:.6} id={}",
                    pos.sym,
                    pos.side.as_str(),
                    pos.tp_target_price,
                    short_id(&oid)
                );
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("PostOnly") || msg.to_lowercase().contains("would cross") {
                info!(
                    "[TP_BRACKET_CROSS] {}: already at/past TP — skipping bracket",
                    pos.sym
                );
            } else {
                warn!("[TP_BRACKET_ERR] {}: {}", pos.sym, truncate(&msg, 120));
            }
        }
    }
}

fn short_id(s: &str) -> &str {
    if s.len() >= 8 { &s[..8] } else { s }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

// Arc-friendly shim
pub async fn open_position_arc(
    om: Arc<FlipsterOrderManager>,
    pos_mgr: Arc<PositionManager>,
    cache: Arc<DataCache>,
    reg: Arc<SymbolRegistry>,
    exec_log: Option<Arc<ExecLogger>>,
    sym: String,
    sig: SignalResult,
    params: StrategyParams,
) -> Result<EntryOutcome> {
    let outcome =
        open_position(&om, &pos_mgr, &cache, &reg, exec_log.as_ref(), &sym, &sig, &params).await;
    if let EntryOutcome::Filled(pos) = &outcome {
        place_tp_bracket(&om, &pos_mgr, pos, &reg).await;
    }
    Ok(outcome)
}

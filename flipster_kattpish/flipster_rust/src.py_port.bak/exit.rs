// Exit logic — periodic monitor that evaluates each open position and triggers close.
// Mirrors Python exit_monitor + _close_position.
//
// Exit triggers (priority order):
//   1. BRACKET_TP — private WS shows position vanished (exchange filled our TP).
//   2. hard_stop  — adverse >= HARD_STOP_BP → MARKET immediately.
//   3. stop       — adverse >= STOP_BP for STOP_CONFIRM_TICKS polls → MAKER 1s → MARKET.
//   4. tp_fallback — profit >= TP_FALLBACK_MIN_BP AND age >= TP_FALLBACK_AGE_MS
//                    → MAKER 3s → MARKET.
//   5. max_hold   — age >= MAX_HOLD_MS → MAKER 1s → MARKET.

use crate::data_cache::DataCache;
use crate::exec_log::ExecLogger;
use crate::order_manager::{FlipsterOrderManager, OrderRequest, OrderType};
use crate::params::StrategyParams;
use crate::position_state::{OpenPosition, PositionManager};
use crate::symbols::{self, SymbolRegistry};
use crate::types::Side;
use chrono::Utc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq)]
enum ExitKind {
    HardStop,
    Stop,
    TpFallback,
    MaxHold,
}

impl ExitKind {
    fn as_str(&self) -> &'static str {
        match self {
            ExitKind::HardStop => "hard_stop",
            ExitKind::Stop => "stop",
            ExitKind::TpFallback => "tp_fallback",
            ExitKind::MaxHold => "max_hold",
        }
    }

    fn maker_timeout_ms(&self, params: &StrategyParams) -> Option<i64> {
        match self {
            ExitKind::HardStop => None,
            ExitKind::TpFallback => Some(params.maker_close_timeout_tp_ms),
            ExitKind::Stop | ExitKind::MaxHold => Some(params.maker_close_timeout_stop_ms),
        }
    }
}

pub fn spawn_exit_monitor(
    om: Arc<FlipsterOrderManager>,
    pos_mgr: Arc<PositionManager>,
    cache: Arc<DataCache>,
    reg: Arc<SymbolRegistry>,
    exec_log: Option<Arc<ExecLogger>>,
    params: StrategyParams,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(200));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let open = pos_mgr.all();
            if open.is_empty() {
                continue;
            }
            for pos in open {
                let om = om.clone();
                let pos_mgr = pos_mgr.clone();
                let cache = cache.clone();
                let reg = reg.clone();
                let exec_log = exec_log.clone();
                let params = params.clone();
                tokio::spawn(async move {
                    evaluate_and_maybe_close(&om, &pos_mgr, &cache, &reg, exec_log.as_ref(), &params, pos).await;
                });
            }
        }
    });
}

async fn evaluate_and_maybe_close(
    om: &FlipsterOrderManager,
    pos_mgr: &PositionManager,
    cache: &DataCache,
    reg: &SymbolRegistry,
    exec_log: Option<&Arc<ExecLogger>>,
    params: &StrategyParams,
    pos: OpenPosition,
) {
    // Re-check position still exists locally (tokio spawn delay races).
    if !pos_mgr.is_open(&pos.sym) {
        return;
    }
    let now_ms = Utc::now().timestamp_millis();
    let age_ms = pos.age_ms(now_ms);

    // 1. BRACKET_TP — only if we placed a bracket AND private WS shows no position.
    if pos.tp_order_id.is_some() && age_ms > 2_000 && !om.state().has_position(&pos.sym) {
        let tp_move_bp =
            side_signed_move_bp(pos.side, pos.entry_fill_price, pos.tp_target_price);
        let pnl_usd = pos.size_usd * tp_move_bp / 1.0e4;
        let cum = pos_mgr.record_pnl(pnl_usd);
        pos_mgr.closes.fetch_add(1, Ordering::Relaxed);
        info!(
            "[CLOSE:BRACKET_TP] {} {} tp={:.6} move={:+.2}bp hold={}ms pnl=${:+.4} cum=${:+.4}",
            pos.sym, pos.side.as_str(), pos.tp_target_price, tp_move_bp, age_ms, pnl_usd, cum,
        );
        if let Some(log) = exec_log {
            log.close_event(
                &pos.sym, pos.side.as_str(), "MAKER", "bracket_tp",
                pos.entry_fill_price, pos.tp_target_price,
                tp_move_bp, age_ms, pnl_usd, cum,
            );
        }
        pos_mgr.remove(&pos.sym);
        return;
    }

    let api = match cache.get_api_bookticker(&pos.sym) {
        Some(b) => b,
        None => return,
    };
    if api.mid() <= 0.0 {
        return;
    }
    let exit_side_price = match pos.side {
        Side::Buy => api.bid_price,
        Side::Sell => api.ask_price,
    };
    let move_bp = side_signed_move_bp(pos.side, pos.entry_fill_price, exit_side_price);

    if move_bp <= -params.hard_stop_bp {
        close_position(om, pos_mgr, cache, reg, exec_log, params, &pos, ExitKind::HardStop, move_bp, age_ms).await;
        return;
    }
    if age_ms >= params.min_hold_ms_for_stop && move_bp <= -params.stop_bp {
        let confirm = pos_mgr.bump_stop_confirm(&pos.sym);
        if confirm as i32 >= params.stop_confirm_ticks {
            close_position(om, pos_mgr, cache, reg, exec_log, params, &pos, ExitKind::Stop, move_bp, age_ms).await;
            return;
        }
    } else {
        pos_mgr.reset_stop_confirm(&pos.sym);
    }
    if age_ms >= params.tp_fallback_age_ms
        && move_bp >= params.tp_fallback_min_bp
        && age_ms < params.max_hold_ms
    {
        close_position(om, pos_mgr, cache, reg, exec_log, params, &pos, ExitKind::TpFallback, move_bp, age_ms).await;
        return;
    }
    if age_ms >= params.max_hold_ms {
        close_position(om, pos_mgr, cache, reg, exec_log, params, &pos, ExitKind::MaxHold, move_bp, age_ms).await;
    }
}

async fn close_position(
    om: &FlipsterOrderManager,
    pos_mgr: &PositionManager,
    cache: &DataCache,
    reg: &SymbolRegistry,
    exec_log: Option<&Arc<ExecLogger>>,
    params: &StrategyParams,
    pos: &OpenPosition,
    kind: ExitKind,
    decision_move_bp: f64,
    decision_age_ms: i64,
) {
    // Cancel TP bracket first (avoid double-close race).
    if let Some(oid) = pos.tp_order_id.clone() {
        if let Err(e) = om.cancel_order(&pos.sym, &oid).await {
            tracing::debug!("[tp_cancel] {}: {}", pos.sym, truncate(&e.to_string(), 80));
        }
    }

    let opp_side = if pos.side == Side::Buy { Side::Sell } else { Side::Buy };
    let leverage = reg.max_leverage(&pos.sym);
    let mut maker_filled = false;
    let mut exit_price = 0.0_f64;

    if let Some(wait_ms) = kind.maker_timeout_ms(params) {
        if let Some((price, filled)) =
            try_maker_close(om, cache, reg, pos, opp_side, leverage, wait_ms).await
        {
            if filled {
                maker_filled = true;
                exit_price = price;
            }
        }
    }

    if !maker_filled {
        // MARKET fallback.
        let req = OrderRequest {
            symbol: pos.sym.clone(),
            side: opp_side,
            price: pos.entry_mid_api.max(0.0), // advisory
            amount_usd: pos.size_usd * 2.0,
            order_type: OrderType::Market,
            post_only: false,
            reduce_only: true,
            leverage,
        };
        match om.submit_order(&req).await {
            Ok(resp) => {
                if resp.avg_fill > 0.0 {
                    exit_price = resp.avg_fill;
                }
            }
            Err(e) => {
                warn!(
                    "[MARKET_CLOSE_ERR] {}: {}",
                    pos.sym,
                    truncate(&e.to_string(), 120)
                );
            }
        }
        if exit_price <= 0.0 {
            // Use current opp-side price as estimate for log.
            exit_price = opp_side_price_now(cache, &pos.sym, opp_side).unwrap_or(0.0);
        }
    }

    let actual_move_bp = if exit_price > 0.0 {
        side_signed_move_bp(pos.side, pos.entry_fill_price, exit_price)
    } else {
        decision_move_bp
    };
    let pnl_usd = pos.size_usd * actual_move_bp / 1.0e4;
    let cum = pos_mgr.record_pnl(pnl_usd);
    pos_mgr.closes.fetch_add(1, Ordering::Relaxed);
    let close_kind = if maker_filled { "MAKER" } else { "TAKER" };
    info!(
        "[CLOSE:{}] {} {} reason={}({:+.1}bp,{}s)  entry={:.6} exit={:.6} move={:+.2}bp hold={}ms pnl=${:+.4} cum=${:+.4}",
        close_kind, pos.sym, pos.side.as_str(), kind.as_str(),
        decision_move_bp, decision_age_ms / 1000,
        pos.entry_fill_price, exit_price, actual_move_bp, decision_age_ms, pnl_usd, cum,
    );
    if let Some(log) = exec_log {
        log.close_event(
            &pos.sym, pos.side.as_str(), close_kind, kind.as_str(),
            pos.entry_fill_price, exit_price, actual_move_bp, decision_age_ms, pnl_usd, cum,
        );
    }
    pos_mgr.remove(&pos.sym);

    if actual_move_bp <= -params.stop_bp {
        let until = Utc::now().timestamp_millis() + params.blacklist_ms;
        pos_mgr.blacklist(&pos.sym, until);
    }
}

/// Submit post_only LIMIT close at web's opp-side price, poll private WS for position
/// to disappear within wait_ms. Returns (exit_price_estimate, filled?).
async fn try_maker_close(
    om: &FlipsterOrderManager,
    cache: &DataCache,
    reg: &SymbolRegistry,
    pos: &OpenPosition,
    opp_side: Side,
    leverage: i32,
    wait_ms: i64,
) -> Option<(f64, bool)> {
    // Close long (SELL) posts at web_ask; close short (BUY) posts at web_bid.
    let web = cache.get_web_bookticker(&pos.sym)?;
    let raw_price = match opp_side {
        Side::Sell => web.ask_price,
        Side::Buy => web.bid_price,
    };
    if raw_price <= 0.0 {
        return None;
    }
    let tick = reg.tick(&pos.sym);
    // For post_only SELL at web_ask: round UP (non-crossing).
    // For post_only BUY at web_bid: round DOWN.
    let dir = if opp_side == Side::Sell { "up" } else { "down" };
    let limit_price = symbols::tick_round(tick, raw_price, dir).unwrap_or(raw_price);

    let req = OrderRequest {
        symbol: pos.sym.clone(),
        side: opp_side,
        price: limit_price,
        amount_usd: pos.size_usd * 2.0,
        order_type: OrderType::Limit,
        post_only: true,
        reduce_only: true,
        leverage,
    };
    let resp = match om.submit_order(&req).await {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("PostOnly") || msg.to_lowercase().contains("would cross") {
                return Some((limit_price, false)); // caller falls back to MARKET
            }
            warn!("[MAKER_CLOSE_ERR] {}: {}", pos.sym, truncate(&msg, 120));
            return None;
        }
    };
    // Instant cross-fill returned in response.
    if resp.filled_size > 0.0 && resp.avg_fill > 0.0 {
        return Some((resp.avg_fill, true));
    }
    let order_id = resp.order_id.clone().unwrap_or_default();

    // Poll private WS for position disappearance (= our close filled OR TP bracket filled).
    let deadline = Utc::now().timestamp_millis() + wait_ms;
    while Utc::now().timestamp_millis() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if !om.state().has_position(&pos.sym) {
            return Some((limit_price, true));
        }
    }
    // Timeout → cancel the resting maker close.
    if !order_id.is_empty() {
        let _ = om.cancel_order(&pos.sym, &order_id).await;
    }
    Some((limit_price, false))
}

fn opp_side_price_now(cache: &DataCache, sym: &str, opp_side: Side) -> Option<f64> {
    let api = cache.get_api_bookticker(sym)?;
    Some(match opp_side {
        Side::Sell => api.bid_price, // we're selling, we get bid
        Side::Buy => api.ask_price,  // we're buying, we pay ask
    })
}

fn side_signed_move_bp(side: Side, from: f64, to: f64) -> f64 {
    if from <= 0.0 {
        return 0.0;
    }
    let raw = (to - from) / from * 1.0e4;
    match side {
        Side::Buy => raw,
        Side::Sell => -raw,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

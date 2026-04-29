//! Reusable decide_order logic for V8-style strategies (v8_0, v8_0_1).
//! Call with `use_few_orders_closing_trigger: true` for v8_0_1 (shortcut), `false` for v8_0.

use crate::config::TradeSettings;
use crate::data_cache::SymbolBooktickerSnapshot;
use crate::gate_order_manager::GateOrderManager;
use crate::market_watcher::MarketGapState;
use crate::order_decision::OrderDecision;
use crate::signal_calculator::SignalResult;

use log::warn;

const EPS: f64 = 1e-12;

/// V8-style decide_order. When `use_few_orders_closing_trigger` is true, uses
/// `few_orders_for_account_or_symbol && is_closing_something => 1` for triggers; otherwise uses formula only.
#[allow(clippy::too_many_arguments)]
pub fn decide_order(
    symbol: &str,
    signal: &SignalResult,
    trade_settings: &TradeSettings,
    current_time_ms: i64,
    gate_bt_server_time: i64,
    binance_bt_server_time: Option<i64>,
    gate_web_bt_server_time: i64,
    funding_rate: f64,
    order_count_size: f64,
    size_trigger: i64,
    max_size_trigger: i64,
    close_size_trigger: i64,
    contract_order_size: i64,
    usdt_position_size: f64,
    avg_spread: Option<f64>,
    avg_mid_gap: Option<f64>,
    total_net_positions_usdt_size: f64,
    previous_snapshot_1s: Option<&SymbolBooktickerSnapshot>,
    previous_snapshot_5s: Option<&SymbolBooktickerSnapshot>,
    previous_snapshot_10s: Option<&SymbolBooktickerSnapshot>,
    previous_snapshot_20s: Option<&SymbolBooktickerSnapshot>,
    gate_order_manager: &GateOrderManager,
    // Full market gap state (primary window + 1m Redis series); use e.g. `market_state.get_gap_1m_gate_vs_quote(symbol)`.
    #[allow(unused_variables)] market_state: &MarketGapState,
    use_few_orders_closing_trigger: bool,
    ignore_net_position_size_check: bool,
) -> Option<OrderDecision> {
    if !signal.has_signal() {
        return None;
    }
    // let is_gate_web_bookticker_in_between_gate_bookticker = (signal.gate_web_bid
    //     >= signal.gate_bid - EPS)
    //     && (signal.gate_web_ask <= signal.gate_ask + EPS);
    // if !is_gate_web_bookticker_in_between_gate_bookticker {
    //     return None;
    // }

    let mut memo = None;
    let _current_time_sec = current_time_ms / 1000_i64;
    let order_side = signal.order_side.as_deref()?;
    let mut order_price = if order_side == "buy" {
        signal.gate_web_ask
    } else if order_side == "sell" {
        signal.gate_web_bid
    } else {
        return None;
    };

    if signal.order_price.is_some() {
        if signal.order_price.unwrap() != order_price {
            warn!(
                "order_price mismatch: {} != {}",
                signal.order_price.unwrap(),
                order_price
            );
            return None;
        }
    }
    let gate_bt_received_at_age = current_time_ms - signal.snapshot.gate_bt_received_at_ms;
    let binance_bt_received_at_age = current_time_ms - signal.snapshot.base_bt_received_at_ms;

    if binance_bt_received_at_age < 3 {
        return None;
    }

    let _contract = gate_order_manager.get_contract(symbol);

    let position = gate_order_manager.get_position(symbol);
    let mutated_contract_order_size = contract_order_size;
    let mut binance_data_is_updated = true;

    let is_symbol_too_many_orders = gate_order_manager.is_symbol_too_many_orders(
        symbol,
        30,
        trade_settings.too_many_orders_time_gap_ms * 2,
    );

    let should_careful_for_too_many_orders_based_on_time_gap = gate_order_manager
        .is_too_many_orders(trade_settings.too_many_orders_time_gap_ms * 2)
        || is_symbol_too_many_orders;

    let gap_1m_gate_vs_quote = market_state.get_gap_1m_gate_vs_quote(symbol); // gate - quote mid gap
    let gap_1m_gate_web_vs_quote = market_state.get_gap_1m_gate_web_vs_quote(symbol); // gate_web - quote mid gap
    let spread_1m_gate_bookticker = market_state.get_spread_1m_gate_bookticker(symbol);
    let spread_1m_gate_webbookticker = market_state.get_spread_1m_gate_webbookticker(symbol);
    let gap_5m_gate_web_vs_quote = market_state.get_gap_5m_gate_web_vs_quote(symbol);
    let spread_5m_gate_webbookticker = market_state.get_spread_5m_gate_webbookticker(symbol);
    let gap_60m_gate_web_vs_quote = market_state.get_gap_60m_gate_web_vs_quote(symbol);
    let spread_60m_gate_webbookticker = market_state.get_spread_60m_gate_webbookticker(symbol);
    let gap_240m_gate_web_vs_quote = market_state.get_gap_240m_gate_web_vs_quote(symbol);
    let spread_240m_gate_webbookticker = market_state.get_spread_240m_gate_webbookticker(symbol);
    let gap_720m_gate_web_vs_quote = market_state.get_gap_720m_gate_web_vs_quote(symbol);
    let spread_720m_gate_webbookticker = market_state.get_spread_720m_gate_webbookticker(symbol);

    let binance_data_age = if let Some(binance_bt_server_time) = binance_bt_server_time {
        current_time_ms - binance_bt_server_time
    } else {
        i64::MAX
    };
    let gate_data_age = current_time_ms - signal.snapshot.gate_bt_received_at_ms;

    let current_spread_bp = signal.spread_bp;
    let current_spread_webbookticker_bp = (signal.gate_web_ask - signal.gate_web_bid)
        / (signal.gate_web_ask + signal.gate_web_bid)
        * 10000.0;

    let mut close_available_based_on_1m = true;
    let mut open_available_based_on_1m = true;
    let mut should_try_close_based_on_1m = false;
    let binance_spread_bp = if let (Some(binance_mid), Some(binance_ask), Some(binance_bid)) =
        (signal.binance_mid, signal.binance_ask, signal.binance_bid)
    {
        (binance_ask - binance_bid) / binance_mid * 10000.0
    } else {
        0.0
    };

    if let (Some(spread_1m_gate_webbookticker), Some(gap_1m_gate_web_vs_quote), Some(binance_mid)) = (
        spread_1m_gate_webbookticker,
        gap_1m_gate_web_vs_quote,
        signal.binance_mid,
    ) {
        let spread_1m_gate_webbookticker_bp = spread_1m_gate_webbookticker * 10000.0;
        let gap_1m_gate_web_vs_quote_bp = gap_1m_gate_web_vs_quote * 10000.0;
        let current_order_gap_bp = (order_price - binance_mid) / binance_mid * 10000.0;

        let max_spread_bp = current_spread_bp
            .max(current_spread_webbookticker_bp)
            .max(spread_1m_gate_webbookticker_bp);

        if order_side == "buy" {
            let chance_bp_1m = -current_order_gap_bp + gap_1m_gate_web_vs_quote_bp;
            if chance_bp_1m
                - (spread_1m_gate_webbookticker_bp / 3.0).max(trade_settings.mid_gap_bp_threshold)
                < 0.0
            {
                open_available_based_on_1m = false;
            }
            if chance_bp_1m - trade_settings.mid_gap_bp_threshold < 0.0 {
                close_available_based_on_1m = false;
            }
            let chance_bp_1m_strict = -current_order_gap_bp + gap_1m_gate_web_vs_quote_bp
                - max_spread_bp
                - binance_spread_bp;
            if chance_bp_1m_strict > trade_settings.close_based_on_1m_chance_bp_threshold + EPS
                && binance_data_age < 100
            {
                should_try_close_based_on_1m = true;
            }
        } else if order_side == "sell" {
            let chance_bp_1m = current_order_gap_bp - gap_1m_gate_web_vs_quote_bp;
            if chance_bp_1m
                - (spread_1m_gate_webbookticker_bp / 3.0).max(trade_settings.mid_gap_bp_threshold)
                < 0.0
            {
                open_available_based_on_1m = false;
            }
            if chance_bp_1m - trade_settings.mid_gap_bp_threshold < 0.0 {
                close_available_based_on_1m = false;
            }
            let chance_bp_1m_strict = current_order_gap_bp
                - gap_1m_gate_web_vs_quote_bp
                - max_spread_bp
                - binance_spread_bp;
            if chance_bp_1m_strict > trade_settings.close_based_on_1m_chance_bp_threshold + EPS
                && binance_data_age < 100
            {
                should_try_close_based_on_1m = true;
            }
        } else {
            open_available_based_on_1m = false;
        }
    }

    let mut open_available_based_on_5m = true;
    let mut close_available_based_on_5m = true;

    if let (Some(gap_5m_gate_web_vs_quote), Some(binance_mid)) =
        (gap_5m_gate_web_vs_quote, signal.binance_mid)
    {
        let gap_5m_gate_web_vs_quote_bp = gap_5m_gate_web_vs_quote * 10000.0;
        let current_order_gap_bp = (order_price - binance_mid) / binance_mid * 10000.0;
        let ref_spread_bp = spread_5m_gate_webbookticker
            .map(|s| s * 10000.0)
            .or_else(|| spread_1m_gate_webbookticker.map(|s| s * 10000.0))
            .unwrap_or(current_spread_webbookticker_bp);

        if order_side == "buy" {
            let chance_bp_5m = -current_order_gap_bp + gap_5m_gate_web_vs_quote_bp;
            if chance_bp_5m - (ref_spread_bp / 3.0).max(trade_settings.mid_gap_bp_threshold) < 0.0 {
                open_available_based_on_5m = false;
            }
            if chance_bp_5m - trade_settings.mid_gap_bp_threshold < 0.0 {
                close_available_based_on_5m = false;
            }
        } else if order_side == "sell" {
            let chance_bp_5m = current_order_gap_bp - gap_5m_gate_web_vs_quote_bp;
            if chance_bp_5m - (ref_spread_bp / 3.0).max(trade_settings.mid_gap_bp_threshold) < 0.0 {
                open_available_based_on_5m = false;
            }
            if chance_bp_5m - trade_settings.mid_gap_bp_threshold < 0.0 {
                close_available_based_on_5m = false;
            }
        } else {
            open_available_based_on_5m = false;
        }
    }

    let mut open_available_based_on_60m = true;
    let mut close_available_based_on_60m = true;
    let mut should_try_close_based_on_60m = false;

    if let (Some(gap_60m_gate_web_vs_quote), Some(binance_mid)) =
        (gap_60m_gate_web_vs_quote, signal.binance_mid)
    {
        let gap_60m_gate_web_vs_quote_bp = gap_60m_gate_web_vs_quote * 10000.0;
        let current_order_gap_bp = (order_price - binance_mid) / binance_mid * 10000.0;
        let ref_spread_bp = spread_60m_gate_webbookticker
            .map(|s| s * 10000.0)
            .or_else(|| spread_5m_gate_webbookticker.map(|s| s * 10000.0))
            .or_else(|| spread_1m_gate_webbookticker.map(|s| s * 10000.0))
            .unwrap_or(current_spread_webbookticker_bp);

        let max_spread_bp = current_spread_bp
            .max(current_spread_webbookticker_bp)
            .max(ref_spread_bp);

        if order_side == "buy" {
            let chance_bp_60m = -current_order_gap_bp + gap_60m_gate_web_vs_quote_bp;
            if chance_bp_60m - (ref_spread_bp / 3.0).max(trade_settings.mid_gap_bp_threshold) < 0.0
            {
                open_available_based_on_60m = false;
            }
            if chance_bp_60m - trade_settings.mid_gap_bp_threshold < 0.0 {
                close_available_based_on_60m = false;
            }

            let chance_bp_60m_strict = -current_order_gap_bp + gap_60m_gate_web_vs_quote_bp
                - max_spread_bp / 2.0
                - binance_spread_bp / 2.0;
            if chance_bp_60m_strict > trade_settings.mid_gap_bp_threshold + EPS
                && binance_data_age < 100
            {
                should_try_close_based_on_60m = true;
            }
        } else if order_side == "sell" {
            let chance_bp_60m = current_order_gap_bp - gap_60m_gate_web_vs_quote_bp;
            if chance_bp_60m - (ref_spread_bp / 3.0).max(trade_settings.mid_gap_bp_threshold) < 0.0
            {
                open_available_based_on_60m = false;
            }
            if chance_bp_60m - trade_settings.mid_gap_bp_threshold < 0.0 {
                close_available_based_on_60m = false;
            }
            let chance_bp_60m_strict = current_order_gap_bp
                - gap_60m_gate_web_vs_quote_bp
                - max_spread_bp / 2.0
                - binance_spread_bp / 2.0;
            if chance_bp_60m_strict > trade_settings.mid_gap_bp_threshold + EPS
                && binance_data_age < 100
            {
                should_try_close_based_on_60m = true;
            }
        } else {
            open_available_based_on_60m = false;
        }
    }

    let mut should_try_close_based_on_240m = false;
    let mut open_available_based_on_240m = true;
    let mut close_available_based_on_240m = true;
    if let (Some(gap_240m_gate_web_vs_quote), Some(binance_mid)) =
        (gap_240m_gate_web_vs_quote, signal.binance_mid)
    {
        let gap_240m_gate_web_vs_quote_bp = gap_240m_gate_web_vs_quote * 10000.0;
        let current_order_gap_bp = (order_price - binance_mid) / binance_mid * 10000.0;
        let ref_spread_bp = spread_240m_gate_webbookticker
            .map(|s| s * 10000.0)
            .or_else(|| spread_5m_gate_webbookticker.map(|s| s * 10000.0))
            .or_else(|| spread_1m_gate_webbookticker.map(|s| s * 10000.0))
            .unwrap_or(current_spread_webbookticker_bp);

        let max_spread_bp = current_spread_bp
            .max(current_spread_webbookticker_bp)
            .max(ref_spread_bp);

        if order_side == "buy" {
            let chance_bp_240m = -current_order_gap_bp + gap_240m_gate_web_vs_quote_bp;
            if chance_bp_240m - (ref_spread_bp / 2.0).max(trade_settings.mid_gap_bp_threshold) < 0.0
            {
                open_available_based_on_240m = false;
            }
            if chance_bp_240m < 0.0 {
                close_available_based_on_240m = false;
            }
            let chance_bp_240m_strict = -current_order_gap_bp + gap_240m_gate_web_vs_quote_bp
                - max_spread_bp / 2.0
                - binance_spread_bp / 2.0;
            if chance_bp_240m_strict > trade_settings.mid_gap_bp_threshold + EPS
            // && binance_data_age > 30
            {
                should_try_close_based_on_240m = true;
            }
        } else if order_side == "sell" {
            let chance_bp_240m = current_order_gap_bp - gap_240m_gate_web_vs_quote_bp;
            if chance_bp_240m - (ref_spread_bp / 2.0).max(trade_settings.mid_gap_bp_threshold) < 0.0
            {
                open_available_based_on_240m = false;
            }
            if chance_bp_240m < 0.0 {
                close_available_based_on_240m = false;
            }
            let chance_bp_240m_strict = current_order_gap_bp
                - gap_240m_gate_web_vs_quote_bp
                - max_spread_bp / 2.0
                - binance_spread_bp / 2.0;
            if chance_bp_240m_strict > trade_settings.mid_gap_bp_threshold + EPS
            // && binance_data_age > 30
            {
                should_try_close_based_on_240m = true;
            }
        } else {
            open_available_based_on_240m = false;
        }
    } else {
        open_available_based_on_240m = false;
        close_available_based_on_240m = false;
    }

    let mut open_available_based_on_720m = true;
    let mut close_available_based_on_720m = true;
    let mut should_try_close_based_on_720m = false;

    if let (Some(gap_720m_gate_web_vs_quote), Some(binance_mid)) =
        (gap_720m_gate_web_vs_quote, signal.binance_mid)
    {
        let gap_720m_gate_web_vs_quote_bp = gap_720m_gate_web_vs_quote * 10000.0;
        let current_order_gap_bp = (order_price - binance_mid) / binance_mid * 10000.0;
        let ref_spread_bp = spread_720m_gate_webbookticker
            .map(|s| s * 10000.0)
            .or_else(|| spread_240m_gate_webbookticker.map(|s| s * 10000.0))
            .or_else(|| spread_5m_gate_webbookticker.map(|s| s * 10000.0))
            .or_else(|| spread_1m_gate_webbookticker.map(|s| s * 10000.0))
            .unwrap_or(current_spread_webbookticker_bp);

        let max_spread_bp = current_spread_bp
            .max(current_spread_webbookticker_bp)
            .max(ref_spread_bp);

        if order_side == "buy" {
            let chance_bp_720m = -current_order_gap_bp + gap_720m_gate_web_vs_quote_bp;
            if chance_bp_720m - (ref_spread_bp / 2.0).max(trade_settings.mid_gap_bp_threshold) < 0.0
            {
                open_available_based_on_720m = false;
            }
            if chance_bp_720m < 0.0 {
                close_available_based_on_720m = false;
            }
            let chance_bp_720m_strict = -current_order_gap_bp + gap_720m_gate_web_vs_quote_bp
                - max_spread_bp / 1.5
                - binance_spread_bp / 2.0;
            if chance_bp_720m_strict > trade_settings.mid_gap_bp_threshold + EPS
            // && binance_data_age > 100
            {
                should_try_close_based_on_720m = true;
            }
        } else if order_side == "sell" {
            let chance_bp_720m = current_order_gap_bp - gap_720m_gate_web_vs_quote_bp;
            if chance_bp_720m - (ref_spread_bp / 2.0).max(trade_settings.mid_gap_bp_threshold) < 0.0
            {
                open_available_based_on_720m = false;
            }
            if chance_bp_720m < 0.0 {
                close_available_based_on_720m = false;
            }
            let chance_bp_720m_strict = current_order_gap_bp
                - gap_720m_gate_web_vs_quote_bp
                - max_spread_bp / 1.5
                - binance_spread_bp / 2.0;
            if chance_bp_720m_strict > trade_settings.mid_gap_bp_threshold + EPS
            // && binance_data_age > 100
            {
                should_try_close_based_on_720m = true;
            }
        } else {
            open_available_based_on_720m = false;
        }
    } else {
        open_available_based_on_720m = false;
        close_available_based_on_720m = false;
    }

    // let mut should_careful_compared_to_1s_snapshot = false;
    // let mut should_careful_compared_to_5s_snapshot = false;
    // let mut should_careful_compared_to_10s_snapshot = false;
    // let mut should_careful_compared_to_20s_snapshot = false;

    // if let Some(prev) = previous_snapshot_1s {
    //     let prev_gate_mid = (prev.gate_bt.ask_price + prev.gate_bt.bid_price) * 0.5;
    //     if let (Some(curr_bin_mid), Some(prev_base)) = (signal.binance_mid, prev.base_bt.as_ref()) {
    //         let prev_bin_mid = (prev_base.ask_price + prev_base.bid_price) * 0.5;
    //         let gate_rise = signal.gate_mid - prev_gate_mid;
    //         let binance_rise = curr_bin_mid - prev_bin_mid;
    //         let gate_web_rise = (signal.gate_web_ask + signal.gate_web_bid) * 0.5
    //             - (prev.gate_web_bt.ask_price + prev.gate_web_bt.bid_price) * 0.5;

    //         if order_side == "buy" {
    //             // Buy: gate mid 상승폭이 binance 상승폭 이하이어야 함.
    //             if gate_rise > binance_rise + EPS {
    //                 should_careful_compared_to_1s_snapshot = true;
    //             }
    //             if gate_web_rise > binance_rise + EPS {
    //                 should_careful_compared_to_1s_snapshot = true;
    //             }
    //         } else if order_side == "sell" {
    //             // Sell: 반대 — gate 상승폭이 binance 이상.
    //             if gate_rise < binance_rise - EPS {
    //                 should_careful_compared_to_1s_snapshot = true;
    //             }
    //             if gate_web_rise < binance_rise - EPS {
    //                 should_careful_compared_to_1s_snapshot = true;
    //             }
    //         }
    //     }
    // }

    // if let Some(prev) = previous_snapshot_5s {
    //     let prev_gate_mid = (prev.gate_bt.ask_price + prev.gate_bt.bid_price) * 0.5;
    //     if let (Some(curr_bin_mid), Some(prev_base)) = (signal.binance_mid, prev.base_bt.as_ref()) {
    //         let prev_bin_mid = (prev_base.ask_price + prev_base.bid_price) * 0.5;
    //         let gate_rise = signal.gate_mid - prev_gate_mid;
    //         let binance_rise = curr_bin_mid - prev_bin_mid;
    //         let gate_web_rise = (signal.gate_web_ask + signal.gate_web_bid) * 0.5
    //             - (prev.gate_web_bt.ask_price + prev.gate_web_bt.bid_price) * 0.5;

    //         if order_side == "buy" {
    //             // Buy: gate mid 상승폭이 binance 상승폭 이하이어야 함.
    //             if gate_rise > binance_rise + EPS && gate_web_rise > binance_rise + EPS {
    //                 should_careful_compared_to_5s_snapshot = true;
    //             }
    //         } else if order_side == "sell" {
    //             // Sell: 반대 — gate 상승폭이 binance 이상.
    //             if gate_rise < binance_rise - EPS && gate_web_rise < binance_rise - EPS {
    //                 should_careful_compared_to_5s_snapshot = true;
    //             }
    //         }
    //     }
    // }

    // if let Some(prev) = previous_snapshot_10s {
    //     let prev_gate_mid = (prev.gate_bt.ask_price + prev.gate_bt.bid_price) * 0.5;
    //     if let (Some(curr_bin_mid), Some(prev_base)) = (signal.binance_mid, prev.base_bt.as_ref()) {
    //         let prev_bin_mid = (prev_base.ask_price + prev_base.bid_price) * 0.5;
    //         let gate_rise = signal.gate_mid - prev_gate_mid;
    //         let binance_rise = curr_bin_mid - prev_bin_mid;
    //         let gate_web_rise = (signal.gate_web_ask + signal.gate_web_bid) * 0.5
    //             - (prev.gate_web_bt.ask_price + prev.gate_web_bt.bid_price) * 0.5;

    //         if order_side == "buy" {
    //             if gate_rise > binance_rise + EPS && gate_web_rise > binance_rise + EPS {
    //                 should_careful_compared_to_10s_snapshot = true;
    //             }
    //         } else if order_side == "sell" {
    //             if gate_rise < binance_rise - EPS && gate_web_rise < binance_rise - EPS {
    //                 should_careful_compared_to_10s_snapshot = true;
    //             }
    //         }
    //     }
    // }

    // if let Some(prev) = previous_snapshot_20s {
    //     let prev_gate_mid = (prev.gate_bt.ask_price + prev.gate_bt.bid_price) * 0.5;
    //     if let (Some(curr_bin_mid), Some(prev_base)) = (signal.binance_mid, prev.base_bt.as_ref()) {
    //         let prev_bin_mid = (prev_base.ask_price + prev_base.bid_price) * 0.5;
    //         let gate_rise = signal.gate_mid - prev_gate_mid;
    //         let binance_rise = curr_bin_mid - prev_bin_mid;
    //         let gate_web_rise = (signal.gate_web_ask + signal.gate_web_bid) * 0.5
    //             - (prev.gate_web_bt.ask_price + prev.gate_web_bt.bid_price) * 0.5;

    //         if order_side == "buy" {
    //             if gate_rise > binance_rise + EPS && gate_web_rise > binance_rise + EPS {
    //                 should_careful_compared_to_20s_snapshot = true;
    //             }
    //         } else if order_side == "sell" {
    //             if gate_rise < binance_rise - EPS && gate_web_rise < binance_rise - EPS {
    //                 should_careful_compared_to_20s_snapshot = true;
    //             }
    //         }
    //     }
    // }

    // let should_careful_compared_to_avg_mid_gap =
    //     if let (Some(avg_mid_gap), Some(binance_mid), Some(order_price)) =
    //         (avg_mid_gap, signal.binance_mid, signal.order_price)
    //     {
    //         let current_order_gap = (order_price - binance_mid) / binance_mid;

    //         if order_side == "buy" {
    //             current_order_gap > avg_mid_gap + EPS
    //         } else if order_side == "sell" {
    //             current_order_gap < avg_mid_gap - EPS
    //         } else {
    //             false
    //         }
    //     } else {
    //         false
    //     };

    let is_dangerous_symbol = gate_order_manager.is_limit_open_blocked(symbol);

    let should_careful_for_too_many_orders =
        gate_order_manager.is_too_many_orders(trade_settings.too_many_orders_time_gap_ms);

    let too_few_orders = !gate_order_manager.is_too_many_orders(15 * 1000 * 60);

    // let is_symbol_too_few_orders = !gate_order_manager.is_symbol_too_many_orders(
    //     symbol,
    //     1,
    //     trade_settings.too_many_orders_time_gap_ms * 3,
    // );

    let is_symbol_too_many_orders = gate_order_manager.is_symbol_too_many_orders(
        symbol,
        30,
        trade_settings.too_many_orders_time_gap_ms * 3,
    );

    let strategy_position_usdt_size: f64 = total_net_positions_usdt_size;

    let is_close_order = (order_side == "buy" && usdt_position_size < 0.0)
        || (order_side == "sell" && usdt_position_size > 0.0);

    let is_order_net_position_close_side_strategy = (strategy_position_usdt_size
        > trade_settings.max_position_size.max(
            trade_settings
                .opposite_side_max_position_size
                .unwrap_or(0.0),
        )
        && order_side == "sell")
        || (strategy_position_usdt_size
            < -trade_settings.max_position_size.max(
                trade_settings
                    .opposite_side_max_position_size
                    .unwrap_or(0.0),
            )
            && order_side == "buy");

    let is_closing_something = is_close_order
        || is_order_net_position_close_side_strategy
        || ignore_net_position_size_check;

    let is_not_too_few_orders_but_is_closing_something = !too_few_orders && is_closing_something;

    let mut size_threshold_multiplier = if should_careful_for_too_many_orders {
        trade_settings.too_many_orders_size_threshold_multiplier
    } else {
        1
    };
    if is_symbol_too_many_orders {
        size_threshold_multiplier *=
            trade_settings.symbol_too_many_orders_size_threshold_multiplier;
    }

    let few_orders_for_account_or_symbol = too_few_orders;

    let can_try_small_size =
        few_orders_for_account_or_symbol || is_not_too_few_orders_but_is_closing_something;

    // When few orders and closing: use configurable triggers (default 10).
    let size_trigger_for_few = trade_settings.size_trigger_for_few_orders.unwrap_or(100);
    let close_size_trigger_for_few = trade_settings
        .close_size_trigger_for_few_orders
        .unwrap_or(100);

    let (converted_size_trigger, converted_close_size_trigger) =
        if use_few_orders_closing_trigger && can_try_small_size {
            (size_trigger_for_few, close_size_trigger_for_few)
        } else {
            (
                (size_trigger * size_threshold_multiplier).min(max_size_trigger / 2) as i64,
                (close_size_trigger * size_threshold_multiplier).min(max_size_trigger / 2) as i64,
            )
        };

    // Latency checks
    if (current_time_ms - gate_bt_server_time) > trade_settings.gate_last_book_ticker_latency_ms {
        return None;
    }
    if let Some(binance_bt_server_time) = binance_bt_server_time {
        if (current_time_ms - binance_bt_server_time)
            > trade_settings.binance_last_book_ticker_latency_ms
        {
            binance_data_is_updated = false;
        }
    } else {
        binance_data_is_updated = false;
    }
    if (current_time_ms - gate_web_bt_server_time)
        > trade_settings.gate_last_webbook_ticker_latency_ms
    {
        return None;
    }

    // Funding rate check
    let mut only_close = false;

    if funding_rate > trade_settings.funding_rate_threshold
        || funding_rate < -trade_settings.funding_rate_threshold
    {
        only_close = true;
    }

    if gate_order_manager.is_close_symbol(symbol) {
        only_close = true;
    }
    if is_dangerous_symbol && trade_settings.only_close_dangerous_symbol {
        only_close = true;
    }

    let corr_gate_bookticker_vs_quote = market_state.get_corr_gate_bookticker_vs_quote(symbol);
    let corr_gate_webbookticker_vs_quote =
        market_state.get_corr_gate_webbookticker_vs_quote(symbol);

    if let (Some(corr_gate_bookticker_vs_quote), Some(corr_gate_webbookticker_vs_quote)) = (
        corr_gate_bookticker_vs_quote,
        corr_gate_webbookticker_vs_quote,
    ) {
        if corr_gate_bookticker_vs_quote < trade_settings.dangerous_symbols_corr_min_gate_bt + EPS {
            only_close = true;
        }
        if corr_gate_webbookticker_vs_quote
            > corr_gate_bookticker_vs_quote
                + trade_settings.dangerous_symbols_corr_web_gt_gate_margin
                + EPS
        {
            only_close = true;
        }
    }

    let is_order_net_position_close_side = (strategy_position_usdt_size
        > trade_settings.max_position_size.max(
            trade_settings
                .opposite_side_max_position_size
                .unwrap_or(0.0),
        )
        && order_side == "sell")
        || (strategy_position_usdt_size
            < -trade_settings.max_position_size.max(
                trade_settings
                    .opposite_side_max_position_size
                    .unwrap_or(0.0),
            )
            && order_side == "buy");

    let is_order_net_position_open_side = (strategy_position_usdt_size
        > trade_settings.max_position_size.max(
            trade_settings
                .opposite_side_max_position_size
                .unwrap_or(0.0),
        )
        && order_side == "buy")
        || (strategy_position_usdt_size
            < -trade_settings.max_position_size.max(
                trade_settings
                    .opposite_side_max_position_size
                    .unwrap_or(0.0),
            )
            && order_side == "sell");

    let mut level = None;
    let close_based_on_binance_ok =
        if let Some(binance_mid_gap_chance_bp) = signal.binance_mid_gap_chance_bp {
            binance_mid_gap_chance_bp > 0.0
        } else {
            false
        };

    let open_available_based_on_binance_or_close = if is_close_order {
        true
    } else if let Some(binance_mid_gap_chance_bp) = signal.binance_mid_gap_chance_bp {
        let is_binance_valid_or_net_position_close_side = signal.is_binance_valid
            || (is_order_net_position_close_side && close_based_on_binance_ok);
        // || ignore_net_position_size_check;
        binance_mid_gap_chance_bp > 0.0
            && is_binance_valid_or_net_position_close_side
            && binance_mid_gap_chance_bp.abs()
                < (trade_settings.max_gate_binance_gap_percentage_threshold * 100.0).abs()
    } else {
        false
    };

    let is_open_available_based_on_spread_or_close = if is_close_order {
        true
    } else if let Some(avg_spread) = avg_spread {
        let avg_spread_bp = avg_spread * 10000.0;
        signal.spread_bp <= avg_spread_bp * trade_settings.avg_spread_ratio_threshold_open
            && signal.spread_bp >= avg_spread_bp * 0.5
    } else {
        true
    };

    let is_close_available_based_on_spread = if let Some(avg_spread) = avg_spread {
        let avg_spread_bp = avg_spread * 10000.0;
        signal.spread_bp <= avg_spread_bp * trade_settings.avg_spread_ratio_threshold_close
            && signal.spread_bp >= avg_spread_bp * 0.5
    } else {
        true
    };

    let is_warn_stale_position = if let Some(position) = position {
        gate_order_manager.is_warn_stale_position(
            &position,
            Some(current_time_ms),
            Some(trade_settings.warn_stale_minutes),
        )
    } else {
        false
    };
    let is_big_close_order = is_close_order
        && usdt_position_size.abs()
            >= (trade_settings.max_position_size / 2.0)
                .max(trade_settings.close_order_size.unwrap_or(0.0) * 1.5);

    // let order_price = if is_warn_stale_position && is_big_close_order {
    //     // gate_mid
    //     signal.gate_mid
    // } else {
    //     order_price
    // };

    // let mut not_should_careful_count = 0;
    // if !should_careful_compared_to_1s_snapshot {
    //     not_should_careful_count += 1;
    // }
    // if !should_careful_compared_to_5s_snapshot {
    //     not_should_careful_count += 1;
    // }
    // if !should_careful_compared_to_10s_snapshot {
    //     not_should_careful_count += 1;
    // }
    // if !should_careful_compared_to_20s_snapshot {
    //     not_should_careful_count += 1;
    // }

    let is_better_than_half_of_gate_mid = if order_side == "buy" {
        let order_price_mid_gap = (signal.gate_mid - order_price).abs();
        let order_price_best_gap = (order_price - signal.gate_bid).abs();
        order_price_best_gap <= order_price_mid_gap + EPS
    } else if order_side == "sell" {
        let order_price_mid_gap = (order_price - signal.gate_mid).abs();
        let order_price_best_gap = (signal.gate_ask - order_price).abs();
        order_price_best_gap <= order_price_mid_gap + EPS
    } else {
        false
    };
    let is_on_edge_of_gate_bt = if order_side == "buy" {
        if let Some(order_price) = signal.order_price {
            order_price
                <= signal.gate_bid
                    + signal
                        .gate_contract
                        .order_price_round
                        .parse::<f64>()
                        .unwrap_or(0.0)
                    + EPS
        } else {
            false
        }
    } else if order_side == "sell" {
        if let Some(order_price) = signal.order_price {
            order_price
                >= signal.gate_ask
                    - signal
                        .gate_contract
                        .order_price_round
                        .parse::<f64>()
                        .unwrap_or(0.0)
                    - EPS
        } else {
            false
        }
    } else {
        false
    };
    // if is_close_order && is_order_net_position_close_side && !is_dangerous_symbol {
    //     not_should_careful_count > 0
    //         || !should_careful_compared_to_avg_mid_gap
    //         || open_available_based_on_1m
    // } else if is_order_net_position_close_side && !is_dangerous_symbol {
    //     not_should_careful_count > 1 || !should_careful_compared_to_avg_mid_gap
    // } else if is_close_order {
    //     not_should_careful_count > 2 || !should_careful_compared_to_avg_mid_gap
    // } else {
    //     not_should_careful_count > 2 && !should_careful_compared_to_avg_mid_gap
    // };
    let tradable_dangerous_symbol = is_better_than_half_of_gate_mid
        // && is_on_edge_of_gate_bt
        && (is_close_order || is_order_net_position_close_side)
        && close_available_based_on_5m
        && close_available_based_on_60m
        && close_available_based_on_240m
        && close_available_based_on_720m && (
            open_available_based_on_5m || open_available_based_on_60m || open_available_based_on_240m || open_available_based_on_720m
        );

    let open_available_for_dangerous_symbol_or_close = if is_close_order {
        true
    } else if is_dangerous_symbol {
        if tradable_dangerous_symbol {
            memo = Some(format!("tradable_dangerous_symbol").to_string());
            true
        } else {
            false
        }
    } else {
        true
    };
    let open_available_for_net_position_open_side_or_close = if is_close_order {
        true
    } else if !is_order_net_position_close_side {
        is_better_than_half_of_gate_mid
    } else {
        true
    };
    let open_available_for_net_position_open_side_and_open_or_is_close =
        if is_order_net_position_close_side || is_close_order {
            true
        } else {
            is_on_edge_of_gate_bt || is_better_than_half_of_gate_mid
        };

    let mut open_order_point = 0;
    let mut close_order_point = 0;
    let mut should_try_close_point = 0;

    if open_available_based_on_1m {
        open_order_point += 1;
    }
    if open_available_based_on_5m {
        open_order_point += 1;
    }
    if open_available_based_on_60m {
        open_order_point += 1;
    }
    if open_available_based_on_240m {
        open_order_point += 1;
    }
    if open_available_based_on_720m {
        open_order_point += 1;
    }

    if close_available_based_on_1m {
        close_order_point += 1;
    }
    if close_available_based_on_5m {
        close_order_point += 1;
    }
    if close_available_based_on_60m {
        close_order_point += 1;
    }
    if close_available_based_on_240m {
        close_order_point += 1;
    }
    if close_available_based_on_720m {
        close_order_point += 1;
    }

    if should_try_close_based_on_60m {
        should_try_close_point += 1;
    }
    if should_try_close_based_on_240m {
        should_try_close_point += 1;
    }
    if should_try_close_based_on_720m {
        should_try_close_point += 1;
    }

    let flipster_mode = crate::flipster_cookie_store::global().is_some();
    let order_available_based_on_previous_snapshots = if flipster_mode {
        // Flipster: market_watcher populates QuestDB/Redis for 1m/5m/60m but the
        // 240m/720m Redis keys aren't guaranteed. Require only the short-window
        // short-windows to pass.
        open_order_point >= 2 && close_order_point >= 2
    } else if !is_close_order && !is_order_net_position_close_side {
            // close_available_based_on_1m
            //     && open_available_based_on_5m
            //     && open_available_based_on_60m
            //     && (open_available_based_on_240m || open_available_based_on_720m)
            open_order_point > 3 && close_order_point > 4
        } else if is_big_close_order && is_order_net_position_close_side
            || (is_close_order && is_dangerous_symbol)
        {
            // (close_available_based_on_1m || close_available_based_on_5m || close_available_based_on_60m)
            //     || (close_available_based_on_240m && close_available_based_on_720m)
            open_order_point > 2 && close_order_point > 3
        } else {
            // (open_available_based_on_1m || open_available_based_on_5m || open_available_based_on_60m)
            //     && (close_available_based_on_240m || close_available_based_on_720m)
            open_order_point > 2 && close_order_point > 4
        };

    // ====== LIMIT OPEN (Entry - Same as Basic) ======
    let cond_1 = signal.mid_gap_chance_bp > trade_settings.mid_gap_bp_threshold - EPS;
    let cond_2 = signal.orderbook_size > 0.0;
    let cond_3 = signal.spread_bp > trade_settings.spread_bp_threshold - EPS;
    let cond_4 = (signal.orderbook_size as i64) > converted_size_trigger;
    let cond_5 = open_available_based_on_binance_or_close;
    let cond_6 = is_open_available_based_on_spread_or_close;
    let cond_7 = binance_data_is_updated;
    let cond_8 = (signal.orderbook_size as i64) <= max_size_trigger;
    let cond_9 = order_available_based_on_previous_snapshots;
    let cond_10 = open_available_for_dangerous_symbol_or_close;
    let cond_11 = open_available_for_net_position_open_side_or_close;
    let cond_12 = open_available_for_net_position_open_side_and_open_or_is_close;

    // Flipster per-gate failure counters.
    if crate::flipster_cookie_store::global().is_some() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: [AtomicU64; 12] = [
            AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
            AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
            AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
        ];
        static TOTAL: AtomicU64 = AtomicU64::new(0);
        let conds = [cond_1, cond_2, cond_3, cond_4, cond_5, cond_6, cond_7, cond_8, cond_9, cond_10, cond_11, cond_12];
        for (i, &c) in conds.iter().enumerate() {
            if !c { C[i].fetch_add(1, Ordering::Relaxed); }
        }
        let t = TOTAL.fetch_add(1, Ordering::Relaxed);
        if t % 5000 == 0 {
            warn!(
                "[FLIPSTER-GATES] t={} fail: c1={} c2={} c3={} c4={} c5={} c6={} c7={} c8={} c9={} c10={} c11={} c12={}",
                t,
                C[0].load(Ordering::Relaxed), C[1].load(Ordering::Relaxed),
                C[2].load(Ordering::Relaxed), C[3].load(Ordering::Relaxed),
                C[4].load(Ordering::Relaxed), C[5].load(Ordering::Relaxed),
                C[6].load(Ordering::Relaxed), C[7].load(Ordering::Relaxed),
                C[8].load(Ordering::Relaxed), C[9].load(Ordering::Relaxed),
                C[10].load(Ordering::Relaxed), C[11].load(Ordering::Relaxed),
            );
        }
    }

    if cond_1 && cond_2 && cond_3 && cond_4 && cond_5 && cond_6
        && cond_7 && cond_8 && cond_9 && cond_10 && cond_11 && cond_12
    {
        level = Some(if only_close {
            "limit_close".to_string()
        } else {
            "limit_open".to_string()
        });
    } else if open_order_point > 4 && should_try_close_point > 2 {
        memo = Some(format!("limit_open_v1").to_string());
        level = Some(if only_close {
            "limit_close".to_string()
        } else {
            "limit_open".to_string()
        });
    } else if (!is_order_net_position_open_side || is_big_close_order)
        && signal.mid_gap_chance_bp > trade_settings.mid_gap_bp_threshold - EPS
        // && (signal.orderbook_size as i64) > converted_size_trigger
        && open_available_for_dangerous_symbol_or_close
        && binance_data_is_updated
        && open_order_point > 3
        && close_order_point > 4
        && should_try_close_point > 0
    {
        memo = Some(format!("limit_open_v2").to_string());
        level = Some(if only_close {
            "limit_close".to_string()
        } else {
            "limit_open".to_string()
        });
    } else if (!is_order_net_position_open_side || is_big_close_order)
        // && (signal.orderbook_size as i64) > converted_size_trigger
        && open_available_for_dangerous_symbol_or_close
        && binance_data_is_updated
        && open_order_point > 4
        && close_order_point > 4
        && should_try_close_point > 1
    {
        memo = Some(format!("limit_open_v3").to_string());
        level = Some(if only_close {
            "limit_close".to_string()
        } else {
            "limit_open".to_string()
        });
    } else if ((!is_order_net_position_open_side
        && is_close_order) || is_big_close_order)
        // && (signal.orderbook_size as i64) > converted_size_trigger
        && open_available_for_dangerous_symbol_or_close
        && binance_data_is_updated
        && open_order_point > 3
        && close_order_point > 4
        && should_try_close_point > 1
    {
        memo = Some(format!("limit_close_v3").to_string());
        level = Some("limit_close".to_string());
    } else if ((!is_order_net_position_open_side
        && is_close_order) || is_big_close_order)
        && signal.mid_gap_chance_bp > trade_settings.mid_gap_bp_threshold - EPS
        // && (signal.orderbook_size as i64) > converted_size_trigger
        && open_available_for_dangerous_symbol_or_close
        && binance_data_is_updated
        && open_order_point > 2
        && close_order_point > 3
        && should_try_close_point > 0
    {
        memo = Some(format!("limit_close_v2").to_string());
        level = Some("limit_close".to_string());
    }
    // ====== LIMIT CLOSE (Exit - Combined Narrative Exit) ======
    else if is_order_net_position_close_side
        || is_warn_stale_position
        || is_dangerous_symbol
        || is_big_close_order
    {
        let has_long_position = usdt_position_size > 0.0;
        let gate_mid = signal.gate_mid;

        let mut available_order_price_threshold_multiplier = 0.0;
        let mut order_price_best_gap_ratio = -0.01;

        if is_order_net_position_close_side {
            available_order_price_threshold_multiplier += 1.0;
            order_price_best_gap_ratio += 1.0;
        }
        if is_big_close_order && is_order_net_position_close_side {
            order_price_best_gap_ratio += 0.5;
        }
        if is_warn_stale_position && is_order_net_position_close_side {
            order_price_best_gap_ratio += 0.5;
        }

        let should_exit_profit = signal.orderbook_size > 0.0
            && signal.spread_bp > trade_settings.spread_bp_threshold
            && (signal.orderbook_size as i64) > converted_close_size_trigger
            && signal.mid_gap_chance_bp > trade_settings.close_raw_mid_profit_bp
            && (signal.orderbook_size as i64) <= max_size_trigger
            && is_close_available_based_on_spread
            && order_available_based_on_previous_snapshots;

        // let close_order_count_exceeded =
        //     order_count_size.abs() > trade_settings.close_order_count as f64;

        if should_exit_profit
            && ((close_based_on_binance_ok
            // && close_order_count_exceeded
            && binance_data_is_updated)
                || is_warn_stale_position)
        {
            if has_long_position && order_side == "sell" {
                if is_order_net_position_close_side
                    && (!is_dangerous_symbol || tradable_dangerous_symbol)
                    && binance_data_is_updated
                    && !only_close
                {
                    level = Some("limit_open".to_string());
                } else {
                    level = Some("limit_close".to_string());
                }
            } else if !has_long_position && order_side == "buy" {
                if is_order_net_position_close_side
                    && (!is_dangerous_symbol || tradable_dangerous_symbol)
                    && binance_data_is_updated
                    && !only_close
                {
                    level = Some("limit_open".to_string());
                } else {
                    level = Some("limit_close".to_string());
                }
            }
        } else if should_exit_profit {
            if order_side == "buy" {
                let available_order_price = signal.gate_bid
                    + signal
                        .gate_contract
                        .order_price_round
                        .parse::<f64>()
                        .unwrap_or(0.0)
                        * available_order_price_threshold_multiplier;

                let order_price_mid_gap = (order_price - gate_mid).abs();
                let order_price_best_gap = (signal.gate_bid - order_price).abs();

                if available_order_price >= order_price && order_price < (gate_mid - EPS) {
                    if is_order_net_position_close_side && binance_data_is_updated && !only_close {
                        level = Some("limit_open".to_string());
                    } else {
                        level = Some("limit_close".to_string());
                    }
                } else if order_price_best_gap
                    <= order_price_mid_gap * order_price_best_gap_ratio + EPS
                    && order_price < gate_mid - EPS
                    && is_order_net_position_close_side
                {
                    if is_order_net_position_close_side
                        && (!is_dangerous_symbol || tradable_dangerous_symbol)
                        && binance_data_is_updated
                        && !only_close
                    {
                        level = Some("limit_open".to_string());
                    } else {
                        level = Some("limit_close".to_string());
                    }
                }
            } else if order_side == "sell" {
                let available_order_price = signal.gate_ask
                    - signal
                        .gate_contract
                        .order_price_round
                        .parse::<f64>()
                        .unwrap_or(0.0)
                        * available_order_price_threshold_multiplier;

                let order_price_mid_gap = (order_price - gate_mid).abs();
                let order_price_best_gap = (signal.gate_ask - order_price).abs();
                if available_order_price <= order_price && order_price > (gate_mid + EPS) {
                    if is_order_net_position_close_side && binance_data_is_updated && !only_close {
                        level = Some("limit_open".to_string());
                    } else {
                        level = Some("limit_close".to_string());
                    }
                } else if order_price_best_gap
                    <= order_price_mid_gap * order_price_best_gap_ratio + EPS
                    && order_price > gate_mid + EPS
                    && is_order_net_position_close_side
                {
                    if is_order_net_position_close_side
                        && (!is_dangerous_symbol || tradable_dangerous_symbol)
                        && binance_data_is_updated
                        && !only_close
                    {
                        level = Some("limit_open".to_string());
                    } else {
                        level = Some("limit_close".to_string());
                    }
                }
            }
        }
    }

    // if should_try_close_based_on_1m
    //     && level.is_none()
    //     && (is_dangerous_symbol || is_warn_stale_position)
    //     && is_big_close_order
    //     && !should_careful_for_too_many_orders_based_on_time_gap
    //     && corr_gate_webbookticker_vs_quote.unwrap_or(0.0) > 0.85
    // {
    //     level = Some("limit_close".to_string());
    //     if order_side == "buy" {
    //         order_price = signal.gate_web_ask;
    //     } else if order_side == "sell" {
    //         order_price = signal.gate_web_bid;
    //     }
    // } else

    if trade_settings.use_long_term_chance_close & level.is_none()
        && corr_gate_webbookticker_vs_quote.unwrap_or(0.0) > 0.80
    {
        if is_big_close_order && (is_order_net_position_close_side || is_dangerous_symbol) {
            if should_try_close_point > 1 && open_order_point > 3 {
                level = Some("limit_close".to_string());
                if order_side == "buy" {
                    order_price = signal.gate_web_ask;
                } else if order_side == "sell" {
                    order_price = signal.gate_web_bid;
                }
                memo = Some(
                    format!(
                        "long_term_chance_close 240m={} 720m={}",
                        should_try_close_based_on_240m, should_try_close_based_on_720m
                    )
                    .to_string(),
                );
            }
        } else if is_big_close_order || is_order_net_position_close_side || is_dangerous_symbol {
            if should_try_close_point > 1 && open_order_point > 3 {
                level = Some("limit_close".to_string());
                if order_side == "buy" {
                    order_price = signal.gate_web_ask;
                } else if order_side == "sell" {
                    order_price = signal.gate_web_bid;
                }
                memo = Some(
                    format!(
                        "long_term_chance_close 240m={} 720m={}",
                        should_try_close_based_on_240m, should_try_close_based_on_720m
                    )
                    .to_string(),
                );
            }
        } else if !is_dangerous_symbol
            && is_order_net_position_close_side
            && open_available_based_on_binance_or_close
            && should_try_close_point > 2
            && open_order_point > 3
        {
            level = Some("limit_open".to_string());
            if order_side == "buy" {
                order_price = signal.gate_web_ask;
            } else if order_side == "sell" {
                order_price = signal.gate_web_bid;
            }
            memo = Some(
                format!(
                    "long_term_chance_open 240m={} 720m={}",
                    should_try_close_based_on_240m, should_try_close_based_on_720m
                )
                .to_string(),
            );
        } else if signal.binance_mid_gap_chance_bp.unwrap_or(0.0) > 0.0
            && (should_try_close_point > 1 || open_order_point > 3)
        {
            if is_order_net_position_close_side && !is_dangerous_symbol {
                level = Some("limit_open".to_string());
                memo = Some(
                    format!(
                        "long_term_chance_open 240m={} 720m={}",
                        should_try_close_based_on_240m, should_try_close_based_on_720m
                    )
                    .to_string(),
                );
            } else {
                level = Some("limit_close".to_string());
                memo = Some(
                    format!(
                        "long_term_chance_close 240m={} 720m={}",
                        should_try_close_based_on_240m, should_try_close_based_on_720m
                    )
                    .to_string(),
                );
            }
            if order_side == "buy" {
                order_price = signal.gate_web_ask;
            } else if order_side == "sell" {
                order_price = signal.gate_web_bid;
            }
        }
    }

    // let order_price = if is_warn_stale_position
    //     && is_big_close_order
    //     && open_available_based_on_binance_or_close
    //     && is_order_net_position_close_side
    // {
    //     // gate_mid
    //     signal.gate_mid
    // } else {
    //     if order_side == "buy" {
    //         signal.gate_web_ask
    //     } else if order_side == "sell" {
    //         signal.gate_web_bid
    //     } else {
    //         order_price
    //     }
    // };

    // Flipster diagnostic — log when level is None to expose which gate blocked.
    if crate::flipster_cookie_store::global().is_some() && level.is_none() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NONE_CTR: AtomicU64 = AtomicU64::new(0);
        let c = NONE_CTR.fetch_add(1, Ordering::Relaxed);
        if c % 500 == 0 {
            warn!(
                "[FLIPSTER-DEC] level=None side={} mid_gap_bp={:.2} orderbook_size={} is_binance_valid={} only_close={} converted_size_trigger={} converted_close_size_trigger={} contract_order_size={} order_count_size={:.3} usdt_pos={:.2}",
                order_side,
                signal.mid_gap_chance_bp,
                signal.orderbook_size,
                signal.is_binance_valid,
                only_close,
                converted_size_trigger,
                converted_close_size_trigger,
                contract_order_size,
                order_count_size,
                usdt_position_size,
            );
        }
    }
    let mut level = level?;

    // Position direction check for close orders
    if level.contains("close") {
        let position_size = gate_order_manager.get_position_size(symbol);

        if position_size >= 0 && order_side == "buy" {
            return None;
        }
        if position_size <= 0 && order_side == "sell" {
            return None;
        }
    }

    let is_opposite_side_position = (usdt_position_size > 0.0 && order_side == "sell")
        || (usdt_position_size < 0.0 && order_side == "buy");

    if gate_order_manager.should_market_close(symbol) && is_opposite_side_position {
        level = "market_close".to_string();
    }

    let order_tif = match level.as_str() {
        "limit_open" => trade_settings.limit_open_tif.clone(),
        "limit_close" => trade_settings.limit_close_tif.clone(),
        "market_close" => "ioc".to_string(),
        "market_open" => "ioc".to_string(),
        _ => "fok".to_string(),
    };

    Some(OrderDecision {
        level,
        order_tif,
        order_size: mutated_contract_order_size,
        only_close,
        order_price: Some(order_price),
        memo: memo,
    })
}

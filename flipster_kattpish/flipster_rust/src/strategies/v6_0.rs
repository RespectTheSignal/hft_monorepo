// V6_0: Same as V6 but intended to run with repeat_profitable_order: false in trade_settings
// (no automatic repeat order after a profitable fill).

use crate::config::TradeSettings;
use crate::data_cache::SymbolBooktickerSnapshot;
use crate::gate_order_manager::{GateContract, GateOrderManager, LastOrder};
use crate::handle_chance::{Chance, HandleChanceManager};
use crate::order_decision::OrderDecision;
use crate::signal_calculator::{self, SignalResult};
use crate::strategy_core::StrategyCore;
use crate::types::{BookTickerC, TradeC};
use log::{debug, error, info, warn};
use rand::Rng;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_POSITION_SIDE_RATIO: f64 = 0.5; // Maximum allowed ratio for net exposure (50%)
static LAST_TOO_MANY_ORDERS_WARN_MS: AtomicI64 = AtomicI64::new(0);
static LAST_NOT_HEALTHY_WARN_MS: AtomicI64 = AtomicI64::new(0);
static LAST_LIMIT_OPEN_BLOCKED_WARN_MS: AtomicI64 = AtomicI64::new(0);

const EPS: f64 = 1e-12;

pub struct V6_0StrategyCore {
    handle_chance_manager: Arc<HandleChanceManager>,
}

impl V6_0StrategyCore {
    pub fn new(leverage: Option<f64>) -> Self {
        Self {
            handle_chance_manager: Arc::new(HandleChanceManager::new(leverage)),
        }
    }
}

impl StrategyCore for V6_0StrategyCore {
    fn name(&self) -> &str {
        "V6_0Strategy"
    }

    fn calculate_signal(
        &self,
        snapshot: &SymbolBooktickerSnapshot,
        contract: &GateContract,
        _gate_last_trade: Option<TradeC>,
        _avg_mid_gap: Option<f64>,
        _market_state: &crate::market_watcher::MarketGapState,
    ) -> SignalResult {
        let gate_bt = &snapshot.gate_bt;
        let gate_web_bt = &snapshot.gate_web_bt;
        let binance_bt = snapshot.base_bt.as_ref();
        // Entry logic is same as Basic (same as V6)
        let mut signal =
            signal_calculator::calculate_signal(snapshot, &contract);
        if signal.order_side.is_some() && signal.order_price.is_some() {
            return signal;
        } else {
            let order_side = if (gate_bt.ask_price + gate_bt.bid_price) * 0.5
                < (gate_web_bt.ask_price + gate_web_bt.bid_price) * 0.5
            {
                "sell"
            } else if (gate_bt.ask_price + gate_bt.bid_price) * 0.5
                > (gate_web_bt.ask_price + gate_web_bt.bid_price) * 0.5
            {
                "buy"
            } else if let Some(binance_bt) = binance_bt {
                if (gate_bt.ask_price + gate_bt.bid_price)
                    < (binance_bt.ask_price + binance_bt.bid_price) * 0.5
                {
                    "buy"
                } else if (gate_bt.ask_price + gate_bt.bid_price)
                    > (binance_bt.ask_price + binance_bt.bid_price) * 0.5
                {
                    "sell"
                } else {
                    return signal;
                }
            } else {
                return signal;
            };
            let order_price = if order_side == "buy" {
                gate_web_bt.bid_price
            } else {
                gate_web_bt.ask_price
            };
            signal.order_side = Some(order_side.to_string());
            signal.order_price = Some(order_price);
            return signal;
        }
    }

    fn decide_order(
        &self,
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
        _avg_entry_price: Option<f64>,
        _gate_last_trade: Option<TradeC>,
        _avg_mid_gap: Option<f64>,
        _avg_spread: Option<f64>,
        total_net_positions_usdt_size: f64,
        _previous_snapshot_1s: Option<&SymbolBooktickerSnapshot>,
        _previous_snapshot_5s: Option<&SymbolBooktickerSnapshot>,
        _previous_snapshot_10s: Option<&SymbolBooktickerSnapshot>,
        _previous_snapshot_20s: Option<&SymbolBooktickerSnapshot>,
        gate_order_manager: &GateOrderManager,
        _market_state: &crate::market_watcher::MarketGapState,
    ) -> Option<OrderDecision> {
        // V6_0: Same as V6 (Gap Dissolve Exit Logic). Use repeat_profitable_order: false in config to disable repeat order.
        if !signal.has_signal() {
            return None;
        }

        let is_gate_web_bookticker_in_between_gate_bookticker = (signal.gate_web_bid
            >= signal.gate_bid - EPS)
            && (signal.gate_web_ask <= signal.gate_ask + EPS);
        if !is_gate_web_bookticker_in_between_gate_bookticker {
            return None;
        }

        let current_time_sec = current_time_ms / 1000 as i64;
        let order_side = signal.order_side.as_deref()?;
        let order_price = signal.order_price?;
        let contract = gate_order_manager.get_contract(symbol);
        let mut mutated_contract_order_size = contract_order_size.min(signal.orderbook_size as i64);

        let mut binance_data_is_updated = true;

        let should_careful_for_too_many_orders =
            gate_order_manager.is_too_many_orders(trade_settings.too_many_orders_time_gap_ms * 2);

        let is_symbol_too_many_orders = gate_order_manager.is_symbol_too_many_orders(
            symbol,
            30,
            trade_settings.too_many_orders_time_gap_ms * 3,
        );

        let mut size_threshold_multiplier = if should_careful_for_too_many_orders {
            trade_settings.too_many_orders_size_threshold_multiplier
        } else {
            1
        };
        if is_symbol_too_many_orders {
            size_threshold_multiplier *=
                trade_settings.symbol_too_many_orders_size_threshold_multiplier;
        }
        let converted_size_trigger =
            (size_trigger * size_threshold_multiplier).min(max_size_trigger / 2) as i64;
        let converted_close_size_trigger =
            (close_size_trigger * size_threshold_multiplier).min(max_size_trigger / 2) as i64;

        if (current_time_ms - gate_bt_server_time) > trade_settings.gate_last_book_ticker_latency_ms
        {
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

        let mut only_close = false;
        if funding_rate > trade_settings.funding_rate_threshold {
            only_close = true;
        } else if funding_rate < -trade_settings.funding_rate_threshold {
            only_close = true;
        }
        if gate_order_manager.is_close_symbol(symbol) {
            only_close = true;
        }

        let account_position_usdt_size: f64 = total_net_positions_usdt_size;
        let is_order_net_position_close_side =
            if account_position_usdt_size > 0.0 && order_side == "sell" {
                true
            } else if usdt_position_size < 0.0 && order_side == "buy" {
                true
            } else {
                false
            };

        let mut level = None;
        let close_based_on_binance_ok =
            if let Some(binance_mid_gap_chance_bp) = signal.binance_mid_gap_chance_bp {
                binance_mid_gap_chance_bp > 0.0
            } else {
                false
            };

        let position = gate_order_manager.get_position(symbol);
        if let Some(position) = position {
            if position.update_time > 0
                && position.update_time < current_time_sec - trade_settings.close_stale_minutes * 60
            {
                let close_stale_minutes_size = trade_settings.close_stale_minutes_size;
                let contract = contract?;
                let quanto_multiplier = contract.quanto_multiplier.parse::<f64>().unwrap_or(0.0);
                let mut order_size = contract_order_size;
                if let Some(close_stale_minutes_size) = close_stale_minutes_size {
                    if close_stale_minutes_size > 0.0 && quanto_multiplier > 0.0 {
                        order_size = (close_stale_minutes_size * quanto_multiplier)
                            .max(contract_order_size as f64)
                            as i64;
                    }
                    info!(
                        "[DecideOrder] Closing position for {} with size {}",
                        symbol, order_size
                    );
                    return Some(OrderDecision {
                        level: "market_close".to_string(),
                        order_tif: "ioc".to_string(),
                        order_size: order_size,
                        only_close: true,
                        order_price: Some(order_price),
                        memo: None,
                    });
                } else {
                    return Some(OrderDecision {
                        level: "market_close".to_string(),
                        order_tif: "ioc".to_string(),
                        order_size: order_size,
                        only_close: true,
                        order_price: Some(order_price),
                        memo: None,
                    });
                }
            } else if position.update_time == 0 {
                warn!("[DecideOrder] Position update time is 0 for {}", symbol);
            }
        }

        if signal.mid_gap_chance_bp > trade_settings.mid_gap_bp_threshold
            && signal.orderbook_size > 0.0
            && signal.spread_bp > trade_settings.spread_bp_threshold
            && (signal.orderbook_size as i64) > converted_size_trigger
            && signal.is_binance_valid
            && binance_data_is_updated
            && (signal.orderbook_size as i64) <= max_size_trigger
            && signal.binance_mid_gap_chance_bp.is_some()
            && signal.binance_mid_gap_chance_bp.unwrap().abs()
                < (trade_settings.max_gate_binance_gap_percentage_threshold * 100.0).abs()
        {
            level = Some(if only_close {
                "limit_close".to_string()
            } else {
                "limit_open".to_string()
            });
            if is_order_net_position_close_side && trade_settings.limit_open_tif == "ioc" {
                mutated_contract_order_size = contract_order_size * 2;
            }
        } else {
            let has_long_position = usdt_position_size > 0.0;
            let gate_mid = signal.gate_mid;

            let should_exit_profit = signal.orderbook_size > 0.0
                && signal.spread_bp > trade_settings.spread_bp_threshold
                && (signal.orderbook_size as i64) > converted_close_size_trigger
                && signal.mid_gap_chance_bp > trade_settings.close_raw_mid_profit_bp
                && (signal.orderbook_size as i64) <= max_size_trigger;

            let close_order_count_exceeded =
                order_count_size.abs() > trade_settings.close_order_count as f64;

            if should_exit_profit
                && close_based_on_binance_ok
                && close_order_count_exceeded
                && binance_data_is_updated
            {
                if has_long_position && order_side == "sell" {
                    level = Some("limit_close".to_string());
                } else if !has_long_position && order_side == "buy" {
                    level = Some("limit_close".to_string());
                }
            } else if should_exit_profit {
                if order_side == "buy" {
                    let available_order_price = signal.gate_bid
                        + signal
                            .gate_contract
                            .order_price_round
                            .parse::<f64>()
                            .unwrap_or(0.0);
                    let order_price_mid_gap = (order_price - gate_mid).abs();
                    let order_price_best_gap = (signal.gate_bid - order_price).abs();
                    if available_order_price >= order_price && order_price < gate_mid {
                        level = Some("limit_close".to_string());
                    } else if order_price_best_gap < order_price_mid_gap
                        && order_price < gate_mid
                        && is_order_net_position_close_side
                    {
                        level = Some("limit_close".to_string());
                    }
                } else if order_side == "sell" {
                    let available_order_price = signal.gate_ask
                        - signal
                            .gate_contract
                            .order_price_round
                            .parse::<f64>()
                            .unwrap_or(0.0);
                    let order_price_mid_gap = (order_price - gate_mid).abs();
                    let order_price_best_gap = (signal.gate_ask - order_price).abs();
                    if available_order_price <= order_price && order_price > gate_mid {
                        level = Some("limit_close".to_string());
                    } else if order_price_best_gap < order_price_mid_gap
                        && order_price > gate_mid
                        && is_order_net_position_close_side
                    {
                        level = Some("limit_close".to_string());
                    }
                }
            }
        }

        let mut level = level?;

        if level.contains("close") {
            let position_size = gate_order_manager.get_position_size(symbol);
            if position_size >= 0 && order_side == "buy" {
                return None;
            }
            if position_size <= 0 && order_side == "sell" {
                return None;
            }
        }

        let is_opposite_side_position = if usdt_position_size > 0.0 && order_side == "sell" {
            true
        } else if usdt_position_size < 0.0 && order_side == "buy" {
            true
        } else {
            false
        };

        if gate_order_manager.should_market_close(symbol) && !is_opposite_side_position {
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
            memo: None,
        })
    }

    fn handle_chance(
        &self,
        chance: &Chance,
        gate_order_manager: &GateOrderManager,
        trade_settings: &TradeSettings,
        futures_account_total: f64,
        futures_account_unrealised_pnl: f64,
        total_net_positions_usdt_size: f64,
    ) -> Option<(i64, f64, bool)> {
        let symbol = &chance.symbol;
        let order_side = &chance.side;
        let is_buy = order_side == "buy";

        if !gate_order_manager.is_account_symbol(symbol) {
            return None;
        }

        let level = &chance.level;
        let last_order = match gate_order_manager.last_orders.read().get(symbol).cloned() {
            Some(last_order) => Some(last_order),
            None => None,
        };

        if level == "limit_close" {
            let position_size = gate_order_manager.get_position_size(symbol);
            let min_order_size = gate_order_manager.get_min_order_size(symbol);
            if position_size.abs() < min_order_size {
                return None;
            }
            if position_size > 0 && chance.side == "buy" {
                error!(
                    "[HandleChance] Step 2 FAILED: Long position but close-buy for {}",
                    symbol
                );
                return None;
            } else if position_size < 0 && chance.side == "sell" {
                error!(
                    "[HandleChance] Step 2 FAILED: Short position but close-sell for {}",
                    symbol
                );
                return None;
            }
        } else {
        }

        let current_ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        if !chance.bypass_time_restriction {
            if let Some(last) = last_order.clone() {
                if last.level == chance.level
                    && last.side == chance.side
                    && (last.price - chance.price).abs() < 1e-9
                    && last.timestamp
                        > current_ts_ms
                            - rand::rng().random_range(
                                trade_settings.same_side_price_time_restriction_ms_min
                                    ..=trade_settings.same_side_price_time_restriction_ms_max,
                            )
                {
                    return None;
                }
            }
            if let Some(last) = last_order.clone() {
                if last.level == "market_open"
                    && last.side == chance.side
                    && last.timestamp > current_ts_ms - 1000
                {
                    return None;
                }
                if last.level == "market_close" && last.timestamp > current_ts_ms - 1000 {
                    return None;
                }
            }
        } else {
        }

        let account_balance = futures_account_total + futures_account_unrealised_pnl;
        if account_balance <= 0.0 {
            warn!(
                "[HandleChance] Step 3 FAILED: Account balance is 0 for {}",
                gate_order_manager.login_name
            );
            return None;
        }

        let mut max_position_size = if trade_settings.dynamic_max_position_size {
            account_balance * trade_settings.max_position_size_multiplier
        } else {
            trade_settings.max_position_size
        };

        let risk_limit = gate_order_manager.get_risk_limit(symbol);
        if total_net_positions_usdt_size > 0.0 && chance.side == "sell" {
            max_position_size = trade_settings
                .opposite_side_max_position_size
                .unwrap_or(max_position_size * 1.5);
        } else if total_net_positions_usdt_size < 0.0 && chance.side == "buy" {
            max_position_size = trade_settings
                .opposite_side_max_position_size
                .unwrap_or(max_position_size * 1.5);
        }
        if risk_limit < max_position_size && risk_limit > 0.0 {
            max_position_size = max_position_size.min(risk_limit as f64);
        }

        let is_symbol_too_many_orders = gate_order_manager.is_symbol_too_many_orders(
            symbol,
            30,
            trade_settings.too_many_orders_time_gap_ms * 2,
        );
        let should_careful_for_too_many_orders =
            gate_order_manager.is_too_many_orders(trade_settings.too_many_orders_time_gap_ms * 2);
        let is_too_many_orders = gate_order_manager
            .is_too_many_orders(trade_settings.too_many_orders_time_gap_ms)
            || is_symbol_too_many_orders;

        let mut bypass_safe_limit_close = chance.bypass_safe_limit_close;
        let order_size_opt = gate_order_manager.get_order_size();

        let (position_usdt_size, is_same_side) = if gate_order_manager.has_position(symbol) {
            let pos_size = gate_order_manager.get_usdt_position_size(symbol);
            let same_side = (pos_size > 0.0 && is_buy) || (pos_size < 0.0 && !is_buy);
            if !same_side {
                if let Some(os) = order_size_opt {
                    if pos_size.abs() > os * 5.0 {
                        bypass_safe_limit_close = true;
                    }
                }
            }
            if level.contains("close") && same_side {
                return None;
            }
            (pos_size, same_side)
        } else {
            if level.contains("close") {
                return None;
            }
            (0.0, false)
        };
        let is_open_order = is_same_side || !gate_order_manager.has_position(symbol);

        let mut time_sleep_multiplier = if should_careful_for_too_many_orders {
            2
        } else {
            1
        };
        if is_too_many_orders {
            time_sleep_multiplier *= 2;
        }
        if gate_order_manager.is_recently_too_many_orders() {
            time_sleep_multiplier *= 2;
        }

        if !chance.bypass_time_restriction {
            if let Some(last) = last_order.clone() {
                if last.level == "limit_open"
                    && is_same_side
                    && last.side == chance.side
                    && last.timestamp
                        > current_ts_ms
                            - rand::rng().random_range(
                                trade_settings.limit_open_time_restriction_ms_min
                                    ..=trade_settings.limit_open_time_restriction_ms_max,
                            ) * time_sleep_multiplier
                {
                    return None;
                }
            }
            if let Some(last) = last_order.clone() {
                if (last.level == "limit_close" || !is_same_side)
                    && last.side == chance.side
                    && last.timestamp
                        > current_ts_ms
                            - rand::rng().random_range(
                                trade_settings.limit_close_time_restriction_ms_min
                                    ..=trade_settings.limit_close_time_restriction_ms_max,
                            ) * time_sleep_multiplier
                {
                    return None;
                }
            }
        } else {
        }

        let mut order_size = chance.size.abs();
        let trade_count = gate_order_manager.get_trade_count(symbol) as f64 + 0.1;
        let profitable_trade_count = gate_order_manager.get_profitable_trade_count(symbol);
        let profitable_rate = profitable_trade_count as f64 / trade_count as f64;
        let profit_bp_ema = gate_order_manager.get_profit_bp_ema(symbol);
        let is_close_order = level.contains("close") || !is_same_side;

        if trade_count > 5.0 && !is_close_order {
            if profit_bp_ema < trade_settings.profit_bp_ema_threshold {
                if should_careful_for_too_many_orders {
                    return None;
                } else if is_symbol_too_many_orders {
                    return None;
                }
                order_size = (order_size as f64 * 0.01).round() as i64;
            }
        }
        if is_too_many_orders && !is_close_order {
            if profit_bp_ema < trade_settings.profit_bp_ema_threshold {
                return None;
            } else if profitable_rate < trade_settings.succes_threshold {
                return None;
            }
        }

        let mut usdt_order_size = if order_size != 0 {
            gate_order_manager.get_usdt_amount_from_size(symbol, order_size)
        } else {
            trade_settings.order_size
        };

        let usdt_position_size = gate_order_manager.get_usdt_position_size(symbol);
        let mut is_close_order = false;
        if usdt_position_size > 0.0 && !is_buy {
            is_close_order = true;
        } else if usdt_position_size < 0.0 && is_buy {
            is_close_order = true;
        }

        if level.contains("close") || is_close_order {
            let close_order_size = if level.contains("market") {
                trade_settings
                    .market_close_order_size
                    .or(trade_settings.close_order_size)
            } else {
                trade_settings.close_order_size
            };
            if let Some(close_usdt) = close_order_size {
                usdt_order_size = close_usdt;
                if (usdt_position_size / 5.0).abs() > close_usdt {
                    usdt_order_size = (usdt_position_size / 5.0).abs();
                }
                order_size = gate_order_manager.get_size_from_usdt_amount(symbol, usdt_order_size);
            } else {
                usdt_order_size = trade_settings.order_size;
                if (usdt_position_size / 5.0).abs() > trade_settings.order_size {
                    usdt_order_size = (usdt_position_size / 5.0).abs();
                }
                order_size = gate_order_manager.get_size_from_usdt_amount(symbol, usdt_order_size);
            }
        }

        if order_size == 0 {
            if let Some(calculated_usdt) = order_size_opt {
                order_size = gate_order_manager.get_size_from_usdt_amount(symbol, calculated_usdt);
            }
        }
        order_size = order_size.abs();

        if level.contains("market") {
            order_size = order_size.min(chance.size);
        }
        let min_order_size = gate_order_manager.get_min_order_size(symbol);
        order_size = order_size.max(min_order_size);

        if is_too_many_orders {
            max_position_size = max_position_size * 0.5;
        }

        if gate_order_manager.has_position(symbol) {
            if is_same_side {
                if position_usdt_size.abs() + usdt_order_size.abs() > max_position_size {
                    let side_color = if chance.side == "buy" {
                        "\x1b[92m"
                    } else {
                        "\x1b[91m"
                    };
                    let reset = "\x1b[0m";
                    let side_str = format!("{}{}{}", side_color, chance.side, reset);
                    debug!("\x1b[93mMax position size exceeded:\x1b[0m side={}, login_name={}, symbol={}, position_usdt_size={}, max_position_size={}, usdt_order_size={}", side_str, gate_order_manager.login_name, symbol, position_usdt_size, max_position_size, usdt_order_size);
                    return None;
                }
            } else {
                let position_size = gate_order_manager.get_position_size(symbol);
                order_size = order_size.min(position_size.abs());
            }
        }

        if chance.side == "buy" {
            order_size = order_size.abs();
        } else {
            order_size = -order_size.abs();
        }

        let reset = "\x1b[0m";
        let usdt_size_sign = if order_size > 0 { "+" } else { "-" };
        let usdt_size_color = if order_size > 0 {
            "\x1b[92m"
        } else {
            "\x1b[91m"
        };
        let level_color = if chance.level == "limit_open" {
            "\x1b[95m"
        } else if chance.level == "limit_close" {
            "\x1b[96m"
        } else {
            "\x1b[33m"
        };
        let net_positions_usdt_size_color = if total_net_positions_usdt_size > 0.0 {
            "\x1b[92m"
        } else {
            "\x1b[91m"
        };
        let net_positions_usdt_size_str = format!(
            "{}{:.2}{}",
            net_positions_usdt_size_color, total_net_positions_usdt_size, reset
        );
        let symbol_str = format!("{}{}{}", "\x1b[33m", symbol, reset);
        let level_str = format!("{}{}{}", level_color, chance.level, reset);
        let usdt_size_str = format!(
            "{}{}{:.2}{}",
            usdt_size_color, usdt_size_sign, usdt_order_size, reset
        );
        let last_order = LastOrder {
            level: level.clone(),
            side: order_side.clone(),
            price: chance.price,
            timestamp: current_ts_ms,
            contract: symbol.to_string(),
        };

        if let Some(time_gap) = gate_order_manager.get_time_gap_to_oldest_order() {
            if time_gap < 3 * 60 * 1000 {
                if !is_close_order {
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    let last_ms = LAST_TOO_MANY_ORDERS_WARN_MS.load(Ordering::Relaxed);
                    if now_ms.saturating_sub(last_ms) >= 1000 {
                        LAST_TOO_MANY_ORDERS_WARN_MS.store(now_ms, Ordering::Relaxed);
                        warn!(
                            "\x1b[33m[skip order] Too many orders in the last 3 minutes for {}: {} side={} level={}\x1b[0m",
                            symbol,
                            gate_order_manager.login_name,
                            order_side,
                            level,
                        );
                    }
                    return None;
                }
            }
        }

        if level.contains("open") && is_open_order {
            if gate_order_manager.is_limit_open_blocked(symbol) {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                let last_ms = LAST_LIMIT_OPEN_BLOCKED_WARN_MS.load(Ordering::Relaxed);
                if now_ms.saturating_sub(last_ms) >= 1000 {
                    LAST_LIMIT_OPEN_BLOCKED_WARN_MS.store(now_ms, Ordering::Relaxed);
                    warn!(
                        "\x1b[33m[skip order] ⚠️ Limit open blocked for symbol: {} side={} level={}\x1b[0m",
                        symbol,
                        order_side,
                        level,
                    );
                }
                return None;
            }
        }

        if gate_order_manager.is_healthy() == Some(false) {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let last_ms = LAST_NOT_HEALTHY_WARN_MS.load(Ordering::Relaxed);
            if now_ms.saturating_sub(last_ms) >= 1000 {
                LAST_NOT_HEALTHY_WARN_MS.store(now_ms, Ordering::Relaxed);
                warn!(
                    "\x1b[33m[HandleChance] 🚨 Account is not healthy: {} side={} level={}\x1b[0m",
                    gate_order_manager.login_name,
                    order_side,
                    level,
                );
            }
            return None;
        }
        gate_order_manager.on_new_order(symbol, last_order);

        if should_careful_for_too_many_orders {
            warn!(
                "\x1b[33m[HandleChance] ⚠️ Should careful for too many orders: {} side={} level={}\x1b[0m",
                gate_order_manager.login_name,
                order_side,
                level,
            );
            if rand::rng().random_bool(0.5) {
                return None;
            }
        }
        if is_too_many_orders {
            warn!(
                "\x1b[33m[HandleChance] ⚠️ Too many orders: {} side={} level={}\x1b[0m",
                gate_order_manager.login_name,
                order_side,
                level,
            );
            if rand::rng().random_bool(0.5) {
                return None;
            }
        }
        if is_symbol_too_many_orders {
            warn!(
                "\x1b[33m[HandleChance] ⚠️ Too many orders for symbol: {} side={} level={}\x1b[0m",
                symbol,
                order_side,
                level,
            );
            if rand::rng().random_bool(0.5) {
                return None;
            }
        }
        if gate_order_manager.is_recently_too_many_orders() {
            warn!(
                "\x1b[33m[HandleChance] ⚠️ Recently too many orders: {} side={} level={}\x1b[0m",
                gate_order_manager.login_name,
                order_side,
                level,
            );
            if rand::rng().random_bool(0.5) {
                return None;
            }
        }

        // Approved; "[HandleChance] Approved" log is emitted by OrderManagerClient after send_order succeeds.
        Some((order_size, usdt_order_size, bypass_safe_limit_close))
    }
}

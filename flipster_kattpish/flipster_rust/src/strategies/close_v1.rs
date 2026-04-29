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
use std::sync::Arc;

const MAX_POSITION_SIDE_RATIO: f64 = 0.5; // Maximum allowed ratio for net exposure (50%)

pub struct CloseV1StrategyCore {
    handle_chance_manager: Arc<HandleChanceManager>,
}

impl CloseV1StrategyCore {
    pub fn new(leverage: Option<f64>) -> Self {
        Self {
            handle_chance_manager: Arc::new(HandleChanceManager::new(leverage)),
        }
    }
}

impl StrategyCore for CloseV1StrategyCore {
    fn name(&self) -> &str {
        "CloseV1Strategy"
    }

    fn calculate_signal(
        &self,
        snapshot: &SymbolBooktickerSnapshot,
        contract: &GateContract,
        _gate_last_trade: Option<TradeC>,
        _avg_mid_gap: Option<f64>,
        _market_state: &crate::market_watcher::MarketGapState,
    ) -> SignalResult {
        // Entry logic is same as Basic
        signal_calculator::calculate_signal(snapshot, &contract)
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
        _total_net_positions_usdt_size: f64,
        _previous_snapshot_1s: Option<&SymbolBooktickerSnapshot>,
        _previous_snapshot_5s: Option<&SymbolBooktickerSnapshot>,
        _previous_snapshot_10s: Option<&SymbolBooktickerSnapshot>,
        _previous_snapshot_20s: Option<&SymbolBooktickerSnapshot>,
        gate_order_manager: &GateOrderManager,
        _market_state: &crate::market_watcher::MarketGapState,
    ) -> Option<OrderDecision> {
        // V7: Gap Dissolve Exit Logic
        // Entry: Same as Basic
        // Exit: More aggressive exit logic (similar to python v26.x)

        if !signal.has_signal() {
            return None;
        }

        let order_side = signal.order_side.as_deref()?;
        let order_price = signal.order_price?;
        let mut mutated_contract_order_size = contract_order_size;

        let mut binance_data_is_updated = true;

        // Latency checks
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

        // Funding rate check
        let mut only_close = false;

        // If funding rate is too high or too low, close position
        if funding_rate > trade_settings.funding_rate_threshold {
            only_close = true;
        } else if funding_rate < -trade_settings.funding_rate_threshold {
            only_close = true;
        }

        if gate_order_manager.is_close_symbol(symbol) {
            only_close = true;
        }

        let account_position_usdt_size = gate_order_manager.get_net_positions_usdt_size();

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

        // let trade_count = gate_order_manager.get_trade_count(symbol);
        // let profitable_trade_count = gate_order_manager.get_profitable_trade_count(symbol);
        // let profitable_rate = profitable_trade_count as f64 / trade_count as f64;
        // let profit_bp_ema = gate_order_manager.get_profit_bp_ema(symbol);

        // if trade_count > 10 && profitable_rate < 0.5 && usdt_position_size.abs() > 0.0 {
        //     only_close = true;
        // }
        // if trade_count > 10
        //     && profit_bp_ema.is_normal()
        //     && profit_bp_ema < trade_settings.profit_bp_ema_threshold
        // {
        //     only_close = true;
        // }

        // ====== LIMIT OPEN (Entry - Same as Basic) ======
        if signal.mid_gap_chance_bp > trade_settings.mid_gap_bp_threshold
            && signal.orderbook_size > 0.0
            && signal.spread_bp > trade_settings.spread_bp_threshold
            && (signal.orderbook_size as i64) > size_trigger
            && signal.is_binance_valid
            && binance_data_is_updated
            && (signal.orderbook_size as i64) <= max_size_trigger
        {
            level = Some(if only_close {
                "limit_close".to_string()
            } else {
                "limit_open".to_string()
            });
            if is_order_net_position_close_side {
                mutated_contract_order_size = contract_order_size * 2;
            }
        }
        // ====== LIMIT CLOSE (Exit - Combined Narrative Exit) ======
        else {
            // We have a position, check exit conditions
            let has_long_position = usdt_position_size > 0.0;

            // Get gate_mid and binance_mid from signal
            let gate_mid = signal.gate_mid;
            // let bin_mid = signal.binance_mid;

            // 1. Profit: same as basic
            let should_exit_profit = signal.orderbook_size > 0.0
                && signal.spread_bp > trade_settings.spread_bp_threshold
                && (signal.orderbook_size as i64) > close_size_trigger
                && signal.mid_gap_chance_bp > trade_settings.close_raw_mid_profit_bp;

            let close_order_count_exceeded =
                order_count_size.abs() > trade_settings.close_order_count as f64;

            if should_exit_profit
                && close_based_on_binance_ok
                && close_order_count_exceeded
                && binance_data_is_updated
            {
                // Profit Exit -> Limit Close
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
                        level = Some(if only_close {
                            "limit_close".to_string()
                        } else {
                            "limit_open".to_string()
                        });
                    } else if order_price_best_gap < order_price_mid_gap
                        && order_price < gate_mid
                        && is_order_net_position_close_side
                    {
                        level = Some(if only_close {
                            "limit_close".to_string()
                        } else {
                            "limit_open".to_string()
                        });
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
                        level = Some(if only_close {
                            "limit_close".to_string()
                        } else {
                            "limit_open".to_string()
                        });
                    } else if order_price_best_gap < order_price_mid_gap
                        && order_price > gate_mid
                        && is_order_net_position_close_side
                    {
                        level = Some(if only_close {
                            "limit_close".to_string()
                        } else {
                            "limit_open".to_string()
                        });
                    }
                }
            }
        }

        // Return None if level is still None
        let level = level?;

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

        let order_tif = match level.as_str() {
            "limit_open" => trade_settings.limit_open_tif.clone(),
            "limit_close" => trade_settings.limit_close_tif.clone(),
            "market_close" => "ioc".to_string(), // Market order usually implies IOC or FOK
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
        _total_net_positions_usdt_size: f64,
    ) -> Option<(i64, f64, bool)> {
        let symbol = &chance.symbol;
        // let net_positions_usdt_size = gate_order_manager.get_net_positions_usdt_size();
        let order_side = &chance.side;
        let is_buy = order_side == "buy";

        // Check if symbol is in account's symbol list
        if !gate_order_manager.is_account_symbol(symbol) {
            return None;
        }

        let level = &chance.level;
        // Read last_orders quickly and release lock immediately
        let last_order = match gate_order_manager.last_orders.read().get(symbol).cloned() {
            Some(last_order) => Some(last_order),
            None => None,
        };

        // Early position direction check for limit_close
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

        // Calculate current timestamp
        let current_ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        // Time restriction check (same as Python)
        if !chance.bypass_time_restriction {
            if let Some(last) = last_order.clone() {
                if last.level == chance.level
                    && last.side == chance.side
                    && (last.price - chance.price).abs() < 1e-9 // Float comparison
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
            }
        } else {
        }

        // Check futures_account_data
        let account_balance = futures_account_total + futures_account_unrealised_pnl;
        if account_balance <= 0.0 {
            warn!(
                "[HandleChance] Step 3 FAILED: Account balance is 0 for {}",
                gate_order_manager.login_name
            );
            return None;
        }

        // Calculate max_position_size (same as Python)
        let mut max_position_size = if trade_settings.dynamic_max_position_size {
            account_balance * trade_settings.max_position_size_multiplier
        } else {
            trade_settings.max_position_size
        };

        let risk_limit = gate_order_manager.get_risk_limit(symbol);

        let net_positions_usdt_size = gate_order_manager.get_net_positions_usdt_size();
        if net_positions_usdt_size > 0.0 && chance.side == "sell" {
            max_position_size = trade_settings
                .opposite_side_max_position_size
                .unwrap_or(max_position_size * 1.5);
        } else if net_positions_usdt_size < 0.0 && chance.side == "buy" {
            max_position_size = trade_settings
                .opposite_side_max_position_size
                .unwrap_or(max_position_size * 1.5);
        }

        if risk_limit < max_position_size && risk_limit > 0.0 {
            max_position_size = max_position_size.min(risk_limit as f64);
        }

        let mut bypass_safe_limit_close = chance.bypass_safe_limit_close;

        let order_size_opt = gate_order_manager.get_order_size();

        // Position checks
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

            // Reject if trying to close same side position
            if level.contains("close") && same_side {
                return None;
            }

            (pos_size, same_side)
        } else {
            // No position - reject close orders
            if level.contains("close") {
                return None;
            }
            (0.0, false)
        };

        // Additional time restriction checks (same as Python)
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
                            )
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
                            )
                {
                    return None;
                }
            }
        } else {
        }

        // Update last_orders (same as Python)
        // Use try_write with timeout to avoid deadlock

        // Calculate order size
        let mut order_size = chance.size.abs();
        // let contract = gate_order_manager.get_contract(symbol);

        let trade_count = gate_order_manager.get_trade_count(symbol) as f64 + 0.1;
        let profitable_trade_count = gate_order_manager.get_profitable_trade_count(symbol);
        let profitable_rate = profitable_trade_count as f64 / trade_count as f64;
        let profit_bp_ema = gate_order_manager.get_profit_bp_ema(symbol);
        let is_too_many_orders =
            gate_order_manager.is_too_many_orders(trade_settings.too_many_orders_time_gap_ms);

        let is_close_order = level.contains("close") || !is_same_side;

        if trade_count > 5.0 && !is_close_order {
            if profit_bp_ema < trade_settings.profit_bp_ema_threshold {
                order_size = (order_size as f64 * 0.01).round() as i64;
            } else if profitable_rate < 0.3 {
                order_size = (order_size as f64 * 0.01).round() as i64;
            } else if profitable_rate < 0.5 {
                order_size = (order_size as f64 * 0.05).round() as i64;
            } else if profitable_rate < 0.7 {
                order_size = (order_size as f64 * 0.1).round() as i64;
            }
        }

        if is_too_many_orders && !is_close_order {
            if profit_bp_ema < trade_settings.profit_bp_ema_threshold {
                return None;
            } else if profitable_rate < 0.7 {
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

        // let contract = gate_order_manager.get_contract(symbol);

        // if let Some(contract) = contract {
        //     if contract.quanto_multiplier.parse::<f64>().unwrap_or(1.0) > 1.0 {
        //         let quanto_multiplier = contract.quanto_multiplier.parse::<u64>().unwrap_or(1);
        //         order_size = ((order_size / quanto_multiplier as i64) as f64).round() as i64;
        //         order_size = order_size * quanto_multiplier as i64;
        //         order_size = order_size.max(quanto_multiplier as i64);
        //     }
        // } else {
        //     error!(
        //         "[HandleChance] Step 4 FAILED: Contract not found for {}",
        //         symbol
        //     );
        //     return None;
        // }

        // Market order size adjustment
        if level.contains("market") {
            order_size = order_size.min(chance.size);
        }

        // Enforce minimum order size
        let min_order_size = gate_order_manager.get_min_order_size(symbol);
        order_size = order_size.max(min_order_size);

        // Risk management: Net exposure check (same as Python)
        if level.contains("open") {
            // Calculate total position sizes across all symbols
            let mut total_long_usdt = 0.0;
            let mut total_short_usdt = 0.0;

            for sym in gate_order_manager.get_symbols() {
                if !gate_order_manager.is_account_symbol(&sym) {
                    continue;
                }
                let pos_usdt = gate_order_manager.get_usdt_position_size(&sym);
                if pos_usdt > 0.0 {
                    total_long_usdt += pos_usdt;
                } else if pos_usdt < 0.0 {
                    total_short_usdt += pos_usdt.abs();
                }
            }

            // Calculate expected positions after this order
            let (expected_long, expected_short) = if is_buy {
                (total_long_usdt + usdt_order_size, total_short_usdt)
            } else {
                (total_long_usdt, total_short_usdt + usdt_order_size)
            };

            // Calculate net exposure
            let expected_net_exposure = expected_long - expected_short;
            let account_balance_with_leverage =
                account_balance * self.handle_chance_manager.leverage;

            // Block buy orders if net long exposure would exceed threshold
            if is_buy && expected_net_exposure > 0.0 {
                let expected_net_exposure_ratio =
                    expected_net_exposure / account_balance_with_leverage;
                if expected_net_exposure_ratio > MAX_POSITION_SIDE_RATIO {
                    return None;
                }
            }

            // Block sell orders if net short exposure would exceed threshold
            if !is_buy && expected_net_exposure < 0.0 {
                let expected_net_exposure_ratio =
                    expected_net_exposure.abs() / account_balance_with_leverage;
                if expected_net_exposure_ratio > MAX_POSITION_SIDE_RATIO {
                    return None;
                }
            }
        } else {
        }

        // Final position size check (same as Python)
        if gate_order_manager.has_position(symbol) {
            if is_same_side {
                if position_usdt_size.abs() + usdt_order_size.abs() > max_position_size {
                    let side_color = if chance.side == "buy" {
                        "\x1b[92m" // Green
                    } else {
                        "\x1b[91m" // Red
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
            "\x1b[92m" // Green
        } else {
            "\x1b[91m" // Red
        };
        let level_color = if chance.level == "limit_open" {
            //magenta
            "\x1b[95m"
        } else if chance.level == "limit_close" {
            //cyan
            "\x1b[96m"
        } else {
            //yellow
            "\x1b[33m"
        };

        let net_positions_usdt_size_color = if net_positions_usdt_size > 0.0 {
            "\x1b[92m" // Green
        } else {
            "\x1b[91m" // Red
        };
        let net_positions_usdt_size_str = format!(
            "{}{:.2}{}",
            net_positions_usdt_size_color, net_positions_usdt_size, reset
        );

        //symbol yellow
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
                    warn!(
                        "\x1b[33m[skip order] Too many orders in the last 3 minutes for {}: {} side={} level={}\x1b[0m",
                        symbol,
                        gate_order_manager.login_name,
                        order_side,
                        level,
                    );
                    return None;
                }
            }
        }
        if is_too_many_orders {
            warn!(
                "\x1b[33m[HandleChance] Too many orders: {} side={} level={}\x1b[0m",
                gate_order_manager.login_name, order_side, level,
            );
        }
        gate_order_manager.on_new_order(symbol, last_order);

        // Approved; "[HandleChance] Approved" log is emitted by OrderManagerClient after send_order succeeds.
        Some((order_size, usdt_order_size, bypass_safe_limit_close))
    }
}

// Handle chance logic - identical to Python handle_chance
// This implements the complete order validation and risk management logic

use crate::config::TradeSettings;
use crate::gate_order_manager::{GateOrderManager, LastOrder};

use log::{debug, error, info, warn};
use parking_lot::RwLock;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_POSITION_SIDE_RATIO: f64 = 0.5; // Maximum allowed ratio for net exposure (50%)
const DEFAULT_LEVERAGE: f64 = 1000.0; // Default leverage multiplier for risk management calculation
static LAST_NOT_HEALTHY_WARN_MS: AtomicI64 = AtomicI64::new(0);
static LAST_LIMIT_OPEN_BLOCKED_WARN_MS: AtomicI64 = AtomicI64::new(0);
static LAST_TOO_MANY_ORDERS_WARN_MS: AtomicI64 = AtomicI64::new(0);

fn default_bypass_max_position_size() -> bool {
    false
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chance {
    pub symbol: String,
    pub side: String,
    pub price: f64,
    pub tif: String,
    pub level: String,
    pub size: i64,
    pub bypass_time_restriction: bool,
    pub bypass_safe_limit_close: bool,
    #[serde(default = "default_bypass_max_position_size")]
    pub bypass_max_position_size: bool,
}

pub struct HandleChanceManager {
    // Track last orders per symbol per level (same as Python order_manager._last_orders)
    pub leverage: f64, // Leverage multiplier for risk management calculation
    pub last_orders: Arc<RwLock<HashMap<String, LastOrder>>>,
}

impl HandleChanceManager {
    pub fn new(leverage: Option<f64>) -> Self {
        Self {
            leverage: leverage.unwrap_or(DEFAULT_LEVERAGE),
            last_orders: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Handle chance - identical to Python handle_chance
    /// Returns Some((order_size, usdt_order_size, bypass_safe_limit_close)) if order should be placed, None otherwise
    pub fn handle_chance(
        &self,
        chance: &Chance,
        gate_order_manager: &GateOrderManager,
        trade_settings: &TradeSettings,
        futures_account_total: f64,
        futures_account_unrealised_pnl: f64,
    ) -> Option<(i64, f64, bool)> {
        // Returns Some((order_size, usdt_order_size, bypass_safe_limit_close)) if order should be placed, None otherwise
        let symbol = &chance.symbol;

        // Check if symbol is in account's symbol list
        if !gate_order_manager.is_account_symbol(symbol) {
            return None;
        }

        let level = &chance.level;
        // Read last_orders quickly and release lock immediately
        let last_order = match gate_order_manager.last_orders.read().get(symbol).cloned() {
            Some(last_order) => Some(last_order),
            None => return None,
        };

        // Early position direction check for limit_close
        if level == "limit_close" {
            let position_usdt_size = gate_order_manager.get_usdt_position_size(symbol);
            if position_usdt_size >= 0.0 && chance.side == "buy" {
                error!(
                    "[HandleChance] Step 2 FAILED: Long position but close-buy for {}",
                    symbol
                );
                return None;
            } else if position_usdt_size <= 0.0 && chance.side == "sell" {
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
                    && last.timestamp > current_ts_ms - rand::rng().random_range(400..=600)
                {
                    return None;
                }
            }

            if let Some(ref last) = last_order {
                if last.level == "market_open"
                    && last.side == chance.side
                    && last.timestamp > current_ts_ms - 1000
                {
                    return None;
                }

                // Add throttling for market_close to prevent thread explosion during narrative fail
                if last.level == "market_close"
                    && last.side == chance.side
                    && last.timestamp > current_ts_ms - 3000
                {
                    warn!(
                        "[HandleChance] Throttling market_close for {} (3s cooldown)",
                        symbol
                    );
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
        let mut bypass_max_position_size = trade_settings.bypass_max_position_size;

        if risk_limit < max_position_size && risk_limit > 0.0 {
            max_position_size = max_position_size.min(risk_limit as f64);
            bypass_max_position_size = false;
        }

        let mut bypass_safe_limit_close = chance.bypass_safe_limit_close;
        let order_side = &chance.side;
        let is_buy = order_side == "buy";

        // Read order_size once for reuse (reduce try_read frequency)
        let order_size_opt = gate_order_manager.get_order_size();

        // Position checks
        let (position_usdt_size, is_same_side) = if gate_order_manager.has_position(symbol) {
            let pos_size = gate_order_manager.get_usdt_position_size(symbol);
            let same_side = (pos_size > 0.0 && is_buy) || (pos_size < 0.0 && !is_buy);

            // Set bypass_safe_limit_close if position is large and opposite side
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
            if let Some(last) = self.last_orders.read().get(symbol).cloned() {
                if last.level == "limit_open"
                    && is_same_side
                    && last.side == chance.side
                    && last.timestamp > current_ts_ms - rand::rng().random_range(300..=600)
                {
                    return None;
                }
            }
        } else {
        }

        // Update last_orders (same as Python)
        // Use try_write with timeout to avoid deadlock

        // {
        //     // Try to acquire write lock with timeout (parking_lot supports try_write_for)
        //     match gate_order_manager.last_orders.try_write_for(Duration::from_millis(10)) {
        //         Some(mut last_orders_write) => {
        //             let symbol_orders = last_orders_write
        //                 .entry(symbol.clone())
        //                 .or_insert_with(|| crate::gate_order_manager::LastOrder {
        //                     level: level.clone(),
        //                     side: order_side.clone(),
        //                     price: chance.price,
        //                     timestamp: current_ts_ms,
        //                 });
        //             symbol_orders.insert(
        //                 level.clone(),
        //                 crate::gate_order_manager::LastOrder {
        //                     level: level.clone(),
        //                     side: order_side.clone(),
        //                     price: chance.price,
        //                     timestamp: current_ts_ms,
        //                 },
        //             );
        //         }
        //         None => {
        //             return None;
        //         }
        //     }
        // }

        // Calculate order size
        let mut order_size = chance.size;

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

        // Adjust order_size if needed (reuse order_size_opt)
        if order_size == 0 {
            if let Some(calculated_usdt) = order_size_opt {
                order_size = gate_order_manager.get_size_from_usdt_amount(symbol, calculated_usdt);
            }
        }
        order_size = order_size.abs();

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
            let account_balance_with_leverage = account_balance * self.leverage;

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
                    if !bypass_max_position_size {
                        info!("\x1b[93mMax position size exceeded:\x1b[0m login_name={}, symbol={}, position_usdt_size={}, max_position_size={}, usdt_order_size={}", gate_order_manager.login_name, symbol, position_usdt_size, max_position_size, usdt_order_size);
                        return None;
                    } else {
                        debug!("\x1b[93mBypassing max position:\x1b[0m login_name={}, symbol={}, position_usdt_size={}, max_position_size={}, usdt_order_size={}", gate_order_manager.login_name, symbol, position_usdt_size, max_position_size, usdt_order_size);
                    }
                }
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

        //symbol yellow
        let symbol_str = format!("{}{}{}", "\x1b[33m", symbol, reset);
        let level_str = format!("{}{}{}", level_color, chance.level, reset);
        let usdt_size_str = format!(
            "{}{}{:.2}{}",
            usdt_size_color, usdt_size_sign, usdt_order_size, reset
        );

        // update last_order
        gate_order_manager.last_orders.write().insert(
            symbol.clone(),
            crate::gate_order_manager::LastOrder {
                level: chance.level.clone(),
                side: chance.side.clone(),
                price: chance.price,
                timestamp: current_ts_ms,
                contract: symbol.clone(),
            },
        );

        // Reached final approval point
        info!(
            "\x1b[92m[HandleChance] Approved:\x1b[0m | {} | {} | {}",
            symbol_str, usdt_size_str, level_str
        );
        Some((order_size, usdt_order_size, bypass_safe_limit_close))
    }
}

/// Handle chance v11 - same logic as v11.rs handle_chance.
/// Returns Some((order_size, usdt_order_size, bypass_safe_limit_close)) if order should be placed, None otherwise.
pub fn handle_change_v11(
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

    let level = if trade_settings.max_position_size > 0.0 {
        &chance.level
    } else {
        "limit_close"
    };

    let last_order = gate_order_manager.last_orders.read().get(symbol).cloned();

    let position_size = gate_order_manager.get_position_size(symbol);

    if level == "limit_close" {
        let min_order_size = gate_order_manager.get_min_order_size(symbol);

        if position_size.abs() < min_order_size {
            return None;
        }

        if position_size > 0 && chance.side == "buy" {
            return None;
        } else if position_size < 0 && chance.side == "sell" {
            return None;
        }
    }

    let current_ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    if !chance.bypass_time_restriction {
        if let Some(ref last) = last_order {
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

        if let Some(ref last) = last_order {
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
        (0.0, true)
    };

    if !chance.bypass_time_restriction {
        if let Some(ref last) = last_order {
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

        if let Some(ref last) = last_order {
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
    }

    let mut order_size =
        gate_order_manager.get_size_from_usdt_amount(symbol, trade_settings.order_size);
    let is_open_order = is_same_side || !gate_order_manager.has_position(symbol);

    if order_size == 0 {
        if let Some(calculated_usdt) = order_size_opt {
            order_size = gate_order_manager.get_size_from_usdt_amount(symbol, calculated_usdt);
        }
    }

    if level.contains("market") {
        order_size = order_size.min(chance.size);
    }

    let min_order_size = gate_order_manager.get_min_order_size(symbol);
    order_size = order_size.max(min_order_size);

    let is_net_positions_usdt_size_close_side =
        if total_net_positions_usdt_size >= 0.0 && chance.side == "sell" {
            true
        } else if total_net_positions_usdt_size <= 0.0 && chance.side == "buy" {
            true
        } else {
            false
        };

    if trade_settings.only_handle_net_position_size_close_side
        && !is_net_positions_usdt_size_close_side
        && is_open_order
    {
        return None;
    }

    if !is_net_positions_usdt_size_close_side {
        order_size /= 2;
    }

    let usdt_order_size_abs = if order_size != 0 {
        gate_order_manager.get_usdt_amount_from_size(symbol, order_size)
    } else {
        trade_settings.order_size
    };

    if is_open_order {
        if position_usdt_size.abs() + usdt_order_size_abs.abs() * 1.5 > max_position_size {
            let side_color = if chance.side == "buy" {
                "\x1b[92m"
            } else {
                "\x1b[91m"
            };
            let reset = "\x1b[0m";
            let side_str = format!("{}{}{}", side_color, chance.side, reset);
            debug!("\x1b[93mMax position size exceeded:\x1b[0m side={}, login_name={}, symbol={}, position_usdt_size={}, max_position_size={}, usdt_order_size={}", side_str, gate_order_manager.login_name, symbol, position_usdt_size, max_position_size, usdt_order_size_abs);
            return None;
        }
    } else {
        order_size = gate_order_manager
            .get_size_from_usdt_amount(
                symbol,
                trade_settings
                    .close_order_size
                    .unwrap_or(trade_settings.order_size),
            )
            .abs()
            .min(position_size.abs())
            .max(min_order_size);
    }

    if chance.side == "buy" {
        order_size = order_size.abs();
    } else {
        order_size = -order_size.abs();
    }

    if is_open_order && !is_net_positions_usdt_size_close_side {
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
                    chance.side,
                    chance.level,
                );
            }
            return None;
        }
    }

    let last_ws_order = if let Some(last_ws_orders) = gate_order_manager.last_ws_orders.try_read() {
        last_ws_orders.get(symbol).cloned()
    } else {
        return None;
    };

    if let Some(last_ws_order) = last_ws_order {
        let is_more_expensive_order = if chance.side == "buy" {
            chance.price >= last_ws_order.price
        } else {
            chance.price <= last_ws_order.price
        };
        let is_big_close_order =
            !is_open_order && position_usdt_size.abs() >= trade_settings.max_position_size / 2.0;

        if current_ts_ms - last_ws_order.timestamp
            < trade_settings.wait_time_same_or_more_expensive_order_ms
            && is_more_expensive_order
        {
            return None;
        }
        if current_ts_ms - last_ws_order.timestamp
            < trade_settings.wait_time_not_close_big_same_or_more_expensive_order_ms
            && is_more_expensive_order
            && !is_big_close_order
            && !is_net_positions_usdt_size_close_side
        {
            return None;
        }
        if gate_order_manager.is_ws_rate_limit_dangerous() {
            if current_ts_ms - last_ws_order.timestamp < 2000
                && is_more_expensive_order
                && !is_big_close_order
                && !is_net_positions_usdt_size_close_side
            {
                return None;
            } else if current_ts_ms - last_ws_order.timestamp < 500 {
                return None;
            }
        }
    }

    gate_order_manager.last_ws_orders.write().insert(
        symbol.to_string(),
        LastOrder {
            level: chance.level.clone(),
            side: chance.side.clone(),
            price: chance.price,
            timestamp: current_ts_ms,
            contract: symbol.to_string(),
        },
    );

    Some((order_size, usdt_order_size_abs, bypass_safe_limit_close))
}

pub fn handle_chance_v11_total_net_position_safe(
    chance: &Chance,
    gate_order_manager: &GateOrderManager,
    trade_settings: &TradeSettings,
    futures_account_total: f64,
    futures_account_unrealised_pnl: f64,
    total_net_positions_usdt_size: f64,
) -> Option<(i64, f64, bool)> {
    let is_net_positions_usdt_size_close_side =
        if total_net_positions_usdt_size > 0.0 && chance.side == "sell" {
            true
        } else if total_net_positions_usdt_size < 0.0 && chance.side == "buy" {
            true
        } else {
            false
        };

    if !is_net_positions_usdt_size_close_side {
        return None;
    }
    handle_change_v11(
        chance,
        gate_order_manager,
        trade_settings,
        futures_account_total,
        futures_account_unrealised_pnl,
        total_net_positions_usdt_size,
    )
}

pub fn handle_chance_v8(
    chance: &Chance,
    gate_order_manager: &GateOrderManager,
    trade_settings: &TradeSettings,
    futures_account_total: f64,
    futures_account_unrealised_pnl: f64,
    total_net_positions_usdt_size: f64,
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

        if position_size >= 0 && chance.side == "buy" {
            error!(
                "[HandleChance] Step 2 FAILED: Long position but close-buy for {}",
                symbol
            );
            return None;
        } else if position_size <= 0 && chance.side == "sell" {
            error!(
                "[HandleChance] Step 2 FAILED: Short position but close-sell for {}",
                symbol
            );
            return None;
        }
    } else {
    }

    let is_total_net_positions_usdt_size_close_side =
        if total_net_positions_usdt_size >= 0.0 && chance.side == "sell" {
            true
        } else if total_net_positions_usdt_size <= 0.0 && chance.side == "buy" {
            true
        } else {
            false
        };
    // Read order_size once for reuse (reduce try_read frequency)
    let order_size_opt = gate_order_manager.get_order_size();
    let mut bypass_safe_limit_close = chance.bypass_safe_limit_close;

    // Position checks
    let (position_usdt_size, is_same_side) = if gate_order_manager.has_position(symbol) {
        let pos_size = gate_order_manager.get_usdt_position_size(symbol);
        let same_side = (pos_size > 0.0 && is_buy) || (pos_size < 0.0 && !is_buy);

        // Set bypass_safe_limit_close if position is large and opposite side
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

    let usdt_position_size = gate_order_manager.get_usdt_position_size(symbol);
    let mut is_close_order = false;

    if usdt_position_size > 0.0 && !is_buy {
        is_close_order = true;
    } else if usdt_position_size < 0.0 && is_buy {
        is_close_order = true;
    }

    let is_open_order = is_same_side || !gate_order_manager.has_position(symbol);

    if trade_settings.only_handle_net_position_size_close_side
        && !is_total_net_positions_usdt_size_close_side
        && is_open_order
    {
        return None;
    }

    let too_few_orders = !gate_order_manager.is_too_many_orders(30 * 1000 * 60);

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
            if last.level == "market_close" && last.timestamp > current_ts_ms - 1000 {
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

    // Update last_orders (same as Python)
    // Use try_write with timeout to avoid deadlock

    // Calculate order size
    let mut order_size = chance.size.abs();
    // let contract = gate_order_manager.get_contract(symbol);

    let trade_count = gate_order_manager.get_trade_count(symbol) as f64 + 0.1;
    let profitable_trade_count = gate_order_manager.get_profitable_trade_count(symbol);
    // let profitable_rate = profitable_trade_count as f64 / trade_count as f64;
    let profit_bp_ema = gate_order_manager.get_profit_bp_ema(symbol);

    if trade_count > 5.0 && !is_close_order && !is_total_net_positions_usdt_size_close_side {
        if profit_bp_ema < trade_settings.profit_bp_ema_threshold {
            if should_careful_for_too_many_orders {
                return None;
            } else if is_symbol_too_many_orders {
                return None;
            }
            order_size = (order_size as f64 * 0.01).round() as i64;
        }
        //  else if profitable_rate < 0.2 {
        //     order_size = (order_size as f64 * 0.01).round() as i64;
        // } else if profitable_rate < 0.4 {
        //     order_size = (order_size as f64 * 0.05).round() as i64;
        // } else if profitable_rate < trade_settings.succes_threshold {
        //     order_size = (order_size as f64 * 0.1).round() as i64;
        // }
    }

    if is_too_many_orders && !is_close_order && !is_total_net_positions_usdt_size_close_side {
        if profit_bp_ema < trade_settings.profit_bp_ema_threshold {
            return None;
        }
        //  else if profitable_rate < trade_settings.succes_threshold {
        //     return None;
        // }
    }

    // if level.contains("close") || is_close_order {
    //     let close_order_size = if level.contains("market") {
    //         trade_settings
    //             .market_close_order_size
    //             .or(trade_settings.close_order_size)
    //     } else {
    //         trade_settings.close_order_size
    //     };
    //     if let Some(close_usdt) = close_order_size {
    //         usdt_order_size = close_usdt;

    //         if (usdt_position_size / 5.0).abs() > close_usdt {
    //             usdt_order_size = (usdt_position_size / 5.0).abs();
    //         }
    //         order_size = gate_order_manager.get_size_from_usdt_amount(symbol, usdt_order_size);
    //     } else {
    //         usdt_order_size = trade_settings.order_size;
    //         if (usdt_position_size / 5.0).abs() > trade_settings.order_size {
    //             usdt_order_size = (usdt_position_size / 5.0).abs();
    //         }
    //         order_size = gate_order_manager.get_size_from_usdt_amount(symbol, usdt_order_size);
    //     }
    // }

    // Adjust order_size if needed (reuse order_size_opt)
    if order_size == 0 {
        if let Some(calculated_usdt) = order_size_opt {
            order_size = gate_order_manager.get_size_from_usdt_amount(symbol, calculated_usdt);
        }
    }
    if !is_total_net_positions_usdt_size_close_side {
        order_size /= 2;
    }
    // Immutable from here (shadow with final value)
    order_size = order_size.abs().min(chance.size.abs() as i64);

    let is_closing_big_position = if let Some(close_order_size) = trade_settings.close_order_size {
        position_usdt_size.abs() > close_order_size.abs() && is_close_order
    } else {
        false
    };

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

    let usdt_order_size_abs = if order_size != 0 {
        gate_order_manager.get_usdt_amount_from_size(symbol, order_size)
    } else {
        trade_settings.order_size
    };

    // let current_time_ms = SystemTime::now()
    //     .duration_since(UNIX_EPOCH)
    //     .unwrap()
    //     .as_millis() as i64;

    // Risk management: Net exposure check (same as Python)
    // if level.contains("open") {
    //     // Calculate total position sizes across all symbols
    //     let mut total_long_usdt = 0.0;
    //     let mut total_short_usdt = 0.0;

    //     for sym in gate_order_manager.get_symbols() {
    //         if !gate_order_manager.is_account_symbol(&sym) {
    //             continue;
    //         }
    //         let pos_usdt = gate_order_manager.get_usdt_position_size(&sym);
    //         if pos_usdt > 0.0 {
    //             total_long_usdt += pos_usdt;
    //         } else if pos_usdt < 0.0 {
    //             total_short_usdt += pos_usdt.abs();
    //         }
    //     }

    //     // Calculate expected positions after this order
    //     let (expected_long, expected_short) = if is_buy {
    //         (total_long_usdt + usdt_order_size, total_short_usdt)
    //     } else {
    //         (total_long_usdt, total_short_usdt + usdt_order_size)
    //     };

    //     // Calculate net exposure
    //     let expected_net_exposure = expected_long - expected_short;
    //     let account_balance_with_leverage =
    //         account_balance * self.handle_chance_manager.leverage;

    //     // Block buy orders if net long exposure would exceed threshold
    //     if is_buy && expected_net_exposure > 0.0 {
    //         let expected_net_exposure_ratio =
    //             expected_net_exposure / account_balance_with_leverage;
    //         if expected_net_exposure_ratio > MAX_POSITION_SIDE_RATIO {
    //             if current_time_ms - LAST_NET_LONG_EXPOSURE_WARN_MS.load(Ordering::Relaxed)
    //                 >= 1000
    //             {
    //                 warn!(
    //                     "\x1b[33m[HandleChance] ⚠️ Net long exposure would exceed threshold: {}\x1b[0m",
    //                     gate_order_manager.login_name
    //                 );
    //                 LAST_NET_LONG_EXPOSURE_WARN_MS.store(current_time_ms, Ordering::Relaxed);
    //             }
    //             return None;
    //         }
    //     }

    //     // Block sell orders if net short exposure would exceed threshold
    //     if !is_buy && expected_net_exposure < 0.0 {
    //         let expected_net_exposure_ratio =
    //             expected_net_exposure.abs() / account_balance_with_leverage;
    //         if expected_net_exposure_ratio > MAX_POSITION_SIDE_RATIO {
    //             if current_time_ms - LAST_NET_SHORT_EXPOSURE_WARN_MS.load(Ordering::Relaxed)
    //                 >= 1000
    //             {
    //                 warn!(
    //                     "\x1b[33m[HandleChance] ⚠️ Net short exposure would exceed threshold: {}\x1b[0m",
    //                     gate_order_manager.login_name
    //                 );
    //                 LAST_NET_SHORT_EXPOSURE_WARN_MS.store(current_time_ms, Ordering::Relaxed);
    //             }
    //             return None;
    //         }
    //     }
    // } else {
    // }
    if usdt_order_size_abs < trade_settings.filter_order_size_on_volatility_usdt_threshold
        && !too_few_orders
    {
        return None;
    }

    if is_too_many_orders {
        max_position_size = max_position_size * 0.5;
    }

    // Final position size check (same as Python)
    if gate_order_manager.has_position(symbol) {
        if is_same_side {
            if position_usdt_size.abs() + usdt_order_size_abs.abs() > max_position_size {
                let side_color = if chance.side == "buy" {
                    "\x1b[92m" // Green
                } else {
                    "\x1b[91m" // Red
                };
                let reset = "\x1b[0m";
                let side_str = format!("{}{}{}", side_color, chance.side, reset);
                debug!("\x1b[93mMax position size exceeded:\x1b[0m side={}, login_name={}, symbol={}, position_usdt_size={}, max_position_size={}, usdt_order_size={}", side_str, gate_order_manager.login_name, symbol, position_usdt_size, max_position_size, usdt_order_size_abs);
                return None;
            }
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

    let net_positions_usdt_size_color = if total_net_positions_usdt_size > 0.0 {
        "\x1b[92m" // Green
    } else {
        "\x1b[91m" // Red
    };
    let net_positions_usdt_size_str = format!(
        "{}{:.2}{}",
        net_positions_usdt_size_color, total_net_positions_usdt_size, reset
    );

    //symbol yellow
    let symbol_str = format!("{}{}{}", "\x1b[33m", symbol, reset);
    let level_str = format!("{}{}{}", level_color, chance.level, reset);
    let usdt_size_str = format!(
        "{}{}{:.2}{}",
        usdt_size_color, usdt_size_sign, usdt_order_size_abs, reset
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
                            "\x1b[33m[skip order] Too many orders in the last 3 minutes for {}: {}\x1b[0m",
                            symbol,
                            gate_order_manager.login_name
                        );
                }
                return None;
            }
        }
    }

    if is_open_order && !is_total_net_positions_usdt_size_close_side {
        if gate_order_manager.is_limit_open_blocked(symbol) {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let last_ms = LAST_LIMIT_OPEN_BLOCKED_WARN_MS.load(Ordering::Relaxed);
            if now_ms.saturating_sub(last_ms) >= 1000 {
                LAST_LIMIT_OPEN_BLOCKED_WARN_MS.store(now_ms, Ordering::Relaxed);
                warn!(
                    "\x1b[33m[skip order] ⚠️ Limit open blocked for symbol: {}\x1b[0m",
                    symbol
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
                "\x1b[33m[HandleChance] 🚨 Account is not healthy: {}\x1b[0m",
                gate_order_manager.login_name
            );
        }
        return None;
    }
    gate_order_manager.on_new_order(symbol, last_order);

    if should_careful_for_too_many_orders
        && !is_closing_big_position
        && !is_total_net_positions_usdt_size_close_side
    {
        warn!(
            "\x1b[33m[HandleChance] ⚠️ Should careful for too many orders: {}\x1b[0m",
            gate_order_manager.login_name
        );
        // drop chance by 50%
        if rand::rng().random_bool(0.5) {
            return None;
        }
    }

    if is_too_many_orders && !is_closing_big_position {
        warn!(
            "\x1b[33m[HandleChance] ⚠️ Too many orders: {}\x1b[0m",
            gate_order_manager.login_name
        );
        // drop chance by 50%
        if rand::rng().random_bool(0.5) {
            return None;
        }
    }
    if is_symbol_too_many_orders && !is_closing_big_position {
        warn!(
            "\x1b[33m[HandleChance] ⚠️ Too many orders for symbol: {}\x1b[0m",
            symbol
        );
        // drop chance by 50%
        if rand::rng().random_bool(0.5) {
            return None;
        }
    }
    if gate_order_manager.is_recently_too_many_orders() && !is_closing_big_position {
        warn!(
            "\x1b[33m[HandleChance] ⚠️ Recently too many orders: {}\x1b[0m",
            gate_order_manager.login_name
        );
        // drop chance by 50%
        if rand::rng().random_bool(0.5) {
            return None;
        }
    }

    // Approved; "[HandleChance] Approved" log is emitted by OrderManagerClient after send_order succeeds.
    Some((order_size, usdt_order_size_abs, bypass_safe_limit_close))
}

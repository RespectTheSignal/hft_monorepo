// Order decision logic
// Determines whether to place an order based on signal, settings, and market conditions

use crate::config::TradeSettings;
use crate::data_cache::SymbolBooktickerSnapshot;
use crate::signal_calculator::SignalResult;

#[derive(Debug, Clone)]
pub struct OrderDecision {
    pub level: String,     // "limit_open" or "limit_close"
    pub order_tif: String, // "fok", "ioc", "gtc", etc.
    pub order_size: i64,   // Contract order size
    pub only_close: bool,  // True if funding rate prevents opening
    pub order_price: Option<f64>,
    pub memo: Option<String>,
}

/// Decide whether to place an order based on signal and market conditions
///
/// Returns None if order should not be placed, Some(OrderDecision) if order should be placed
pub fn decide_order(
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
    usdt_position_size: f64, // Current position size in USDT (positive = long, negative = short)
    _previous_snapshot_1s: Option<&SymbolBooktickerSnapshot>,
    _previous_snapshot_5s: Option<&SymbolBooktickerSnapshot>,
    _previous_snapshot_10s: Option<&SymbolBooktickerSnapshot>,
    _previous_snapshot_20s: Option<&SymbolBooktickerSnapshot>,
) -> Option<OrderDecision> {
    // Early return if no signal
    if !signal.has_signal() {
        return None;
    }

    let order_side = signal.order_side.as_deref()?;
    let order_price = signal.order_price?;

    // Latency checks - reject if data is too old
    if (current_time_ms - gate_bt_server_time) > trade_settings.gate_last_book_ticker_latency_ms {
        return None;
    }
    if let Some(binance_bt_server_time) = binance_bt_server_time {
        if (current_time_ms - binance_bt_server_time)
            > trade_settings.binance_last_book_ticker_latency_ms
        {
            return None;
        }
    } else {
        return None;
    }

    if (current_time_ms - gate_web_bt_server_time)
        > trade_settings.gate_last_webbook_ticker_latency_ms
    {
        return None;
    }

    // Funding rate check - determine if we should only close positions
    let mut only_close = false;
    if order_side == "buy" && funding_rate > trade_settings.funding_rate_threshold {
        only_close = true; // Funding rate too high, don't open long positions
    } else if order_side == "sell" && funding_rate < -trade_settings.funding_rate_threshold {
        only_close = true; // Funding rate too negative, don't open short positions
    }

    // Determine order level (limit_open or limit_close)
    let mut level = None;

    let close_based_on_binance_ok =
        if let Some(binance_mid_gap_chance_bp) = signal.binance_mid_gap_chance_bp {
            binance_mid_gap_chance_bp > 0.0
        } else {
            false
        };

    // Limit open condition
    // Check if mid_gap is large enough, spread is sufficient, orderbook size is in range, and Binance validates
    if signal.mid_gap_chance_bp > trade_settings.mid_gap_bp_threshold
        && signal.orderbook_size > 0.0
        && signal.spread_bp > trade_settings.spread_bp_threshold
        && (signal.orderbook_size as i64) > size_trigger
        && signal.is_binance_valid
        && (signal.orderbook_size as i64) <= max_size_trigger
    {
        // If funding rate prevents opening, convert to limit_close
        level = Some(if only_close {
            "limit_close".to_string()
        } else {
            "limit_open".to_string()
        });
    }
    // Limit close condition
    // Check if mid_gap is sufficient for closing, spread is good, orderbook size is large enough, and we have open orders
    else if signal.mid_gap_chance_bp > trade_settings.close_raw_mid_profit_bp
        && signal.orderbook_size > 0.0
        && signal.spread_bp > trade_settings.spread_bp_threshold
        && (signal.orderbook_size as i64) > close_size_trigger
        && order_count_size.abs() > trade_settings.close_order_count as f64
        && close_based_on_binance_ok
    {
        level = Some("limit_close".to_string());
    }

    let level = level?;

    // Position check for limit_close orders
    // Don't close if we're trying to close in the wrong direction
    if level == "limit_close" {
        if usdt_position_size > 0.0 && order_side == "buy" {
            // Long position, trying to close with buy order (wrong direction)
            return None;
        }
        if usdt_position_size < 0.0 && order_side == "sell" {
            // Short position, trying to close with sell order (wrong direction)
            return None;
        }
    }

    // Determine order TIF (Time In Force)
    let order_tif = match level.as_str() {
        "limit_open" => trade_settings.limit_open_tif.clone(),
        "limit_close" => trade_settings.limit_close_tif.clone(),
        _ => "fok".to_string(), // Default to FOK
    };

    Some(OrderDecision {
        level,
        order_tif,
        order_size: contract_order_size,
        only_close,
        order_price: Some(order_price),
        memo: None,
    })
}

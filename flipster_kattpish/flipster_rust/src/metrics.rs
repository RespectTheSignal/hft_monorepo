// Metrics printing - identical to Python print_order, print_position, print_trade

use crate::gate_order_manager::{GateContract, GateOpenOrder, GateOrderManager};
use crate::plog;

pub fn timestamp_to_str(timestamp: i64) -> String {
    let timestamp_ms = timestamp % 1000;
    let timestamp_sec = timestamp / 1000;
    let timestamp_min = timestamp_sec / 60;
    let timestamp_hour = timestamp_min / 60;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        timestamp_hour % 24,
        timestamp_min % 60,
        timestamp_sec % 60,
        timestamp_ms
    )
}

/// Print order information (same as Python print_order)
pub fn print_order(order: &GateOpenOrder, login_name: &str, order_manager: &GateOrderManager) {
    if crate::log::is_quiet() {
        return;
    }
    let is_ws = order.text != "web";
    let size = order.size;
    let left = order.left;
    let price = order.price;
    let reduce_only = order.is_reduce_only;
    let order_type = &order.tif;
    let contract = &order.contract;
    let background = if is_ws { "\x1b[44m" } else { "" }; // Blue background for WS
    let reset = "\x1b[0m";

    let side_str = if size > 0 {
        "BUY"
    } else if size < 0 {
        "SELL"
    } else {
        "CLOSE"
    };

    // Same as Python: long_color = Back.BLACK + Fore.LIGHTGREEN_EX, short_color = Back.BLACK + Fore.LIGHTRED_EX, closed_color = Back.BLACK + Fore.YELLOW
    let side_color = if side_str == "BUY" {
        "\x1b[40m\x1b[92m" // Back.BLACK + Fore.LIGHTGREEN_EX
    } else if side_str == "SELL" {
        "\x1b[40m\x1b[91m" // Back.BLACK + Fore.LIGHTRED_EX
    } else {
        "\x1b[40m\x1b[33m" // Back.BLACK + Fore.YELLOW
    };

    let reduce_only_bg = if reduce_only { "\x1b[43m" } else { background }; // Yellow background if reduce_only

    let order_type_upper = order_type.to_uppercase();
    let order_type_color = if order_type_upper == "LIMIT" {
        background
    } else {
        "\x1b[35m" // Magenta
    };

    let symbol_short = contract.replace("_USDT", "");

    let left_usdt_amount = order_manager.get_usdt_amount_from_size(contract.as_str(), left);

    // let usdt_amount_abs = usdt_amount.abs();

    let usdt_amount_color = if left_usdt_amount > 0.0 {
        "\x1b[92m" // Light green
    } else if left_usdt_amount < 0.0 {
        "\x1b[91m" // Light red
    } else {
        "\x1b[37m" // White
    };

    let update_time = order.update_time;

    // format update_time to HH:MM:SS:MS
    let update_time_str = format!(
        "{:02}:{:02}:{:02}.{:03}",
        update_time / 3600000 % 24,
        (update_time / 60000) % 60,
        (update_time / 1000) % 60,
        update_time % 1000
    );

    plog!(
        "{}{:>10} | {}{:>3}{} | {}{:>+12.2}{} | {:>8} | {}{:>6}{} | {:>12} | {:>14} | {:>12} |",
        background,
        symbol_short,
        order_type_color,
        order_type_upper,
        reset,
        usdt_amount_color,
        left_usdt_amount,
        reset,
        price,
        reduce_only_bg,
        reduce_only,
        reset,
        login_name,
        update_time_str,
        order.id
    );
}

/// Print position information (same as Python print_position)
pub fn print_position(
    symbol: &str,
    previous_size: i64,
    current_size: i64,
    realised_pnl: f64,
    last_close_pnl: f64,
    history_pnl: f64,
    login_name: &str,
    order_manager: &GateOrderManager,
) {
    if crate::log::is_quiet() {
        return;
    }
    let order_side = if current_size > previous_size {
        "LONG"
    } else if current_size < previous_size {
        "SHORT"
    } else {
        "CLOSED"
    };

    let is_buy = order_side == "LONG";
    let is_sell = order_side == "SHORT";

    let mut order_side_str = order_side.to_string();
    if current_size.abs() < previous_size.abs() {
        if order_side == "LONG" {
            order_side_str = "CLOSE_SHORT".to_string();
        } else if order_side == "SHORT" {
            order_side_str = "CLOSE_LONG".to_string();
        }
    }

    // Same as Python: long_color = Back.BLACK + Fore.LIGHTGREEN_EX, short_color = Back.BLACK + Fore.LIGHTRED_EX, closed_color = Back.BLACK + Fore.YELLOW
    let side_color = if is_buy {
        "\x1b[40m\x1b[92m" // Back.BLACK + Fore.LIGHTGREEN_EX
    } else if is_sell {
        "\x1b[40m\x1b[91m" // Back.BLACK + Fore.LIGHTRED_EX
    } else {
        "\x1b[40m\x1b[33m" // Back.BLACK + Fore.YELLOW
    };

    // Same as Python: positive_color = Fore.LIGHTGREEN_EX + Back.BLACK, negative_color = Fore.LIGHTRED_EX + Back.BLACK
    let realised_pnl_color = if realised_pnl > 0.0 {
        "\x1b[92m\x1b[40m" // Fore.LIGHTGREEN_EX + Back.BLACK
    } else if realised_pnl < 0.0 {
        "\x1b[91m\x1b[40m" // Fore.LIGHTRED_EX + Back.BLACK
    } else {
        "\x1b[40m" // Back.BLACK
    };

    let last_close_pnl_color = if last_close_pnl > 0.0 {
        "\x1b[92m\x1b[40m"
    } else if last_close_pnl < 0.0 {
        "\x1b[91m\x1b[40m"
    } else {
        "\x1b[40m"
    };

    let history_pnl_color = if history_pnl > 0.0 {
        "\x1b[92m\x1b[40m"
    } else if history_pnl < 0.0 {
        "\x1b[91m\x1b[40m"
    } else {
        "\x1b[40m"
    };

    let reset = "\x1b[0m";
    let bg_black = "\x1b[40m";
    let yellow = "\x1b[33m";

    let symbol_short = symbol.replace("_USDT", "");
    let size_change = current_size - previous_size;

    let previous_usdt_size = order_manager.get_usdt_amount_from_size(symbol, previous_size);
    let current_usdt_size = order_manager.get_usdt_amount_from_size(symbol, current_size);

    if previous_size != current_size {
        plog!(
            "Position {}: {:.2} -> {:.2} (Change: {}{:+.4}{})",
            symbol,
            previous_usdt_size,
            current_usdt_size,
            side_color,
            size_change,
            reset
        );
    }

    plog!(
        "{}{}{}{}{}",
        bg_black,
        yellow,
        "=".repeat(112),
        reset,
        reset
    );
    plog!(
        "{}{:>10} | {:>12} | {:>16} | {:>12} | {:>18} | {:>12} | {:>12} |{}",
        bg_black,
        "Contract",
        "Side",
        "Size",
        "Realized PnL",
        "Last Close PnL",
        "History PnL",
        "Account ID",
        reset
    );
    plog!("{}{}{}", bg_black, "-".repeat(112), reset);
    plog!(
        "{}{:>10}{} | {}{:>12}{} | {:>16} | {}{:>12.4}{} | {}{:>18.4}{} | {}{:>12.4}{} | {:>12} |",
        bg_black,
        symbol_short,
        reset,
        side_color,
        order_side_str,
        reset,
        format!("{:.2} -> {:.2}", previous_usdt_size, current_usdt_size),
        realised_pnl_color,
        realised_pnl,
        reset,
        last_close_pnl_color,
        last_close_pnl,
        reset,
        history_pnl_color,
        history_pnl,
        reset,
        login_name
    );
    plog!(
        "{}{}{}{}{}\n",
        bg_black,
        yellow,
        "=".repeat(112),
        reset,
        reset
    );
}

/// Print trade information (same as Python print_trade)
pub fn print_trade(
    symbol: &str,
    size: i64,
    price: f64,
    fee: f64,
    last_book_ticker_bid: Option<f64>,
    last_book_ticker_ask: Option<f64>,
    login_name: &str,
    contract: Option<&GateContract>,
    success_rate: f64,
    profitable_rate: f64,
    profit_bp_ema: f64,
    create_time_ms: i64,
) {
    if crate::log::is_quiet() {
        return;
    }
    let side_str = if size > 0 { "BUY" } else { "SELL" };
    // Same as Python: long_color = Back.BLACK + Fore.LIGHTGREEN_EX, short_color = Back.BLACK + Fore.LIGHTRED_EX
    let side_color = if side_str == "BUY" {
        "\x1b[40m\x1b[92m" // Back.BLACK + Fore.LIGHTGREEN_EX
    } else {
        "\x1b[40m\x1b[91m" // Back.BLACK + Fore.LIGHTRED_EX
    };

    let symbol_short = symbol.replace("_USDT", "");
    let reset = "\x1b[0m";
    let bg_black = "\x1b[40m";

    if let (Some(bid), Some(ask)) = (last_book_ticker_bid, last_book_ticker_ask) {
        let (instant_profit_bp, order_side_profit_bp) = if side_str == "BUY" {
            (
                (bid - price) / price * 10000.0,
                (ask - price) / price * 10000.0,
            )
        } else {
            (
                (price - ask) / price * 10000.0,
                (price - bid) / price * 10000.0,
            )
        };

        // Same as Python: positive_color = Fore.LIGHTGREEN_EX + Back.BLACK, negative_color = Fore.LIGHTRED_EX + Back.BLACK
        let instant_profit_color = if instant_profit_bp > 0.0 {
            "\x1b[92m\x1b[40m" // Fore.LIGHTGREEN_EX + Back.BLACK
        } else if instant_profit_bp < 0.0 {
            "\x1b[91m\x1b[40m" // Fore.LIGHTRED_EX + Back.BLACK
        } else {
            "\x1b[37m\x1b[40m" // Fore.WHITE + Back.BLACK
        };

        // Same as Python: positive_color = Fore.LIGHTGREEN_EX + Back.BLACK, negative_color = Fore.LIGHTRED_EX + Back.BLACK
        let order_side_profit_color =
            if order_side_profit_bp > 1.5 && (order_side_profit_bp + instant_profit_bp) > 2.0 {
                "\x1b[92m\x1b[40m" // Fore.LIGHTGREEN_EX + Back.BLACK
            } else if order_side_profit_bp > 0.0 {
                "\x1b[92m\x1b[40m"
            } else if order_side_profit_bp < 0.0 {
                "\x1b[91m\x1b[40m"
            } else {
                "\x1b[37m\x1b[40m"
            };

        let usdt_amount = if let Some(contract) = contract {
            let quanto_multiplier: f64 = contract.quanto_multiplier.parse().unwrap_or(1.0);
            Some(quanto_multiplier * price * size as f64)
        } else {
            None
        };

        let mut fee_color = "\x1b[37m\x1b[40m";
        let mut fee_rate_bp = 0.0;
        let mut fee_type: &str = "";

        if let Some(usdt) = usdt_amount {
            fee_rate_bp = fee.abs() / usdt.abs() * 10000.0;
            (fee_type, fee_color) = if fee_rate_bp > 2.5 {
                // market trade yellow
                ("taker", "\x1b[32m")
            } else if fee_rate_bp > 0.5 {
                // maker trade red
                ("maker", "\x1b[33m")
            } else if fee_rate_bp > 0.0 {
                // fee not updated magenta
                ("not updated", "\x1b[35m")
            } else {
                // fee not updated white
                ("not updated", "\x1b[37m\x1b[40m")
            };
        }

        let border_color = if instant_profit_bp + order_side_profit_bp > 0.0 {
            "\x1b[32m" // Green
        } else if instant_profit_bp + order_side_profit_bp < 0.0 {
            "\x1b[31m" // Red
        } else {
            "\x1b[37m" // White
        };

        let border_bg_color = if instant_profit_bp + order_side_profit_bp > 2.0 {
            match fee_type {
                "taker" => "\x1b[42m", // Light green
                "maker" => "\x1b[43m", // Magenta
                _ => "\x1b[40m",       // Black
            }
        } else if instant_profit_bp + order_side_profit_bp < -2.0 {
            match fee_type {
                "taker" => "\x1b[41m", // Light red
                "maker" => "\x1b[43m", // Orange
                _ => "\x1b[40m",       // Black
            }
        } else {
            "\x1b[40m" // Black
        };

        plog!(
            "\n{}{}{}{}",
            border_bg_color,
            border_color,
            "=".repeat(102),
            reset
        );

        if let Some(usdt) = usdt_amount {
            plog!("{}{}{}", bg_black, "-".repeat(102), reset);
            plog!(
                "{}{:>10} | {:>12} | {:>10} | {:>10} | {:>10} | {:>16} | {:>14} |{}",
                bg_black,
                "Contract",
                "USDT Amount",
                "I.P.",
                "O.S.P.",
                "Price",
                "Name",
                "Time",
                reset
            );
            plog!("{}{}{}", bg_black, "-".repeat(102), reset);
            plog!(
                "{}{:>10} | {}{:>+12.2}{} | {}{:>10.2}{} | {}{:>10.2}{} | {:>10} | {:>16} | {:>14} |",
                bg_black,
                symbol_short,
                if usdt > 0.0 { "\x1b[92m" } else { "\x1b[91m" },
                usdt,
                reset,
                instant_profit_color,
                instant_profit_bp,
                reset,
                order_side_profit_color,
                order_side_profit_bp,
                reset,
                price,
                login_name,
                timestamp_to_str(create_time_ms)
            );
            plog!("{}{}{}", bg_black, "-".repeat(102), reset);
        } else {
            plog!(
                "{}Contract{:>10} | Side{:>6} | I.P.{:>10} | O.S.P.{:>10} | Price{:>10} | Name{:>16} | Time{:>14} |{}",
                bg_black,
                "",
                "",
                "",
                "",
                "",
                "",
                "",
                reset
            );
            plog!("{}{}{}", bg_black, "-".repeat(102), reset);
            plog!(
                "{}{:>10} | {}{:>6}{} | {}{:>10.2}{} | {}{:>10.2}{} | {:>10} | {:>16} | {:>14} |",
                bg_black,
                symbol_short,
                side_color,
                side_str,
                reset,
                instant_profit_color,
                instant_profit_bp,
                reset,
                order_side_profit_color,
                order_side_profit_bp,
                reset,
                price,
                login_name,
                timestamp_to_str(create_time_ms)
            );
        }

        if success_rate > 0.0 || profitable_rate > 0.0 {
            plog!(
                "{}{}: {:>10.2} | {}: {:>10.2} |{}",
                bg_black,
                "Success Rate",
                success_rate,
                "Profitable Rate",
                profitable_rate,
                reset
            );
        }
        if profit_bp_ema.is_normal() {
            plog!(
                "{}{}: {:>10.2} | {}",
                bg_black,
                "Profit BP EMA",
                profit_bp_ema,
                reset
            );
        }
        if fee_rate_bp > 0.0 {
            plog!(
                "{}{}: {:>10.2} | {}",
                fee_color,
                "Fee Rate BP",
                fee_rate_bp,
                reset
            );
        }
        plog!(
            "{}{}{}{}",
            border_bg_color,
            border_color,
            "=".repeat(102),
            reset
        );
    }
}

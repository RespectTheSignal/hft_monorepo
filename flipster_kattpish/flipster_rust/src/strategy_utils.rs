use crate::gate_order_manager::GateContract;
use crate::signal_calculator::SignalResult;
use crate::types::BookTickerC;
use rust_decimal::prelude::*;
use rust_decimal::Decimal;

const EPS: f64 = 1e-12;

pub fn calculate_mid_profit_bp(
    order_side: &str,
    gate_last_book_ticker: &BookTickerC,
    gate_last_web_book_ticker: &BookTickerC,
) -> f64 {
    let last_book_ticker_mid =
        (gate_last_book_ticker.ask_price + gate_last_book_ticker.bid_price) * 0.5;
    let web_book_ticker_order_price = if order_side == "buy" {
        gate_last_web_book_ticker.ask_price
    } else {
        gate_last_web_book_ticker.bid_price
    };
    let mid_profit_bp = if order_side == "buy" {
        (last_book_ticker_mid - web_book_ticker_order_price) / (web_book_ticker_order_price + EPS)
            * 10000.0
    } else if order_side == "sell" {
        (web_book_ticker_order_price - last_book_ticker_mid) / (web_book_ticker_order_price + EPS)
            * 10000.0
    } else {
        return 0.0;
    };
    mid_profit_bp
}

pub fn is_order_price_at_edge(signal: &SignalResult, order_price_round_multiplier: f64) -> bool {
    let order_side = signal.order_side.as_ref().unwrap();
    let order_price = *signal.order_price.as_ref().unwrap();
    let gate_mid = signal.gate_mid;
    if order_side == "buy" {
        let available_order_price = signal.gate_bid
            + signal
                .gate_contract
                .order_price_round
                .parse::<f64>()
                .unwrap_or(0.0)
                * order_price_round_multiplier
            + EPS;
        if available_order_price >= order_price && order_price < gate_mid {
            return true;
        }
    } else if order_side == "sell" {
        let available_order_price = signal.gate_ask
            - signal
                .gate_contract
                .order_price_round
                .parse::<f64>()
                .unwrap_or(0.0)
                * order_price_round_multiplier
            - EPS;
        if available_order_price <= order_price && order_price > gate_mid {
            return true;
        }
    }
    false
}

pub fn is_order_price_quarter_edge(signal: &SignalResult) -> bool {
    let order_side = signal.order_side.as_ref().unwrap();
    let order_price = *signal.order_price.as_ref().unwrap();
    let gate_mid = signal.gate_mid;
    if order_side == "buy" {
        let order_price_mid_gap = (order_price - gate_mid).abs();
        let order_price_best_gap = (signal.gate_bid - order_price).abs();
        if order_price_best_gap < order_price_mid_gap && order_price < gate_mid {
            return true;
        }
    } else if order_side == "sell" {
        let order_price_mid_gap = (order_price - gate_mid).abs();
        let order_price_best_gap = (signal.gate_ask - order_price).abs();
        if order_price_best_gap < order_price_mid_gap && order_price > gate_mid {
            return true;
        }
    }
    false
}

pub fn get_valid_order_price(contract_info: &GateContract, order_side: &str, price: f64) -> String {
    if price == 0.0 {
        return "0".to_string();
    }

    let order_price_round: Decimal = contract_info
        .order_price_round
        .parse()
        .unwrap_or_else(|_| Decimal::from_str("0.0000000001").unwrap());

    if order_price_round == Decimal::ZERO {
        return price.to_string();
    }

    let modified_price = if order_side == "buy" {
        price + EPS
    } else {
        price - EPS
    };

    let price_decimal = Decimal::from_str_exact(&format!("{:.10}", modified_price))
        .or_else(|_| Decimal::from_str(&modified_price.to_string()))
        .unwrap_or_else(|_| Decimal::ZERO);

    let divided = price_decimal / order_price_round;

    let rounded_price = match order_side {
        "buy" => divided.floor(),
        "sell" => divided.ceil(),
        _ => divided.round_dp(0),
    };

    let quantized_price = rounded_price * order_price_round;
    return quantized_price.to_string();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_contract(order_price_round: &str) -> GateContract {
        GateContract {
            name: "TEST_USDT".to_string(),
            contract_type: "direct".to_string(),
            quanto_multiplier: "1".to_string(),
            leverage_min: "1".to_string(),
            leverage_max: "25".to_string(),
            mark_price: "0".to_string(),
            funding_rate: "0".to_string(),
            order_size_min: 1,
            order_size_max: 100000,
            order_price_round: order_price_round.to_string(),
        }
    }

    #[test]
    fn test_get_valid_order_price_eul_buy_sell() {
        // EUL_USDT: order_price_round = 0.0001
        let c = mk_contract("0.0001");
        // Buy: floor(1.40593 / 0.0001) * 0.0001 = 1.4059 (buy rounds down after +EPS)
        let buy = get_valid_order_price(&c, "buy", 1.40593);
        println!("buy  1.40593 -> {}", buy);
        // Sell: ceil(1.40593 / 0.0001) * 0.0001 = 1.4060 (sell rounds up after -EPS)
        let sell = get_valid_order_price(&c, "sell", 1.40593);
        println!("sell 1.40593 -> {}", sell);
        assert_eq!(buy, "1.4059");
        assert_eq!(sell, "1.4060");
    }

    #[test]
    fn test_get_valid_order_price_exact_round() {
        // Price exactly on tick: buy/sell should stay same (with EPS adjustment handled)
        let c = mk_contract("0.0001");
        let buy = get_valid_order_price(&c, "buy", 1.4059);
        let sell = get_valid_order_price(&c, "sell", 1.4059);
        println!("exact buy  1.4059 -> {}", buy);
        println!("exact sell 1.4059 -> {}", sell);
        assert_eq!(buy, "1.4059");
        assert_eq!(sell, "1.4059");
    }

    #[test]
    fn test_get_valid_order_price_btc_tick_0_1() {
        // BTC_USDT-ish: order_price_round = 0.1
        let c = mk_contract("0.1");
        let buy = get_valid_order_price(&c, "buy", 65432.567);
        let sell = get_valid_order_price(&c, "sell", 65432.567);
        println!("BTC buy  65432.567 -> {}", buy);
        println!("BTC sell 65432.567 -> {}", sell);
        assert_eq!(buy, "65432.5");
        assert_eq!(sell, "65432.6");
    }

    #[test]
    fn test_get_valid_order_price_zero() {
        let c = mk_contract("0.0001");
        assert_eq!(get_valid_order_price(&c, "buy", 0.0), "0");
    }

    #[test]
    fn test_get_valid_order_price_many_decimals() {
        // 1.405699999999... like f64 precision noise
        let c = mk_contract("0.0001");
        let noisy: f64 = 1.40569999999999999999;
        println!("noisy repr = {:.20}", noisy);
        let buy = get_valid_order_price(&c, "buy", noisy);
        let sell = get_valid_order_price(&c, "sell", noisy);
        println!("noisy buy  {} -> {}", noisy, buy);
        println!("noisy sell {} -> {}", noisy, sell);

        // Same with slightly under tick
        let near: f64 = 1.4056999999999999;
        println!("near repr = {:.20}", near);
        let buy2 = get_valid_order_price(&c, "buy", near);
        let sell2 = get_valid_order_price(&c, "sell", near);
        println!("near buy  {} -> {}", near, buy2);
        println!("near sell {} -> {}", near, sell2);
    }

    #[test]
    fn test_get_valid_order_price_f64_edge_cases() {
        let c = mk_contract("0.0001");
        // These can't be exactly represented in f64
        let tests: &[(f64, &str)] = &[
            (0.1f64 + 0.2f64, "0.1+0.2"),           // 0.30000000000000004
            (1.0 - 0.9999999999999999, "1-0.999..."),
            (1.4057_f64 * 3.0 / 3.0, "1.4057 * 3 / 3"),
        ];
        for (v, label) in tests {
            let buy = get_valid_order_price(&c, "buy", *v);
            let sell = get_valid_order_price(&c, "sell", *v);
            println!("{:<20} = {:.20} -> buy={} sell={}", label, v, buy, sell);
        }
    }

    #[test]
    fn test_get_valid_order_price_large_tick() {
        // order_price_round = 1
        let c = mk_contract("1");
        let buy = get_valid_order_price(&c, "buy", 99.9);
        let sell = get_valid_order_price(&c, "sell", 99.9);
        println!("tick=1 buy  99.9 -> {}", buy);
        println!("tick=1 sell 99.9 -> {}", sell);
        assert_eq!(buy, "99");
        assert_eq!(sell, "100");
    }
}


// Risk management logic

const MAX_POSITION_SIDE_RATIO: f64 = 0.5;  // 50%
const DEFAULT_LEVERAGE: f64 = 50.0;

pub struct PositionInfo {
    pub total_long_usdt: f64,
    pub total_short_usdt: f64,
}

pub fn check_risk_management(
    is_buy: bool,
    usdt_order_size: f64,
    account_balance: f64,
    position_info: &PositionInfo,
) -> bool {
    // Calculate expected positions after this order
    let expected_long = if is_buy {
        position_info.total_long_usdt + usdt_order_size
    } else {
        position_info.total_long_usdt
    };
    
    let expected_short = if is_buy {
        position_info.total_short_usdt
    } else {
        position_info.total_short_usdt + usdt_order_size
    };
    
    // Calculate net exposure
    let expected_net_exposure = expected_long - expected_short;
    let account_balance_with_leverage = account_balance * DEFAULT_LEVERAGE;
    
    if account_balance_with_leverage <= 0.0 {
        return false; // Invalid account balance
    }
    
    // Block buy orders if net long exposure would exceed threshold
    if is_buy && expected_net_exposure > 0.0 {
        let expected_net_exposure_ratio = expected_net_exposure / account_balance_with_leverage;
        if expected_net_exposure_ratio > MAX_POSITION_SIDE_RATIO {
            return false; // Blocked
        }
    }
    
    // Block sell orders if net short exposure would exceed threshold
    if !is_buy && expected_net_exposure < 0.0 {
        let expected_net_exposure_ratio = expected_net_exposure.abs() / account_balance_with_leverage;
        if expected_net_exposure_ratio > MAX_POSITION_SIDE_RATIO {
            return false; // Blocked
        }
    }
    
    true // Allowed
}


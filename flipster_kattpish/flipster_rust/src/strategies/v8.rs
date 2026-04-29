use crate::config::TradeSettings;
use crate::data_cache::SymbolBooktickerSnapshot;
use crate::gate_order_manager::{GateContract, GateOrderManager, LastOrder};
use crate::handle_chance::handle_chance_v8;
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
static LAST_NET_LONG_EXPOSURE_WARN_MS: AtomicI64 = AtomicI64::new(0);
static LAST_NET_SHORT_EXPOSURE_WARN_MS: AtomicI64 = AtomicI64::new(0);

const EPS: f64 = 1e-12;

pub struct V8StrategyCore {
    handle_chance_manager: Arc<HandleChanceManager>,
    // Symbol -> Timestamp (ms) when narrative fail started
    // narrative_fail_start_times: Arc<RwLock<HashMap<String, i64>>>,
}

impl V8StrategyCore {
    pub fn new(leverage: Option<f64>) -> Self {
        Self {
            handle_chance_manager: Arc::new(HandleChanceManager::new(leverage)),
            // narrative_fail_start_times: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl StrategyCore for V8StrategyCore {
    fn name(&self) -> &str {
        "V8Strategy"
    }

    fn calculate_signal(
        &self,
        snapshot: &SymbolBooktickerSnapshot,
        contract: &GateContract,
        gate_last_trade: Option<TradeC>,
        _avg_mid_gap: Option<f64>,
        market_state: &crate::market_watcher::MarketGapState,
    ) -> SignalResult {
        let signal = signal_calculator::calculate_signal_v8(
            snapshot,
            &contract,
            gate_last_trade,
            market_state,
        );
        return signal;

        // if signal.order_side.is_some() && signal.order_price.is_some() {
        //     return signal;
        // } else {
        //     let order_side = if (gate_bt.ask_price + gate_bt.bid_price) * 0.5
        //         < (gate_web_bt.ask_price + gate_web_bt.bid_price) * 0.5
        //     {
        //         "sell"
        //     } else if (gate_bt.ask_price + gate_bt.bid_price) * 0.5
        //         > (gate_web_bt.ask_price + gate_web_bt.bid_price) * 0.5
        //     {
        //         "buy"
        //     } else if let Some(binance_bt) = binance_bt {
        //         if (gate_bt.ask_price + gate_bt.bid_price)
        //             < (binance_bt.ask_price + binance_bt.bid_price) * 0.5
        //         {
        //             "buy"
        //         } else if (gate_bt.ask_price + gate_bt.bid_price)
        //             > (binance_bt.ask_price + binance_bt.bid_price) * 0.5
        //         {
        //             "sell"
        //         } else {
        //             return signal;
        //         }
        //     } else {
        //         return signal;
        //     };
        //     let order_price = if order_side == "buy" {
        //         gate_web_bt.bid_price
        //     } else {
        //         gate_web_bt.ask_price
        //     };
        //     signal.order_side = Some(order_side.to_string());
        //     signal.order_price = Some(order_price);
        //     return signal;
        // }
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
        avg_mid_gap: Option<f64>,
        avg_spread: Option<f64>,
        total_net_positions_usdt_size: f64,
        previous_snapshot_1s: Option<&SymbolBooktickerSnapshot>,
        previous_snapshot_5s: Option<&SymbolBooktickerSnapshot>,
        previous_snapshot_10s: Option<&SymbolBooktickerSnapshot>,
        previous_snapshot_20s: Option<&SymbolBooktickerSnapshot>,
        gate_order_manager: &GateOrderManager,
        market_state: &crate::market_watcher::MarketGapState,
    ) -> Option<OrderDecision> {
        crate::decide_order_v8::decide_order(
            symbol,
            signal,
            trade_settings,
            current_time_ms,
            gate_bt_server_time,
            binance_bt_server_time,
            gate_web_bt_server_time,
            funding_rate,
            order_count_size,
            size_trigger,
            max_size_trigger,
            close_size_trigger,
            contract_order_size,
            usdt_position_size,
            avg_spread,
            avg_mid_gap,
            total_net_positions_usdt_size,
            previous_snapshot_1s,
            previous_snapshot_5s,
            previous_snapshot_10s,
            previous_snapshot_20s,
            gate_order_manager,
            market_state,
            trade_settings
                .use_few_orders_closing_trigger
                .unwrap_or(true), // v8_0 default: no shortcut
            trade_settings
                .ignore_net_position_size_check
                .unwrap_or(false),
        )
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
        handle_chance_v8(
            chance,
            gate_order_manager,
            trade_settings,
            futures_account_total,
            futures_account_unrealised_pnl,
            total_net_positions_usdt_size,
        )
    }
}

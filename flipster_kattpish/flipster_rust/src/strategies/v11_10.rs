// V11_10: v10 signal (calculate_signal_v10) + v10 decide_order + v11 handle_chance. Uses market watcher (avg_mid_gap) like v10.
use crate::config::TradeSettings;
use crate::data_cache::SymbolBooktickerSnapshot;
use crate::gate_order_manager::{GateContract, GateOrderManager};
use crate::handle_chance::{Chance, HandleChanceManager};
use crate::order_decision::OrderDecision;
use crate::signal_calculator::{self, SignalResult};
use crate::strategy_core::StrategyCore;
use crate::types::{BookTickerC, TradeC};
use std::sync::Arc;

pub struct V11_10StrategyCore {
    handle_chance_manager: Arc<HandleChanceManager>,
}

impl V11_10StrategyCore {
    pub fn new(leverage: Option<f64>) -> Self {
        Self {
            handle_chance_manager: Arc::new(HandleChanceManager::new(leverage)),
        }
    }
}

impl StrategyCore for V11_10StrategyCore {
    fn name(&self) -> &str {
        "V11_10Strategy"
    }

    // v10: signal_v10 + avg_mid_gap (market watcher)
    fn calculate_signal(
        &self,
        snapshot: &SymbolBooktickerSnapshot,
        contract: &GateContract,
        gate_last_trade: Option<TradeC>,
        avg_mid_gap: Option<f64>,
        _market_state: &crate::market_watcher::MarketGapState,
    ) -> SignalResult {
        signal_calculator::calculate_signal_v10(snapshot, contract,
            gate_last_trade,
            avg_mid_gap,
        )
        // if signal.order_side.is_some() && signal.order_price.is_some() {
        //     return signal;
        // }
        // let order_side = if (gate_bt.ask_price + gate_bt.bid_price) * 0.5
        //     < (gate_web_bt.ask_price + gate_web_bt.bid_price) * 0.5
        // {
        //     "sell"
        // } else if (gate_bt.ask_price + gate_bt.bid_price) * 0.5
        //     > (gate_web_bt.ask_price + gate_web_bt.bid_price) * 0.5
        // {
        //     "buy"
        // } else if let Some(binance_bt) = binance_bt {
        //     if (gate_bt.ask_price + gate_bt.bid_price)
        //         < (binance_bt.ask_price + binance_bt.bid_price) * 0.5
        //     {
        //         "buy"
        //     } else if (gate_bt.ask_price + gate_bt.bid_price)
        //         > (binance_bt.ask_price + binance_bt.bid_price) * 0.5
        //     {
        //         "sell"
        //     } else {
        //         return signal;
        //     }
        // } else {
        //     return signal;
        // };
        // let order_price = if order_side == "buy" {
        //     gate_web_bt.bid_price
        // } else {
        //     gate_web_bt.ask_price
        // };
        // signal.order_side = Some(order_side.to_string());
        // signal.order_price = Some(order_price);
        // signal
    }

    // v10: decide_order_v8 with use_few_orders_closing_trigger default false
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
            _avg_spread,
            None,
            total_net_positions_usdt_size,
            previous_snapshot_1s,
            previous_snapshot_5s,
            previous_snapshot_10s,
            previous_snapshot_20s,
            gate_order_manager,
            market_state,
            trade_settings
                .use_few_orders_closing_trigger
                .unwrap_or(false),
            trade_settings
                .ignore_net_position_size_check
                .unwrap_or(false),
        )
    }

    // v11 handle_chance via shared handle_change_v11
    fn handle_chance(
        &self,
        chance: &Chance,
        gate_order_manager: &GateOrderManager,
        trade_settings: &TradeSettings,
        futures_account_total: f64,
        futures_account_unrealised_pnl: f64,
        total_net_positions_usdt_size: f64,
    ) -> Option<(i64, f64, bool)> {
        crate::handle_chance::handle_change_v11(
            chance,
            gate_order_manager,
            trade_settings,
            futures_account_total,
            futures_account_unrealised_pnl,
            total_net_positions_usdt_size,
        )
    }
}

use crate::config::TradeSettings;
use crate::data_cache::SymbolBooktickerSnapshot;
use crate::gate_order_manager::GateContract;
use crate::gate_order_manager::GateOrderManager;
use crate::handle_chance::{Chance, HandleChanceManager};
use crate::order_decision::{self, OrderDecision};
use crate::signal_calculator::{self, SignalResult};
use crate::strategy_core::StrategyCore;
use crate::types::{BookTickerC, TradeC};
use std::sync::Arc;

pub struct BasicStrategyCore {
    handle_chance_manager: Arc<HandleChanceManager>,
}

impl BasicStrategyCore {
    pub fn new(leverage: Option<f64>) -> Self {
        Self {
            handle_chance_manager: Arc::new(HandleChanceManager::new(leverage)),
        }
    }
}

impl StrategyCore for BasicStrategyCore {
    fn name(&self) -> &str {
        "BasicStrategy"
    }

    fn calculate_signal(
        &self,
        snapshot: &SymbolBooktickerSnapshot,
        contract: &GateContract,
        _gate_last_trade: Option<TradeC>,
        _avg_mid_gap: Option<f64>,
        _market_state: &crate::market_watcher::MarketGapState,
    ) -> SignalResult {
        signal_calculator::calculate_signal(snapshot, &contract)
    }

    fn decide_order(
        &self,
        _symbol: &str,
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
        previous_snapshot_1s: Option<&SymbolBooktickerSnapshot>,
        previous_snapshot_5s: Option<&SymbolBooktickerSnapshot>,
        previous_snapshot_10s: Option<&SymbolBooktickerSnapshot>,
        previous_snapshot_20s: Option<&SymbolBooktickerSnapshot>,
        _gate_order_manager: &GateOrderManager,
        _market_state: &crate::market_watcher::MarketGapState,
    ) -> Option<OrderDecision> {
        order_decision::decide_order(
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
            previous_snapshot_1s,
            previous_snapshot_5s,
            previous_snapshot_10s,
            previous_snapshot_20s,
        )
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
        self.handle_chance_manager.handle_chance(
            chance,
            gate_order_manager,
            trade_settings,
            futures_account_total,
            futures_account_unrealised_pnl,
        )
    }
}

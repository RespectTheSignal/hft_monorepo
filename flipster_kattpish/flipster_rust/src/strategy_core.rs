use crate::config::TradeSettings;
use crate::data_cache::SymbolBooktickerSnapshot;
use crate::gate_order_manager::GateContract;
use crate::gate_order_manager::GateOrderManager;
use crate::handle_chance::Chance;
use crate::market_watcher::MarketGapState;
use crate::order_decision::OrderDecision;
use crate::signal_calculator::SignalResult;
use crate::types::{BookTickerC, TradeC};

pub trait StrategyCore: Send + Sync {
    fn name(&self) -> &str;

    fn calculate_signal(
        &self,
        snapshot: &SymbolBooktickerSnapshot,
        contract: &GateContract,
        gate_last_trade: Option<TradeC>,
        avg_mid_gap: Option<f64>,
        market_state: &MarketGapState,
    ) -> SignalResult;

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
        avg_entry_price: Option<f64>,
        gate_last_trade: Option<TradeC>,
        avg_mid_gap: Option<f64>,
        avg_spread: Option<f64>,
        total_net_positions_usdt_size: f64,
        previous_snapshot_1s: Option<&SymbolBooktickerSnapshot>,
        previous_snapshot_5s: Option<&SymbolBooktickerSnapshot>,
        previous_snapshot_10s: Option<&SymbolBooktickerSnapshot>,
        previous_snapshot_20s: Option<&SymbolBooktickerSnapshot>,
        gate_order_manager: &GateOrderManager,
        market_state: &MarketGapState,
    ) -> Option<OrderDecision>;

    fn handle_chance(
        &self,
        chance: &Chance,
        gate_order_manager: &GateOrderManager,
        trade_settings: &TradeSettings,
        futures_account_total: f64,
        futures_account_unrealised_pnl: f64,
        total_net_positions_usdt_size: f64,
    ) -> Option<(i64, f64, bool)>;
}

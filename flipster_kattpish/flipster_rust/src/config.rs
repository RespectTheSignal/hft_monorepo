// Configuration management - load strategy settings from Supabase
//
// This mirrors Python's StrategySetting (src/models/settings.py),
// which loads from Supabase tables:
// - strategy_settings (by login_name)
// - trade_settings    (by id, column trade_settings is JSON)
// - symbol_sets       (by id, column symbols is string[])

use anyhow::{anyhow, Result};
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TradeSettings {
    #[serde(default = "default_only_handle_net_position_size_close_side")]
    pub only_handle_net_position_size_close_side: bool,

    #[serde(default = "default_update_interval_seconds")]
    pub update_interval_seconds: f64,
    pub order_size: f64,
    #[serde(default = "default_ws_order_size")]
    pub ws_order_size: f64,
    pub close_order_size: Option<f64>,
    pub market_close_order_size: Option<f64>,
    #[serde(default = "default_should_market_close_inverval_seconds")]
    pub should_market_close_inverval_seconds: i64,

    pub max_position_size: f64,
    #[serde(default)]
    pub dynamic_max_position_size: bool,
    #[serde(default = "default_max_position_size_multiplier")]
    pub max_position_size_multiplier: f64,

    pub trade_size_trigger: f64,
    pub max_trade_size_trigger: Option<f64>,
    pub close_trade_size_trigger: Option<f64>,

    pub close_order_count: i64,
    #[serde(default = "default_max_order_size_multiplier")]
    pub max_order_size_multiplier: i64,

    pub close_raw_mid_profit_bp: f64,

    #[serde(default)]
    pub ignore_recent_trade_ms: i64,

    pub mid_gap_bp_threshold: f64,
    pub spread_bp_threshold: f64,

    #[serde(default = "default_wait_time_close_ws_order_ms")]
    pub wait_time_close_ws_order_ms: i64,

    #[serde(default)]
    pub bypass_safe_limit_close: bool,
    #[serde(default = "default_only_follow_trade_amount")]
    pub only_follow_trade_amount: bool,

    #[serde(default = "default_allow_limit_close")]
    pub allow_limit_close: bool,

    #[serde(default = "default_ignore_non_profitable_order_rate")]
    pub ignore_non_profitable_order_rate: f64,
    #[serde(default = "default_order_success_rate")]
    pub order_success_rate: f64,

    #[serde(default = "default_gate_last_trade_latency_ms")]
    pub gate_last_trade_latency_ms: i64,
    pub gate_last_book_ticker_latency_ms: i64,
    pub binance_last_book_ticker_latency_ms: i64,
    #[serde(default = "default_gate_last_webbook_ticker_latency_ms")]
    pub gate_last_webbook_ticker_latency_ms: i64,

    #[serde(default = "default_funding_rate_threshold")]
    pub funding_rate_threshold: f64,

    #[serde(default = "default_wait_for_book_ticker_after_trade_time_ms")]
    pub wait_for_book_ticker_after_trade_time_ms: i64,

    #[serde(default = "default_limit_open_tif")]
    pub limit_open_tif: String,
    #[serde(default = "default_limit_close_tif")]
    pub limit_close_tif: String,

    #[serde(default = "default_bypass_max_position_size")]
    pub bypass_max_position_size: bool,

    pub opposite_side_max_position_size: Option<f64>,

    #[serde(default = "default_limit_open_time_restriction_ms_min")]
    pub limit_open_time_restriction_ms_min: i64,
    #[serde(default = "default_limit_open_time_restriction_ms_max")]
    pub limit_open_time_restriction_ms_max: i64,
    #[serde(default = "default_limit_close_time_restriction_ms_min")]
    pub limit_close_time_restriction_ms_min: i64,
    #[serde(default = "default_limit_close_time_restriction_ms_max")]
    pub limit_close_time_restriction_ms_max: i64,

    #[serde(default = "default_same_side_price_time_restriction_ms_min")]
    pub same_side_price_time_restriction_ms_min: i64,
    #[serde(default = "default_same_side_price_time_restriction_ms_max")]
    pub same_side_price_time_restriction_ms_max: i64,

    #[serde(default = "default_profit_bp_ema_alpha")]
    pub profit_bp_ema_alpha: f64,

    #[serde(default = "default_profit_bp_ema_threshold")]
    pub profit_bp_ema_threshold: f64,

    #[serde(default = "default_succes_threshold")]
    pub succes_threshold: f64,
    #[serde(default = "default_normalize_trade_count")]
    pub normalize_trade_count: i64,

    #[serde(default = "default_repeat_profitable_order")]
    pub repeat_profitable_order: bool,

    /// When true (v11 series), repeat profitable order via WebSocket amend instead of order_manager_client place_order.
    #[serde(default)]
    pub repeat_profitable_order_use_amend: Option<bool>,

    #[serde(default = "default_close_stale_minutes")]
    pub close_stale_minutes: i64,

    #[serde(default = "default_warn_stale_minutes")]
    pub warn_stale_minutes: i64,

    #[serde(default = "default_close_stale_minutes_size")]
    pub close_stale_minutes_size: Option<f64>,

    #[serde(default = "default_too_many_orders_time_gap_ms")]
    pub too_many_orders_time_gap_ms: i64,

    #[serde(default = "default_too_many_orders_size_threshold_multiplier")]
    pub too_many_orders_size_threshold_multiplier: i64,

    #[serde(default = "default_symbol_too_many_orders_size_threshold_multiplier")]
    pub symbol_too_many_orders_size_threshold_multiplier: i64,

    #[serde(default = "default_max_gate_binance_gap_percentage_threshold")]
    pub max_gate_binance_gap_percentage_threshold: f64,

    /// CLI/override: when set, decide_order_v8 uses this for few_orders/closing trigger shortcut.
    #[serde(default)]
    pub use_few_orders_closing_trigger: Option<bool>,

    /// When few orders and closing: size_trigger to use (default 100). Only used when use_few_orders_closing_trigger is true.
    #[serde(default)]
    pub size_trigger_for_few_orders: Option<i64>,

    /// When few orders and closing: close_size_trigger to use (default 100). Only used when use_few_orders_closing_trigger is true.
    #[serde(default)]
    pub close_size_trigger_for_few_orders: Option<i64>,

    #[serde(default = "default_filter_order_size_on_volatility_usdt_threshold")]
    pub filter_order_size_on_volatility_usdt_threshold: f64,

    #[serde(default)]
    pub ignore_net_position_size_check: Option<bool>,

    #[serde(default = "default_ws_order_price_round_multiplier")]
    pub ws_order_price_round_multiplier: f64,

    #[serde(default = "default_ws_order_spread_multiplier")]
    pub ws_order_spread_multiplier: f64,

    #[serde(default = "default_avg_spread_ratio_threshold_open")]
    pub avg_spread_ratio_threshold_open: f64,

    #[serde(default = "default_avg_spread_ratio_threshold_close")]
    pub avg_spread_ratio_threshold_close: f64,

    #[serde(default = "default_expire_time_ms")]
    pub expire_time_ms: i64,

    #[serde(default = "default_close_expire_time_ms")]
    pub close_expire_time_ms: i64,

    #[serde(default = "default_wait_time_same_or_more_expensive_order_ms")]
    pub wait_time_same_or_more_expensive_order_ms: i64,

    #[serde(default = "default_wait_time_not_close_big_same_or_more_expensive_order_ms")]
    pub wait_time_not_close_big_same_or_more_expensive_order_ms: i64,

    #[serde(default = "default_dangerous_symbols_corr_min_gate_bt")]
    pub dangerous_symbols_corr_min_gate_bt: f64,
    #[serde(default = "default_dangerous_symbols_corr_web_gt_gate_margin")]
    pub dangerous_symbols_corr_web_gt_gate_margin: f64,
    #[serde(default = "default_only_close_dangerous_symbol")]
    pub only_close_dangerous_symbol: bool,
    #[serde(default = "default_close_based_on_1m_chance_bp_threshold")]
    pub close_based_on_1m_chance_bp_threshold: f64,

    #[serde(default = "default_use_long_term_chance_close")]
    pub use_long_term_chance_close: bool,
}

fn default_only_handle_net_position_size_close_side() -> bool {
    true
}

// Default value functions (matching Python GateHFTV21TradeSettings)
fn default_update_interval_seconds() -> f64 {
    0.1
}
fn default_max_position_size_multiplier() -> f64 {
    1.5
}
fn default_max_order_size_multiplier() -> i64 {
    3
}
fn default_wait_time_close_ws_order_ms() -> i64 {
    1000
}
fn default_only_follow_trade_amount() -> bool {
    true
}
fn default_allow_limit_close() -> bool {
    true
}
fn default_ignore_non_profitable_order_rate() -> f64 {
    0.6
}
fn default_order_success_rate() -> f64 {
    0.05
}
fn default_gate_last_trade_latency_ms() -> i64 {
    1000
}
fn default_gate_last_webbook_ticker_latency_ms() -> i64 {
    500
}
fn default_funding_rate_threshold() -> f64 {
    0.005
}
fn default_wait_for_book_ticker_after_trade_time_ms() -> i64 {
    100
}
fn default_limit_open_tif() -> String {
    "fok".to_string()
}
fn default_limit_close_tif() -> String {
    "fok".to_string()
}
fn default_bypass_max_position_size() -> bool {
    false
}

fn default_limit_open_time_restriction_ms_min() -> i64 {
    100
}
fn default_limit_open_time_restriction_ms_max() -> i64 {
    200
}

fn default_limit_close_time_restriction_ms_min() -> i64 {
    100
}
fn default_limit_close_time_restriction_ms_max() -> i64 {
    200
}

fn default_same_side_price_time_restriction_ms_min() -> i64 {
    300
}
fn default_same_side_price_time_restriction_ms_max() -> i64 {
    600
}
fn default_profit_bp_ema_alpha() -> f64 {
    0.1
}
fn default_profit_bp_ema_threshold() -> f64 {
    1.20
}

fn default_succes_threshold() -> f64 {
    0.6
}

fn default_normalize_trade_count() -> i64 {
    100
}

fn default_repeat_profitable_order() -> bool {
    true
}

fn default_close_stale_minutes() -> i64 {
    60 * 24 * 2
}

fn default_close_stale_minutes_size() -> Option<f64> {
    None
}

fn default_warn_stale_minutes() -> i64 {
    60
}

fn default_should_market_close_inverval_seconds() -> i64 {
    300
}

fn default_too_many_orders_time_gap_ms() -> i64 {
    10 * 60 * 1000
}

fn default_too_many_orders_size_threshold_multiplier() -> i64 {
    2
}
fn default_symbol_too_many_orders_size_threshold_multiplier() -> i64 {
    2
}

fn default_max_gate_binance_gap_percentage_threshold() -> f64 {
    1.0
}

fn default_filter_order_size_on_volatility_usdt_threshold() -> f64 {
    100.0
}

fn default_ws_order_price_round_multiplier() -> f64 {
    300.0
}

fn default_ws_order_spread_multiplier() -> f64 {
    30.0
}

fn default_avg_spread_ratio_threshold_open() -> f64 {
    1.3
}

fn default_avg_spread_ratio_threshold_close() -> f64 {
    1.3
}

fn default_ws_order_size() -> f64 {
    10.0
}

fn default_expire_time_ms() -> i64 {
    40
}

fn default_close_expire_time_ms() -> i64 {
    50
}

fn default_wait_time_same_or_more_expensive_order_ms() -> i64 {
    100
}
fn default_wait_time_not_close_big_same_or_more_expensive_order_ms() -> i64 {
    500
}

fn default_dangerous_symbols_corr_min_gate_bt() -> f64 {
    0.80
}
fn default_dangerous_symbols_corr_web_gt_gate_margin() -> f64 {
    0.02
}

fn default_only_close_dangerous_symbol() -> bool {
    true
}

fn default_close_based_on_1m_chance_bp_threshold() -> f64 {
    2.0
}

fn default_use_long_term_chance_close() -> bool {
    false
}

/// Optional runtime overrides from Supabase `strategy_settings.trading_runtime` (JSONB).
/// When set, these take precedence over .env values.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TradingRuntimeOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redis_state_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_status_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_grpc_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_lb_grpc_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_processor_ip: Option<String>,
}

/// Resolve value: Supabase override > .env > default.
pub fn resolve_string(override_val: Option<&String>, env_key: &str, default: &str) -> String {
    if let Some(s) = override_val.filter(|s| !s.is_empty()) {
        return s.clone();
    }
    std::env::var(env_key).unwrap_or_else(|_| default.to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrategyConfig {
    pub login_name: String,
    pub symbols: Vec<String>,
    pub trade_settings: TradeSettings,
    pub symbols_map: HashMap<String, bool>,
    /// Optional runtime overrides from Supabase (trading_runtime JSONB). Supabase > .env.
    pub trading_runtime: Option<TradingRuntimeOverrides>,
}

impl StrategyConfig {
    pub fn new(
        login_name: String,
        symbols: Vec<String>,
        trade_settings: TradeSettings,
        trading_runtime: Option<TradingRuntimeOverrides>,
    ) -> Self {
        let symbols_map: HashMap<String, bool> = symbols
            .clone()
            .into_iter()
            .map(|s| (s.clone(), true))
            .collect();

        Self {
            login_name,
            symbols,
            trade_settings,
            symbols_map,
            trading_runtime,
        }
    }

    pub fn is_strategy_symbol(&self, symbol: &str) -> bool {
        match self.symbols_map.get(symbol) {
            Some(is_strategy_symbol) => *is_strategy_symbol,
            None => false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct StrategySettingRow {
    login_name: String,
    #[serde(deserialize_with = "deserialize_text_id")]
    trade_setting: String, // Text type in DB (references trade_settings.id)
    symbol_set: i64, // Bigint type in DB (references symbol_sets.id)
    /// Optional trading runtime overrides (JSONB). When set, overrides .env for this login.
    #[serde(default)]
    trading_runtime: Option<TradingRuntimeOverrides>,
}

#[derive(Debug, Deserialize)]
struct TradeSettingsRow {
    #[serde(deserialize_with = "deserialize_text_id")]
    id: String, // Text type in DB
    trade_settings: TradeSettings,
}

#[derive(Debug, Deserialize)]
struct SymbolSetRow {
    id: i64,              // Bigint type in DB
    symbols: Vec<String>, // JSON array in DB
}

// Helper to deserialize text id (handles both string and number from JSON, converts to String)
fn deserialize_text_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Visitor;
    use std::fmt;

    struct TextIdVisitor;

    impl<'de> Visitor<'de> for TextIdVisitor {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string or number representing a text id")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(value.to_string())
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(value.to_string())
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(value.to_string())
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(value.to_string())
        }
    }

    deserializer.deserialize_any(TextIdVisitor)
}

pub struct ConfigManager;

impl ConfigManager {
    pub fn new() -> Self {
        Self
    }

    pub fn load_strategy_configs(&self, login_names: Vec<String>) -> Result<Vec<StrategyConfig>> {
        let supabase_url =
            env::var("SUPABASE_URL").map_err(|_| anyhow!("SUPABASE_URL is not set"))?;
        let supabase_key =
            env::var("SUPABASE_KEY").map_err(|_| anyhow!("SUPABASE_KEY is not set"))?;
        let retry_count = env::var("RETRY_LOAD_STRATEGY_CONFIG")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(3)
            .max(1);

        for attempt in 1..=retry_count {
            match Self::load_strategy_configs_once(&supabase_url, &supabase_key, &login_names) {
                Ok(configs) => return Ok(configs),
                Err(e) => {
                    if attempt == retry_count {
                        return Err(e);
                    }
                    error!(
                        "load_strategy_configs attempt {}/{} failed: {}, retrying...",
                        attempt, retry_count, e
                    );
                }
            }
        }
        unreachable!()
    }

    fn load_strategy_configs_once(
        supabase_url: &str,
        supabase_key: &str,
        login_names: &[String],
    ) -> Result<Vec<StrategyConfig>> {
        let client = reqwest::blocking::Client::new();
        let mut configs = Vec::new();

        for login in login_names {
            // 1) strategy_settings by login_name
            let url = format!(
                "{}/rest/v1/strategy_settings?login_name=eq.{}&select=*",
                supabase_url, login
            );

            let resp = client
                .get(&url)
                .header("apikey", supabase_key)
                .header("Authorization", format!("Bearer {}", supabase_key))
                .header("Accept", "application/json")
                .send()?;

            if !resp.status().is_success() {
                error!(
                    "Failed to load strategy_settings for {}: HTTP {}",
                    login,
                    resp.status()
                );
                return Err(anyhow!(
                    "Failed to load strategy_settings for {}: HTTP {}",
                    login,
                    resp.status()
                ));
            }

            let rows: Vec<StrategySettingRow> = match resp.json() {
                Ok(rows) => {
                    info!(
                        "Strategy settings loaded for login_name={}: {:?}",
                        login, rows
                    );
                    rows
                }
                Err(e) => {
                    error!("Failed to parse strategy_settings for {}: {}", login, e);
                    return Err(anyhow!(
                        "Failed to parse strategy_settings for {}: {}",
                        login,
                        e
                    ));
                }
            };
            if rows.is_empty() {
                error!("No strategy_settings row found for login_name={}", login);
                return Err(anyhow!(
                    "No strategy_settings row found for login_name={}",
                    login
                ));
            }
            let strat_row = &rows[0];

            // 2) trade_settings by id, get JSON field trade_settings
            let trade_url = format!(
                "{}/rest/v1/trade_settings?id=eq.{}&select=*",
                supabase_url, strat_row.trade_setting
            );

            let resp = match client
                .get(&trade_url)
                .header("apikey", supabase_key)
                .header("Authorization", format!("Bearer {}", supabase_key))
                .header("Accept", "application/json")
                .send()
            {
                Ok(resp) => resp,
                Err(e) => {
                    error!("Failed to load trade_settings for {}: {}", login, e);
                    return Err(anyhow!(
                        "Failed to load trade_settings for {}: {}",
                        login,
                        e
                    ));
                }
            };

            if !resp.status().is_success() {
                error!(
                    "Failed to load trade_settings id={} for login_name={}: HTTP {}",
                    strat_row.trade_setting,
                    login,
                    resp.status()
                );
                return Err(anyhow!(
                    "Failed to load trade_settings id={} for login_name={}: HTTP {}",
                    strat_row.trade_setting,
                    login,
                    resp.status()
                ));
            }

            let trade_rows: Vec<TradeSettingsRow> = match resp.json() {
                Ok(rows) => {
                    info!("Trade settings loaded for login_name={}: {:?}", login, rows);
                    rows
                }
                Err(e) => {
                    error!("Failed to parse trade_settings for {}: {}", login, e);
                    return Err(anyhow!(
                        "Failed to parse trade_settings for {}: {}",
                        login,
                        e
                    ));
                }
            };
            if trade_rows.is_empty() {
                error!(
                    "No trade_settings row found for id={} (login_name={})",
                    strat_row.trade_setting, login
                );
                return Err(anyhow!(
                    "No trade_settings row found for id={} (login_name={})",
                    strat_row.trade_setting,
                    login
                ));
            }
            let trade_settings = trade_rows[0].trade_settings.clone();

            // 3) symbol_sets by id
            let symbol_url = format!(
                "{}/rest/v1/symbol_sets?id=eq.{}&select=*",
                supabase_url, strat_row.symbol_set
            );

            let resp = match client
                .get(&symbol_url)
                .header("apikey", supabase_key)
                .header("Authorization", format!("Bearer {}", supabase_key))
                .header("Accept", "application/json")
                .send()
            {
                Ok(resp) => resp,
                Err(e) => {
                    error!("Failed to load symbol_sets for {}: {}", login, e);
                    return Err(anyhow!("Failed to load symbol_sets for {}: {}", login, e));
                }
            };

            if !resp.status().is_success() {
                error!(
                    "Failed to load symbol_sets id={} for login_name={}: HTTP {}",
                    strat_row.symbol_set,
                    login,
                    resp.status()
                );
                return Err(anyhow!(
                    "Failed to load symbol_sets id={} for login_name={}: HTTP {}",
                    strat_row.symbol_set,
                    login,
                    resp.status()
                ));
            }

            let symbol_rows: Vec<SymbolSetRow> = match resp.json() {
                Ok(rows) => {
                    info!("Symbol sets loaded for login_name={}: {:?}", login, rows);
                    rows
                }
                Err(e) => {
                    error!("Failed to parse symbol_sets for {}: {}", login, e);
                    return Err(anyhow!("Failed to parse symbol_sets for {}: {}", login, e));
                }
            };
            if symbol_rows.is_empty() {
                error!(
                    "No symbol_sets row found for id={} (login_name={})",
                    strat_row.symbol_set, login
                );
                return Err(anyhow!(
                    "No symbol_sets row found for id={} (login_name={})",
                    strat_row.symbol_set,
                    login
                ));
            }
            let symbols = symbol_rows[0].symbols.clone();

            let trading_runtime = strat_row.trading_runtime.clone();
            let strategy_config =
                StrategyConfig::new(login.clone(), symbols, trade_settings, trading_runtime);
            configs.push(strategy_config);
        }

        Ok(configs)
    }
}

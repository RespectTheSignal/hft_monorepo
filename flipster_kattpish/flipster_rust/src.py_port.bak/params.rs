// Strategy tunables — mirrors Python flipster_latency_pick.py module-level constants.
// All units explicit (bp, ms, USD).

#[derive(Debug, Clone)]
pub struct StrategyParams {
    // ---- Entry ----
    /// Minimum cross_bp to consider entering.
    pub min_edge_bp: f64,
    /// Reject signals where cross_bp exceeds this (overshoot → revert).
    pub max_cross_bp: f64,
    /// Reject signals where either api_spread or web_spread exceeds this.
    pub entry_max_spread_bp: f64,
    /// Maximum staleness of web feed relative to api tick.
    pub max_web_age_ms: i64,

    // ---- Entry poll loop ----
    /// Safety cap on entry wait (ms).
    pub entry_timeout_ms: i64,
    /// Poll interval inside entry wait loop.
    pub entry_poll_ms: i64,
    /// If api-web gap drops below this, edge gone → cancel.
    pub entry_converge_bp: f64,

    // ---- Sizing ----
    pub amount_usd: f64,

    // ---- Exit (TP / stop / max_hold) ----
    pub min_hold_ms: i64,
    pub max_hold_ms: i64,
    /// Dynamic TP uses api_ask/bid as target — no additional min distance.
    pub use_dynamic_tp: bool,
    /// If in profit ≥ this bp after tp_fallback_age_ms, take market.
    pub tp_fallback_min_bp: f64,
    pub tp_fallback_age_ms: i64,
    /// Adverse move that triggers soft stop (bp below entry).
    pub stop_bp: f64,
    pub stop_confirm_ticks: i32,
    pub min_hold_ms_for_stop: i64,
    /// Hard stop — immediate MARKET close regardless of maker wait.
    pub hard_stop_bp: f64,

    // ---- Maker close ----
    pub maker_close_timeout_tp_ms: i64,
    pub maker_close_timeout_stop_ms: i64,

    // ---- Filter (orderbook-stale / ticker divergence) ----
    pub orderbook_stale_diverge_bp: f64,

    // ---- Rate limit ----
    pub rate_limit_per_min: i64,

    // ---- Post-loss blacklist ----
    pub blacklist_ms: i64,
}

impl Default for StrategyParams {
    fn default() -> Self {
        // Values lifted verbatim from flipster_latency_pick.py stalemaker_v3 tuning.
        Self {
            min_edge_bp: 1.0,
            max_cross_bp: 20.0,
            entry_max_spread_bp: 6.0,
            max_web_age_ms: 5_000,
            entry_timeout_ms: 30_000,
            entry_poll_ms: 100,
            entry_converge_bp: 0.5,
            amount_usd: 10.0,
            min_hold_ms: 3_000,
            max_hold_ms: 20_000,
            use_dynamic_tp: true,
            tp_fallback_min_bp: 1.0,
            tp_fallback_age_ms: 10_000,
            stop_bp: 5.0,
            stop_confirm_ticks: 1,
            min_hold_ms_for_stop: 2_000,
            hard_stop_bp: 15.0,
            maker_close_timeout_tp_ms: 3_000,
            maker_close_timeout_stop_ms: 1_000,
            orderbook_stale_diverge_bp: 2.0,
            rate_limit_per_min: 40,
            blacklist_ms: 20 * 60 * 1000,
        }
    }
}

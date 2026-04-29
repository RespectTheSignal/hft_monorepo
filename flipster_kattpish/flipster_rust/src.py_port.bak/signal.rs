// Signal detection — cross-book edge + filters. Mirrors Python on_api_update.

use crate::data_cache::SymbolSnapshot;
use crate::params::StrategyParams;
use crate::types::Side;

#[derive(Debug, Clone)]
pub struct SignalResult {
    pub side: Option<Side>,
    /// Positive bp; magnitude of api-vs-web cross on entry side.
    pub cross_bp: f64,
    /// (bid, ask) we should post at if side=Some(Buy) => web_ask, Sell => web_bid.
    pub maker_price: Option<f64>,
    /// api_ask (for Buy TP) or api_bid (for Sell TP).
    pub api_tp_target: Option<f64>,
    /// Filter that caused skip, if any (for observability).
    pub skip_reason: Option<&'static str>,
}

impl SignalResult {
    pub fn none(reason: &'static str) -> Self {
        Self {
            side: None,
            cross_bp: 0.0,
            maker_price: None,
            api_tp_target: None,
            skip_reason: Some(reason),
        }
    }

    pub fn has_signal(&self) -> bool {
        self.side.is_some()
    }
}

/// Cross-book signal detection.
///
/// Condition (cross-up, buy): api.bid > web.ask
///   → web ask is stale below current bid; post BUY @ web.ask.
/// Condition (cross-dn, sell): api.ask < web.bid
///   → web bid is stale above current ask; post SELL @ web.bid.
///
/// `now_ms` is wall clock for web_age check.
pub fn calculate_signal(
    snap: &SymbolSnapshot,
    now_ms: i64,
    params: &StrategyParams,
) -> SignalResult {
    let api = &snap.api_bt;
    let web = &snap.web_bt;
    if api.bid_price <= 0.0 || api.ask_price <= 0.0 || web.bid_price <= 0.0 || web.ask_price <= 0.0 {
        return SignalResult::none("invalid_price");
    }

    // Web staleness — use max(orderbook_ts, ticker_ts) as the effective web heartbeat.
    // Without ticker fallback, illiquid symbols sit stale in orderbook-v2 for 9+ seconds
    // (Python comment: `eff_web_ts = max(web_ts, tk_ts)`).
    let eff_web_ts = match snap.web_ticker_mid {
        Some((_, ts)) => web.server_time.max(ts),
        None => web.server_time,
    };
    let web_age_ms = api.server_time - eff_web_ts;
    if web_age_ms < 0 || web_age_ms > params.max_web_age_ms {
        return SignalResult::none("web_age");
    }
    let _ = now_ms; // reserved

    // Orderbook-stale divergence filter. If ticker midPrice diverges substantially
    // from orderbook mid, the orderbook bid/ask is stale — cross-book check is invalid.
    // Mirrors Python ORDERBOOK_STALE_DIVERGE_BP.
    let api_mid = api.mid();
    let web_mid = web.mid();
    if api_mid <= 0.0 || web_mid <= 0.0 {
        return SignalResult::none("invalid_mid");
    }
    if let Some((ticker_mid, _)) = snap.web_ticker_mid {
        if ticker_mid > 0.0 {
            let diverge_bp = (ticker_mid - web_mid).abs() / web_mid * 1.0e4;
            if diverge_bp > params.orderbook_stale_diverge_bp {
                return SignalResult::none("orderbook_stale");
            }
        }
    }

    // Spread filter.
    let api_sp = (api.ask_price - api.bid_price) / api_mid * 1.0e4;
    let web_sp = (web.ask_price - web.bid_price) / web_mid * 1.0e4;
    if api_sp.max(web_sp) > params.entry_max_spread_bp {
        return SignalResult::none("wide_spread");
    }

    // Cross detection
    let (side, cross_bp, maker_price, api_tp_target) = if api.bid_price > web.ask_price {
        let c = (api.bid_price - web.ask_price) / web_mid * 1.0e4;
        (Side::Buy, c, web.ask_price, api.ask_price)
    } else if api.ask_price < web.bid_price {
        let c = (web.bid_price - api.ask_price) / web_mid * 1.0e4;
        (Side::Sell, c, web.bid_price, api.bid_price)
    } else {
        return SignalResult::none("no_cross");
    };

    if cross_bp < params.min_edge_bp {
        return SignalResult::none("below_min_edge");
    }
    if cross_bp > params.max_cross_bp {
        return SignalResult::none("overshoot");
    }

    SignalResult {
        side: Some(side),
        cross_bp,
        maker_price: Some(maker_price),
        api_tp_target: Some(api_tp_target),
        skip_reason: None,
    }
}

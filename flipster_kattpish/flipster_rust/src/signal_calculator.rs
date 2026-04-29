// Signal calculation logic
// Calculates trading signals based on Gate.io and Binance bookticker data

use crate::data_cache::SymbolBooktickerSnapshot;
use crate::gate_order_manager::GateContract;
use crate::strategy_utils::get_valid_order_price;
use crate::types::{BookTickerC, TradeC};

use std::time::{SystemTime, UNIX_EPOCH};

use log::debug;

const EPS: f64 = 1e-12;

#[derive(Debug, Clone)]
pub struct SignalResult {
    pub order_side: Option<String>,
    pub order_price: Option<f64>,
    pub mid_gap_chance_bp: f64,
    pub spread_bp: f64,
    pub orderbook_size: f64,
    pub is_binance_valid: bool,
    pub binance_mid_gap_chance_bp: Option<f64>,
    pub gate_mid: f64,
    pub binance_mid: Option<f64>,
    pub gate_bid: f64,
    pub gate_ask: f64,
    pub binance_bid: Option<f64>,
    pub binance_ask: Option<f64>,
    pub gate_web_bid: f64,
    pub gate_web_ask: f64,
    pub gate_contract: GateContract,
    /// 1m Redis (`gate:{quote}:1`): mean gate vs quote mid gap.
    pub gap_1m_gate_vs_quote: Option<f64>,
    /// 1m Redis (`gate_web:{quote}:1`): mean gate_web vs quote mid gap.
    pub gap_1m_gate_web_vs_quote: Option<f64>,
    /// 1m Redis spread pair: mean relative spread on `gate_bookticker`.
    pub spread_1m_gate_bookticker: Option<f64>,
    /// 1m Redis spread pair: mean relative spread on `gate_webbookticker`.
    pub spread_1m_gate_webbookticker: Option<f64>,
    /// 1m Pearson correlation: gate_bookticker mid vs quote mid.
    pub corr_gate_bookticker_vs_quote: Option<f64>,
    /// 1m Pearson correlation: gate_webbookticker mid vs quote mid.
    pub corr_gate_webbookticker_vs_quote: Option<f64>,
    pub snapshot: SymbolBooktickerSnapshot,
    pub gate_last_trade: Option<TradeC>,
}

impl SignalResult {
    pub fn has_signal(&self) -> bool {
        self.order_side.is_some() && self.order_price.is_some()
    }

    /// Fills 1m market stats from [`crate::market_watcher::MarketGapState`] (Redis-fed).
    pub fn apply_market_gap_1m(
        &mut self,
        gap: &crate::market_watcher::MarketGapState,
        symbol: &str,
    ) {
        self.gap_1m_gate_vs_quote = gap.get_gap_1m_gate_vs_quote(symbol);
        self.gap_1m_gate_web_vs_quote = gap.get_gap_1m_gate_web_vs_quote(symbol);
        self.spread_1m_gate_bookticker = gap.get_spread_1m_gate_bookticker(symbol);
        self.spread_1m_gate_webbookticker = gap.get_spread_1m_gate_webbookticker(symbol);
        self.corr_gate_bookticker_vs_quote = gap.get_corr_gate_bookticker_vs_quote(symbol);
        self.corr_gate_webbookticker_vs_quote = gap.get_corr_gate_webbookticker_vs_quote(symbol);
    }
}

/// Calculate trading signal from Gate.io and Binance bookticker data
///
/// Logic:
/// - buy_price = gate_web_ask (best ask from web bookticker)
/// - sell_price = gate_web_bid (best bid from web bookticker)
/// - If buy_price < gate_mid: buy signal
/// - If sell_price > gate_mid: sell signal
/// - Binance validation: buy signal is valid if order_price < binance_bid
///                      sell signal is valid if order_price > binance_ask
pub fn calculate_signal(
    snapshot: &SymbolBooktickerSnapshot,
    contract: &GateContract,
) -> SignalResult {
    let gate_bt: &BookTickerC = &snapshot.gate_bt;
    let gate_web_bt: &BookTickerC = &snapshot.gate_web_bt;
    let binance_bt: Option<&BookTickerC> = snapshot.base_bt.as_ref();
    // Extract prices from Gate bookticker
    let gate_ask = gate_bt.ask_price;
    let gate_bid = gate_bt.bid_price;
    let gate_mid = (gate_ask + gate_bid) * 0.5;

    // Extract prices from Gate web bookticker
    let gate_web_ask = gate_web_bt.ask_price;
    let gate_web_bid = gate_web_bt.bid_price;

    // Extract prices from Binance bookticker
    let bin_ask = if let Some(binance_bt) = binance_bt {
        Some(binance_bt.ask_price)
    } else {
        None
    };
    let bin_bid = if let Some(binance_bt) = binance_bt {
        Some(binance_bt.bid_price)
    } else {
        None
    };
    let bin_mid = if let (Some(bin_ask), Some(bin_bid)) = (bin_ask, bin_bid) {
        Some((bin_ask + bin_bid) / 2.0)
    } else {
        None
    };
    // Calculate potential buy/sell prices
    let buy_price = gate_web_ask;
    let sell_price = gate_web_bid;

    // Initialize result
    let mut order_side = None;
    let mut order_price = None;
    let mut mid_gap_chance_bp = 0.0;
    let mut orderbook_size = 0.0;
    let mut binance_mid_gap_chance_bp = None;
    // Determine order side and price based on mid gap
    if buy_price < gate_mid {
        // Buy signal: web ask is below Gate mid price
        order_side = Some("buy".to_string());
        mid_gap_chance_bp = (gate_mid - buy_price) / (gate_mid + EPS) * 10_000.0;
        orderbook_size = gate_web_bt.ask_size;
        order_price = Some(buy_price);
        binance_mid_gap_chance_bp = if let Some(bin_mid) = bin_mid {
            Some((bin_mid - buy_price) / (bin_mid + EPS) * 10_000.0)
        } else {
            None
        };
    } else if sell_price > gate_mid {
        // Sell signal: web bid is above Gate mid price
        order_side = Some("sell".to_string());
        mid_gap_chance_bp = (sell_price - gate_mid) / (gate_mid + EPS) * 10_000.0;
        orderbook_size = gate_web_bt.bid_size;
        order_price = Some(sell_price);
        binance_mid_gap_chance_bp = if let Some(bin_mid) = bin_mid {
            Some((sell_price - bin_mid) / (bin_mid + EPS) * 10_000.0)
        } else {
            None
        };
    }
    // else: no signal (buy_price >= gate_mid && sell_price <= gate_mid)

    // Calculate spread in basis points
    let spread_bp = (gate_ask - gate_bid) / (gate_mid + EPS) * 10_000.0;

    // Validate against Binance prices
    let is_binance_valid = if let (Some(side), Some(price), Some(bin_bid), Some(bin_ask)) =
        (order_side.as_deref(), order_price, bin_bid, bin_ask)
    {
        match side {
            "buy" => price < bin_bid, // Buy signal is valid if price is below Binance bid
            "sell" => price > bin_ask, // Sell signal is valid if price is above Binance ask
            _ => false,
        }
    } else {
        false
    };

    let orderbook_size = if let Some(side) = order_side.as_ref() {
        if side == "buy" {
            gate_web_bt.ask_size
        } else if side == "sell" {
            gate_web_bt.bid_size
        } else {
            0.0
        }
    } else {
        0.0
    } as f64;

    // gate web bookticker should be in between gate bookticker
    let is_gate_web_bookticker_in_between_gate_bookticker =
        (gate_web_bid >= gate_bid - EPS) && (gate_web_ask <= gate_ask + EPS);
    if !is_gate_web_bookticker_in_between_gate_bookticker {
        return SignalResult {
            order_side: None,
            order_price: None,
            mid_gap_chance_bp: 0.0,
            spread_bp: 0.0,
            orderbook_size: 0.0,
            is_binance_valid: false,
            binance_mid_gap_chance_bp: None,
            gate_mid: gate_mid,
            binance_mid: bin_mid,
            gate_bid: gate_bid,
            gate_ask: gate_ask,
            binance_bid: bin_bid,
            binance_ask: bin_ask,
            gate_web_bid: gate_web_bid,
            gate_web_ask: gate_web_ask,
            gate_contract: contract.clone(),
            gap_1m_gate_vs_quote: None,
            gap_1m_gate_web_vs_quote: None,
            spread_1m_gate_bookticker: None,
            spread_1m_gate_webbookticker: None,
            corr_gate_bookticker_vs_quote: None,
            corr_gate_webbookticker_vs_quote: None,
            snapshot: snapshot.clone(),
            gate_last_trade: None,
        };
    }

    SignalResult {
        order_side,
        order_price,
        mid_gap_chance_bp,
        spread_bp,
        orderbook_size,
        is_binance_valid,
        binance_mid_gap_chance_bp,
        gate_mid,
        binance_mid: bin_mid,
        gate_bid: gate_bid,
        gate_ask: gate_ask,
        binance_bid: bin_bid,
        binance_ask: bin_ask,
        gate_web_bid: gate_web_bid,
        gate_web_ask: gate_web_ask,
        gate_contract: contract.clone(),
        gap_1m_gate_vs_quote: None,
        gap_1m_gate_web_vs_quote: None,
        spread_1m_gate_bookticker: None,
        spread_1m_gate_webbookticker: None,
        corr_gate_bookticker_vs_quote: None,
        corr_gate_webbookticker_vs_quote: None,
        snapshot: snapshot.clone(),
        gate_last_trade: None,
    }
}

pub fn calculate_signal_v8(
    snapshot: &SymbolBooktickerSnapshot,
    contract: &GateContract,
    gate_last_trade: Option<TradeC>,
    market_state: &crate::market_watcher::MarketGapState,
) -> SignalResult {
    let gate_bt: &BookTickerC = &snapshot.gate_bt;
    let gate_web_bt: &BookTickerC = &snapshot.gate_web_bt;
    let binance_bt: Option<&BookTickerC> = snapshot.base_bt.as_ref();
    // Extract prices from Gate bookticker
    let gate_ask = gate_bt.ask_price;
    let gate_bid = gate_bt.bid_price;
    let gate_mid = (gate_ask + gate_bid) * 0.5;

    // Extract prices from Gate web bookticker
    let mut gate_web_ask = gate_web_bt.ask_price;
    let mut gate_web_bid = gate_web_bt.bid_price;

    // Extract prices from Binance bookticker
    let bin_ask = if let Some(binance_bt) = binance_bt {
        Some(binance_bt.ask_price)
    } else {
        None
    };
    let bin_bid = if let Some(binance_bt) = binance_bt {
        Some(binance_bt.bid_price)
    } else {
        None
    };
    let bin_mid = if let (Some(bin_ask), Some(bin_bid)) = (bin_ask, bin_bid) {
        Some((bin_ask + bin_bid) / 2.0)
    } else {
        None
    };

    let is_gate_web_bookticker_in_between_gate_bookticker =
        (gate_web_bid >= gate_bid - EPS) && (gate_web_ask <= gate_ask + EPS);

    let current_time_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let gate_bt_age = current_time_ms - snapshot.gate_bt_received_at_ms;

    // In gate_hft the web feed is usually INSIDE the api feed's bid/ask. In Flipster
    // the web feed lags the api feed — when price moves, web sits OUTSIDE. Skip
    // this "in-between" gate in Flipster mode so cross-book signals can emit.
    let flipster_mode = crate::flipster_cookie_store::global().is_some();
    if !flipster_mode
        && !is_gate_web_bookticker_in_between_gate_bookticker
        && gate_bt_age < 3
    {
        return SignalResult {
            order_side: None,
            order_price: None,
            mid_gap_chance_bp: 0.0,
            spread_bp: 0.0,
            orderbook_size: 0.0,
            is_binance_valid: false,
            binance_mid_gap_chance_bp: None,
            gate_mid: gate_mid,
            binance_mid: bin_mid,
            gate_bid: gate_bid,
            gate_ask: gate_ask,
            binance_bid: bin_bid,
            binance_ask: bin_ask,
            gate_web_bid: gate_web_bid,
            gate_web_ask: gate_web_ask,
            gate_contract: contract.clone(),
            gap_1m_gate_vs_quote: None,
            gap_1m_gate_web_vs_quote: None,
            spread_1m_gate_bookticker: None,
            spread_1m_gate_webbookticker: None,
            corr_gate_bookticker_vs_quote: None,
            corr_gate_webbookticker_vs_quote: None,
            snapshot: snapshot.clone(),
            gate_last_trade: gate_last_trade.clone(),
        };
    }

    if snapshot.gate_bt.server_time > snapshot.gate_web_bt.server_time {
        if gate_ask < gate_web_ask {
            gate_web_ask = gate_ask;
        }
        if gate_bid > gate_web_bid {
            gate_web_bid = gate_bid;
        }
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // let gate_bt_latency = now_ms - gate_bt.server_time;
    let gate_web_bt_latency = now_ms - gate_web_bt.server_time;
    let gate_bt_latency = now_ms - gate_bt.server_time;

    // gate web bookticker should be in between gate bookticker
    let is_gate_web_bookticker_in_between_gate_bookticker =
        (gate_web_bid >= gate_bid - EPS) && (gate_web_ask <= gate_ask + EPS);

    if gate_web_bt.event_time > 0
        && gate_bt.event_time > gate_web_bt.event_time
        && gate_bt_latency < 30
    {
        let gate_event_time_gap = gate_bt.event_time - gate_web_bt.event_time;
        debug!(
            "{}: gate_event_time_gap: web_bid={} web_ask={} gate_bid={} gate_ask={} gap={} ms",
            contract.name, gate_web_bid, gate_web_ask, gate_bid, gate_ask, gate_event_time_gap
        );
    }

    if gate_web_bt.event_time > gate_bt.event_time
        && !is_gate_web_bookticker_in_between_gate_bookticker
        && !crate::flipster_cookie_store::global().is_some()
    {
        debug!(
            "{}: gate_web_bt.event_time < gate_bt.event_time: web_bid={} web_ask={} gate_bid={} gate_ask={}",
            contract.name, gate_web_bid, gate_web_ask, gate_bid, gate_ask,
        );
        return SignalResult {
            order_side: None,
            order_price: None,
            mid_gap_chance_bp: 0.0,
            spread_bp: 0.0,
            orderbook_size: 0.0,
            is_binance_valid: false,
            binance_mid_gap_chance_bp: None,
            gate_mid: gate_mid,
            binance_mid: bin_mid,
            gate_bid: gate_bid,
            gate_ask: gate_ask,
            binance_bid: bin_bid,
            binance_ask: bin_ask,
            gate_web_bid: gate_web_bid,
            gate_web_ask: gate_web_ask,
            gate_contract: contract.clone(),
            gap_1m_gate_vs_quote: None,
            gap_1m_gate_web_vs_quote: None,
            spread_1m_gate_bookticker: None,
            spread_1m_gate_webbookticker: None,
            corr_gate_bookticker_vs_quote: None,
            corr_gate_webbookticker_vs_quote: None,
            snapshot: snapshot.clone(),
            gate_last_trade: gate_last_trade.clone(),
        };
    }

    // if let Some(gate_last_trade) = gate_last_trade {
    //     let is_trade_more_recent_than_webbook_ticker =
    //         gate_last_trade.create_time_ms > gate_web_bt.event_time;
    //     let last_trade_size = gate_last_trade.size;
    //     let last_trade_price = gate_last_trade.price;

    //     if is_trade_more_recent_than_webbook_ticker {
    //         if last_trade_size > 0.0 {
    //             gate_web_ask = last_trade_price;
    //         } else {
    //             gate_web_bid = last_trade_price;
    //         }
    //     } else {
    //         gate_web_ask = gate_web_bt.ask_price;
    //         gate_web_bid = gate_web_bt.bid_price;
    //     }
    // }

    // Calculate potential buy/sell prices
    let buy_price = gate_web_ask;
    let sell_price = gate_web_bid;

    // Initialize result
    let mut order_side = None;
    let mut order_price = None;
    let mut mid_gap_chance_bp = 0.0;
    let mut binance_mid_gap_chance_bp = None;
    // Determine order side and price based on mid gap
    if buy_price < gate_mid {
        // Buy signal: web ask is below Gate mid price
        order_side = Some("buy".to_string());
        mid_gap_chance_bp = (gate_mid - buy_price) / (gate_mid + EPS) * 10_000.0;
        order_price = Some(buy_price);
        binance_mid_gap_chance_bp = if let Some(bin_mid) = bin_mid {
            Some((bin_mid - buy_price) / (bin_mid + EPS) * 10_000.0)
        } else {
            None
        };
    } else if sell_price > gate_mid {
        // Sell signal: web bid is above Gate mid price
        order_side = Some("sell".to_string());
        mid_gap_chance_bp = (sell_price - gate_mid) / (gate_mid + EPS) * 10_000.0;
        order_price = Some(sell_price);
        binance_mid_gap_chance_bp = if let Some(bin_mid) = bin_mid {
            Some((sell_price - bin_mid) / (bin_mid + EPS) * 10_000.0)
        } else {
            None
        };
    } else if let Some(bin_mid) = bin_mid {
        let gate_web_gap_240m = market_state.get_gap_240m_gate_web_vs_quote(contract.name.as_str());
        let gate_web_gap_720m = market_state.get_gap_720m_gate_web_vs_quote(contract.name.as_str());

        if let (Some(gate_web_gap_240m), Some(gate_web_gap_720m)) =
            (gate_web_gap_240m, gate_web_gap_720m)
        {
            let gate_web_mid = (gate_web_ask + gate_web_bid) * 0.5;
            let current_web_mid_gap_bp = (gate_web_mid - bin_mid) / (bin_mid + EPS) * 10_000.0;
            let gate_web_gap_240m_bp = gate_web_gap_240m * 10000.0;
            let gate_web_gap_720m_bp = gate_web_gap_720m * 10000.0;

            if current_web_mid_gap_bp < gate_web_gap_240m_bp
                && current_web_mid_gap_bp < gate_web_gap_720m_bp
            {
                order_side = Some("buy".to_string());
                order_price = Some(buy_price);
            } else if current_web_mid_gap_bp > gate_web_gap_240m_bp
                && current_web_mid_gap_bp > gate_web_gap_720m_bp
            {
                order_side = Some("sell".to_string());
                order_price = Some(sell_price);
            }
        }
    }
    // else: no signal (buy_price >= gate_mid && sell_price <= gate_mid)

    // Calculate spread in basis points
    let spread_bp = (gate_ask - gate_bid) / (gate_mid + EPS) * 10_000.0;

    // Validate against Binance prices
    let is_binance_valid = if let (Some(side), Some(price), Some(bin_bid), Some(bin_ask)) =
        (order_side.as_deref(), order_price, bin_bid, bin_ask)
    {
        match side {
            "buy" => price < bin_bid, // Buy signal is valid if price is below Binance bid
            "sell" => price > bin_ask, // Sell signal is valid if price is above Binance ask
            _ => false,
        }
    } else {
        false
    };

    let orderbook_size = if let Some(side) = order_side.as_ref() {
        if side == "buy" {
            gate_web_bt.ask_size
        } else if side == "sell" {
            gate_web_bt.bid_size
        } else {
            0.0
        }
    } else {
        0.0
    } as f64;

    // gate web bookticker should be in between gate bookticker
    let is_gate_web_bookticker_in_between_gate_bookticker =
        (gate_web_bid >= gate_bid - EPS) && (gate_web_ask <= gate_ask + EPS);

    // let current_time_ms = SystemTime::now()
    //     .duration_since(UNIX_EPOCH)
    //     .unwrap_or_default()
    //     .as_millis() as i64;
    // let gate_bt_latency = current_time_ms - gate_bt.server_time;

    if !is_gate_web_bookticker_in_between_gate_bookticker
        && !flipster_mode
    // && (gate_web_bt.server_time >= gate_bt.server_time || gate_bt_latency > 50)
    {
        debug!(
            "{}: gate_web_bt_latency={}ms is_gate_web_bookticker_in_between_gate_bookticker={}",
            contract.name, gate_web_bt_latency, is_gate_web_bookticker_in_between_gate_bookticker
        );

        return SignalResult {
            order_side: None,
            order_price: None,
            mid_gap_chance_bp: 0.0,
            spread_bp: 0.0,
            orderbook_size: 0.0,
            is_binance_valid: false,
            binance_mid_gap_chance_bp: None,
            gate_mid: gate_mid,
            binance_mid: bin_mid,
            gate_bid: gate_bid,
            gate_ask: gate_ask,
            binance_bid: bin_bid,
            binance_ask: bin_ask,
            gate_web_bid: gate_web_bid,
            gate_web_ask: gate_web_ask,
            gate_contract: contract.clone(),
            gap_1m_gate_vs_quote: None,
            gap_1m_gate_web_vs_quote: None,
            spread_1m_gate_bookticker: None,
            spread_1m_gate_webbookticker: None,
            corr_gate_bookticker_vs_quote: None,
            corr_gate_webbookticker_vs_quote: None,
            snapshot: snapshot.clone(),
            gate_last_trade: gate_last_trade.clone(),
        };
    }

    SignalResult {
        order_side,
        order_price,
        mid_gap_chance_bp,
        spread_bp,
        orderbook_size,
        is_binance_valid,
        binance_mid_gap_chance_bp,
        gate_mid,
        binance_mid: bin_mid,
        gate_bid: gate_bid,
        gate_ask: gate_ask,
        binance_bid: bin_bid,
        binance_ask: bin_ask,
        gate_web_bid: gate_web_bid,
        gate_web_ask: gate_web_ask,
        gate_contract: contract.clone(),
        gap_1m_gate_vs_quote: None,
        gap_1m_gate_web_vs_quote: None,
        spread_1m_gate_bookticker: None,
        spread_1m_gate_webbookticker: None,
        corr_gate_bookticker_vs_quote: None,
        corr_gate_webbookticker_vs_quote: None,
        snapshot: snapshot.clone(),
        gate_last_trade: None,
    }
}

pub fn calculate_signal_v10(
    snapshot: &SymbolBooktickerSnapshot,
    contract: &GateContract,
    gate_last_trade: Option<TradeC>,
    avg_mid_gap: Option<f64>,
) -> SignalResult {
    let gate_bt: &BookTickerC = &snapshot.gate_bt;
    let gate_web_bt: &BookTickerC = &snapshot.gate_web_bt;
    let binance_bt: Option<&BookTickerC> = snapshot.base_bt.as_ref();
    // Extract prices from Gate bookticker
    let gate_ask = gate_bt.ask_price;
    let gate_bid = gate_bt.bid_price;
    let gate_mid = (gate_ask + gate_bid) * 0.5;

    // Extract prices from Gate web bookticker
    let mut gate_web_ask = gate_web_bt.ask_price;
    let mut gate_web_bid = gate_web_bt.bid_price;

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    // let gate_bt_latency = now_ms - gate_bt.server_time;
    let gate_web_bt_latency = now_ms - gate_web_bt.server_time;
    let gate_bt_latency = now_ms - gate_bt.server_time;

    // gate web bookticker should be in between gate bookticker
    let is_gate_web_bookticker_in_between_gate_bookticker =
        (gate_web_bid >= gate_bid - EPS) && (gate_web_ask <= gate_ask + EPS);

    if gate_web_bt.event_time > 0
        && gate_bt.event_time > gate_web_bt.event_time
        && gate_bt_latency < 30
    {
        let gate_event_time_gap = gate_bt.event_time - gate_web_bt.event_time;
        debug!(
            "{}: gate_event_time_gap: web_bid={} web_ask={} gate_bid={} gate_ask={} gap={} ms",
            contract.name, gate_web_bid, gate_web_ask, gate_bid, gate_ask, gate_event_time_gap
        );
    }

    if gate_web_bt.event_time > gate_bt.event_time
        && !is_gate_web_bookticker_in_between_gate_bookticker
        && !crate::flipster_cookie_store::global().is_some()
    {
        debug!(
            "{}: gate_web_bt.event_time < gate_bt.event_time: web_bid={} web_ask={} gate_bid={} gate_ask={}",
            contract.name, gate_web_bid, gate_web_ask, gate_bid, gate_ask,
        );
        return SignalResult {
            order_side: None,
            order_price: None,
            mid_gap_chance_bp: 0.0,
            spread_bp: 0.0,
            orderbook_size: 0.0,
            is_binance_valid: false,
            binance_mid_gap_chance_bp: None,
            gate_mid: gate_mid,
            binance_mid: None,
            gate_bid: gate_bid,
            gate_ask: gate_ask,
            binance_bid: None,
            binance_ask: None,
            gate_web_bid: gate_web_bid,
            gate_web_ask: gate_web_ask,
            gate_contract: contract.clone(),
            gap_1m_gate_vs_quote: None,
            gap_1m_gate_web_vs_quote: None,
            spread_1m_gate_bookticker: None,
            spread_1m_gate_webbookticker: None,
            corr_gate_bookticker_vs_quote: None,
            corr_gate_webbookticker_vs_quote: None,
            snapshot: snapshot.clone(),
            gate_last_trade: gate_last_trade.clone(),
        };
    }
    // if let Some(gate_last_trade) = gate_last_trade {
    //     let is_trade_more_recent_than_webbook_ticker =
    //         gate_last_trade.create_time_ms > gate_web_bt.event_time;
    //     let last_trade_size = gate_last_trade.size;
    //     let last_trade_price = gate_last_trade.price;

    //     if is_trade_more_recent_than_webbook_ticker {
    //         if last_trade_size > 0.0 {
    //             gate_web_ask = last_trade_price;
    //         } else {
    //             gate_web_bid = last_trade_price;
    //         }
    //     } else {
    //         gate_web_ask = gate_web_bt.ask_price;
    //         gate_web_bid = gate_web_bt.bid_price;
    //     }
    // }

    // Extract prices from Binance bookticker
    let bin_ask = if let Some(binance_bt) = binance_bt {
        Some(binance_bt.ask_price)
    } else {
        None
    };
    let bin_bid = if let Some(binance_bt) = binance_bt {
        Some(binance_bt.bid_price)
    } else {
        None
    };
    // use normalized bin mid instead of bin mid
    // let bin_mid = if let (Some(bin_ask), Some(bin_bid)) = (bin_ask, bin_bid) {
    //     Some((bin_ask + bin_bid) / 2.0)
    // } else {
    //     None
    // };
    // Calculate potential buy/sell prices
    let buy_price = gate_web_ask;
    let sell_price = gate_web_bid;

    // Initialize result
    let mut order_side = None;
    let mut order_price = None;
    let mut mid_gap_chance_bp = 0.0;
    let mut orderbook_size = 0.0;
    let mut binance_mid_gap_chance_bp = None;

    let normalized_bin_bid = if let Some(bin_bid) = bin_bid {
        Some(bin_bid * (1.0 + avg_mid_gap.unwrap_or(0.0)))
    } else {
        None
    };
    let normalized_bin_ask = if let Some(bin_ask) = bin_ask {
        Some(bin_ask * (1.0 + avg_mid_gap.unwrap_or(0.0)))
    } else {
        None
    };

    let normalized_bin_mid = if let (Some(normalized_bin_bid), Some(normalized_bin_ask)) =
        (normalized_bin_bid, normalized_bin_ask)
    {
        Some((normalized_bin_bid + normalized_bin_ask) / 2.0)
    } else {
        None
    };

    // Determine order side and price based on mid gap
    if buy_price < gate_mid {
        // Buy signal: web ask is below Gate mid price
        order_side = Some("buy".to_string());
        mid_gap_chance_bp = (gate_mid - buy_price) / (gate_mid + EPS) * 10_000.0;
        orderbook_size = gate_web_bt.ask_size;
        order_price = Some(buy_price);
        binance_mid_gap_chance_bp = if let Some(normalized_bin_mid) = normalized_bin_mid {
            Some((normalized_bin_mid - buy_price) / (normalized_bin_mid + EPS) * 10_000.0)
        } else {
            None
        };
    } else if sell_price > gate_mid {
        // Sell signal: web bid is above Gate mid price
        order_side = Some("sell".to_string());
        mid_gap_chance_bp = (sell_price - gate_mid) / (gate_mid + EPS) * 10_000.0;
        orderbook_size = gate_web_bt.bid_size;
        order_price = Some(sell_price);
        binance_mid_gap_chance_bp = if let Some(normalized_bin_mid) = normalized_bin_mid {
            Some((sell_price - normalized_bin_mid) / (normalized_bin_mid + EPS) * 10_000.0)
        } else {
            None
        };
    }
    // else: no signal (buy_price >= gate_mid && sell_price <= gate_mid)

    // Calculate spread in basis points
    let spread_bp = (gate_ask - gate_bid) / (gate_mid + EPS) * 10_000.0;

    // Validate against Binance prices
    let is_binance_valid =
        if let (Some(side), Some(price), Some(normalized_bin_bid), Some(normalized_bin_ask)) = (
            order_side.as_deref(),
            order_price,
            normalized_bin_bid,
            normalized_bin_ask,
        ) {
            match side {
                "buy" => price < normalized_bin_bid, // Buy signal is valid if price is below Binance bid
                "sell" => price > normalized_bin_ask, // Sell signal is valid if price is above Binance ask
                _ => false,
            }
        } else {
            false
        };

    let current_time_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let gate_bt_latency = current_time_ms - gate_bt.server_time;

    if !is_gate_web_bookticker_in_between_gate_bookticker
    // && (gate_web_bt.server_time >= gate_bt.server_time || gate_bt_latency > 50)
    {
        return SignalResult {
            order_side: None,
            order_price: None,
            mid_gap_chance_bp: 0.0,
            spread_bp: 0.0,
            orderbook_size: 0.0,
            is_binance_valid: false,
            binance_mid_gap_chance_bp: None,
            gate_mid: gate_mid,
            binance_mid: normalized_bin_mid,
            gate_bid: gate_bid,
            gate_ask: gate_ask,
            binance_bid: normalized_bin_bid,
            binance_ask: normalized_bin_ask,
            gate_web_bid: gate_web_bid,
            gate_web_ask: gate_web_ask,
            gate_contract: contract.clone(),
            gap_1m_gate_vs_quote: None,
            gap_1m_gate_web_vs_quote: None,
            spread_1m_gate_bookticker: None,
            spread_1m_gate_webbookticker: None,
            corr_gate_bookticker_vs_quote: None,
            corr_gate_webbookticker_vs_quote: None,
            snapshot: snapshot.clone(),
            gate_last_trade: gate_last_trade.clone(),
        };
    }

    SignalResult {
        order_side,
        order_price,
        mid_gap_chance_bp,
        spread_bp,
        orderbook_size,
        is_binance_valid,
        binance_mid_gap_chance_bp,
        gate_mid,
        binance_mid: normalized_bin_mid,
        gate_bid: gate_bid,
        gate_ask: gate_ask,
        binance_bid: normalized_bin_bid,
        binance_ask: normalized_bin_ask,
        gate_web_bid: gate_web_bid,
        gate_web_ask: gate_web_ask,
        gate_contract: contract.clone(),
        gap_1m_gate_vs_quote: None,
        gap_1m_gate_web_vs_quote: None,
        spread_1m_gate_bookticker: None,
        spread_1m_gate_webbookticker: None,
        corr_gate_bookticker_vs_quote: None,
        corr_gate_webbookticker_vs_quote: None,
        snapshot: snapshot.clone(),
        gate_last_trade: gate_last_trade.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_signal_buy() {
        let gate_bt = BookTickerC {
            exchange: [0; 16],
            symbol: [0; 32],
            bid_price: 50000.0,
            ask_price: 50010.0,
            bid_size: 1.0,
            ask_size: 1.0,
            event_time: 0,
            server_time: 0,
            publisher_sent_ms: 0,
            subscriber_received_ms: 0,
            subscriber_dump_ms: 0,
        };

        let gate_web_bt = BookTickerC {
            exchange: [0; 16],
            symbol: [0; 32],
            bid_price: 49990.0,
            ask_price: 49995.0, // Below gate_mid (50005)
            bid_size: 1.0,
            ask_size: 2.0,
            event_time: 0,
            server_time: 0,
            publisher_sent_ms: 0,
            subscriber_received_ms: 0,
            subscriber_dump_ms: 0,
        };

        let binance_bt = BookTickerC {
            exchange: [0; 16],
            symbol: [0; 32],
            bid_price: 50000.0,
            ask_price: 50010.0,
            bid_size: 1.0,
            ask_size: 1.0,
            event_time: 0,
            server_time: 0,
            publisher_sent_ms: 0,
            subscriber_received_ms: 0,
            subscriber_dump_ms: 0,
        };

        let contract = GateContract {
            name: "BTC_USDT".to_string(),
            contract_type: "futures".to_string(),
            quanto_multiplier: "1.0".to_string(),
            leverage_min: "1".to_string(),
            leverage_max: "100".to_string(),
            mark_price: "100000.0".to_string(),
            funding_rate: "0.01".to_string(),
            order_size_min: 1,
            order_size_max: 100,
            order_price_round: "0.01".to_string(),
        };

        let snapshot = SymbolBooktickerSnapshot {
            gate_bt: gate_bt.clone(),
            gate_web_bt: gate_web_bt.clone(),
            base_bt: Some(binance_bt.clone()),
            gate_previous_bt: None,
            gate_bt_received_at_ms: 0,
            gate_web_bt_received_at_ms: 0,
            base_bt_received_at_ms: 0,
        };
        let result = calculate_signal(&snapshot, &contract);

        assert_eq!(result.order_side, Some("buy".to_string()));
        assert_eq!(result.order_price, Some(49995.0));
        assert!(result.mid_gap_chance_bp > 0.0);
        assert_eq!(result.orderbook_size, 2.0);
        assert!(result.is_binance_valid); // 49995 < 50000 (bin_bid)
        assert!(result.binance_mid_gap_chance_bp.is_some());
    }

    #[test]
    fn test_calculate_signal_sell() {
        let gate_bt = BookTickerC {
            exchange: [0; 16],
            symbol: [0; 32],
            bid_price: 50000.0,
            ask_price: 50010.0,
            bid_size: 1.0,
            ask_size: 1.0,
            event_time: 0,
            server_time: 0,
            publisher_sent_ms: 0,
            subscriber_received_ms: 0,
            subscriber_dump_ms: 0,
        };

        let gate_web_bt = BookTickerC {
            exchange: [0; 16],
            symbol: [0; 32],
            bid_price: 50015.0, // Above gate_mid (50005)
            ask_price: 50020.0,
            bid_size: 2.0,
            ask_size: 1.0,
            event_time: 0,
            server_time: 0,
            publisher_sent_ms: 0,
            subscriber_received_ms: 0,
            subscriber_dump_ms: 0,
        };

        let binance_bt = BookTickerC {
            exchange: [0; 16],
            symbol: [0; 32],
            bid_price: 50000.0,
            ask_price: 50010.0,
            bid_size: 1.0,
            ask_size: 1.0,
            event_time: 0,
            server_time: 0,
            publisher_sent_ms: 0,
            subscriber_received_ms: 0,
            subscriber_dump_ms: 0,
        };

        let contract = GateContract {
            name: "BTC_USDT".to_string(),
            contract_type: "futures".to_string(),
            quanto_multiplier: "1.0".to_string(),
            leverage_min: "1".to_string(),
            leverage_max: "100".to_string(),
            mark_price: "100000.0".to_string(),
            funding_rate: "0.01".to_string(),
            order_size_min: 1,
            order_size_max: 100,
            order_price_round: "0.01".to_string(),
        };
        let snapshot = SymbolBooktickerSnapshot {
            gate_bt: gate_bt.clone(),
            gate_web_bt: gate_web_bt.clone(),
            base_bt: Some(binance_bt.clone()),
            gate_previous_bt: None,
            gate_bt_received_at_ms: 0,
            gate_web_bt_received_at_ms: 0,
            base_bt_received_at_ms: 0,
        };
        let result = calculate_signal(&snapshot, &contract);

        assert_eq!(result.order_side, Some("sell".to_string()));
        assert_eq!(result.order_price, Some(50015.0));
        assert!(result.mid_gap_chance_bp > 0.0);
        assert_eq!(result.orderbook_size, 2.0);
        assert!(result.is_binance_valid); // 50015 > 50010 (bin_ask)
        assert!(result.binance_mid_gap_chance_bp.is_some());
    }

    #[test]
    fn test_calculate_signal_no_signal() {
        let gate_bt = BookTickerC {
            exchange: [0; 16],
            symbol: [0; 32],
            bid_price: 50000.0,
            ask_price: 50010.0,
            bid_size: 1.0,
            ask_size: 1.0,
            event_time: 0,
            server_time: 0,
            publisher_sent_ms: 0,
            subscriber_received_ms: 0,
            subscriber_dump_ms: 0,
        };

        let gate_web_bt = BookTickerC {
            exchange: [0; 16],
            symbol: [0; 32],
            bid_price: 50000.0,
            ask_price: 50010.0, // No gap
            bid_size: 1.0,
            ask_size: 1.0,
            event_time: 0,
            server_time: 0,
            publisher_sent_ms: 0,
            subscriber_received_ms: 0,
            subscriber_dump_ms: 0,
        };

        let binance_bt = BookTickerC {
            exchange: [0; 16],
            symbol: [0; 32],
            bid_price: 50000.0,
            ask_price: 50010.0,
            bid_size: 1.0,
            ask_size: 1.0,
            event_time: 0,
            server_time: 0,
            publisher_sent_ms: 0,
            subscriber_received_ms: 0,
            subscriber_dump_ms: 0,
        };

        let contract = GateContract {
            name: "BTC_USDT".to_string(),
            contract_type: "futures".to_string(),
            quanto_multiplier: "1.0".to_string(),
            leverage_min: "1".to_string(),
            leverage_max: "100".to_string(),
            mark_price: "100000.0".to_string(),
            funding_rate: "0.01".to_string(),
            order_size_min: 1,
            order_size_max: 100,
            order_price_round: "0.01".to_string(),
        };
        let snapshot = SymbolBooktickerSnapshot {
            gate_bt: gate_bt.clone(),
            gate_web_bt: gate_web_bt.clone(),
            base_bt: Some(binance_bt.clone()),
            gate_previous_bt: None,
            gate_bt_received_at_ms: 0,
            gate_web_bt_received_at_ms: 0,
            base_bt_received_at_ms: 0,
        };
        let result = calculate_signal(&snapshot, &contract);

        assert_eq!(result.order_side, None);
        assert_eq!(result.order_price, None);
        assert_eq!(result.mid_gap_chance_bp, 0.0);
        assert!(!result.is_binance_valid);
        assert_eq!(result.binance_mid_gap_chance_bp, None);
    }
}

//! Order decision — 레거시 `order_decision::decide_order` 기반.
//!
//! SignalResult + TradeSettings + 현재 시각/latency/펀딩/포지션 정보 → OrderDecision.
//! 레거시와 1:1 시맨틱 유지하되 타입을 타이트하게 바꿈:
//!   * `level: String` → [`OrderLevel`] enum
//!   * `order_tif: String` → 보관은 `&'static str` (레거시 TIF 문자열은 FFI 경계에서만 필요)
//!   * alloc 0. hot path 에서 String 해시/비교 없음.

use hft_strategy_config::TradeSettings;
use std::str::FromStr;

use crate::signal::SignalResult;

/// 주문 side. 문자열 "buy"/"sell" 을 wrap 해 hot path 에서는 비교를 enum 으로.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }
    #[inline]
    pub fn is_buy(self) -> bool {
        matches!(self, Self::Buy)
    }
}

impl FromStr for OrderSide {
    type Err = ();

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "buy" => Ok(Self::Buy),
            "sell" => Ok(Self::Sell),
            _ => Err(()),
        }
    }
}

/// 주문 레벨. v8 에서 market_close 가 추가됨.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderLevel {
    LimitOpen,
    LimitClose,
    MarketClose,
}

impl OrderLevel {
    #[inline]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LimitOpen => "limit_open",
            Self::LimitClose => "limit_close",
            Self::MarketClose => "market_close",
        }
    }
    #[inline]
    pub fn is_close(self) -> bool {
        matches!(self, Self::LimitClose | Self::MarketClose)
    }
}

/// 주문 결정. 레거시 `OrderDecision` 의 필드 그대로 유지하되 타입 강화.
#[derive(Debug, Clone)]
pub struct OrderDecision {
    pub level: OrderLevel,
    pub order_tif: &'static str, // "fok"/"ioc"/"gtc"/"poc"
    pub order_size: i64,
    pub only_close: bool,
}

/// TIF 문자열을 `&'static str` 로 normalize. 알 수 없으면 "fok" 로 fallback.
#[inline]
fn normalize_tif(s: &str) -> &'static str {
    match s {
        "fok" => "fok",
        "ioc" => "ioc",
        "gtc" => "gtc",
        "poc" => "poc",
        _ => "fok",
    }
}

/// 기본 `decide_order` (v0). 레거시와 동일 시맨틱.
///
/// Latency gate → funding_rate 로 only_close 결정 → limit_open / limit_close 조건 검사.
///
/// `binance_bt_server_time` 이 None 이면 v0 시맨틱에서는 **절대 주문하지 않음**.
/// v8 은 binance_data_is_updated 만 false 로 두고 open 조건의 일부로 사용한다.
#[allow(clippy::too_many_arguments)]
pub fn decide_order(
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
) -> Option<OrderDecision> {
    if !signal.has_signal() {
        return None;
    }
    let side_str = signal.order_side?;
    let side = side_str.parse::<OrderSide>().ok()?;
    let _price = signal.order_price?;

    // latency gate ― gate BT
    if current_time_ms - gate_bt_server_time > trade_settings.gate_last_book_ticker_latency_ms {
        return None;
    }
    // binance BT 는 v0 에서는 필수
    match binance_bt_server_time {
        Some(ts) => {
            if current_time_ms - ts > trade_settings.binance_last_book_ticker_latency_ms {
                return None;
            }
        }
        None => return None,
    }
    if current_time_ms - gate_web_bt_server_time
        > trade_settings.gate_last_webbook_ticker_latency_ms
    {
        return None;
    }

    // 펀딩 레이트 → only_close
    let only_close = match side {
        OrderSide::Buy => funding_rate > trade_settings.funding_rate_threshold,
        OrderSide::Sell => funding_rate < -trade_settings.funding_rate_threshold,
    };

    // close_based_on_binance_ok
    let close_based_on_binance_ok =
        matches!(signal.binance_mid_gap_chance_bp, Some(bp) if bp > 0.0);

    let orderbook_size_i = signal.orderbook_size as i64;

    // ─ LIMIT OPEN ─
    let mut level: Option<OrderLevel> = None;
    if signal.mid_gap_chance_bp > trade_settings.mid_gap_bp_threshold
        && signal.orderbook_size > 0.0
        && signal.spread_bp > trade_settings.spread_bp_threshold
        && orderbook_size_i > size_trigger
        && signal.is_binance_valid
        && orderbook_size_i <= max_size_trigger
    {
        level = Some(if only_close {
            OrderLevel::LimitClose
        } else {
            OrderLevel::LimitOpen
        });
    } else if signal.mid_gap_chance_bp > trade_settings.close_raw_mid_profit_bp
        && signal.orderbook_size > 0.0
        && signal.spread_bp > trade_settings.spread_bp_threshold
        && orderbook_size_i > close_size_trigger
        && order_count_size.abs() > trade_settings.close_order_count as f64
        && close_based_on_binance_ok
    {
        level = Some(OrderLevel::LimitClose);
    }

    let level = level?;

    // 방향 체크 (close 주문이 잘못된 방향이면 skip)
    if level == OrderLevel::LimitClose {
        if usdt_position_size > 0.0 && side == OrderSide::Buy {
            return None;
        }
        if usdt_position_size < 0.0 && side == OrderSide::Sell {
            return None;
        }
    }

    let tif = match level {
        OrderLevel::LimitOpen => normalize_tif(&trade_settings.limit_open_tif),
        OrderLevel::LimitClose => normalize_tif(&trade_settings.limit_close_tif),
        OrderLevel::MarketClose => "ioc",
    };

    Some(OrderDecision {
        level,
        order_tif: tif,
        order_size: contract_order_size,
        only_close,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// v8 variant — close_stale short-circuit + order-rate driven size_threshold.
// ─────────────────────────────────────────────────────────────────────────────

/// V8 variant 입력 중 **레거시 gate_hft_rust::strategies::v8 에서만 필요한** 추가 인자.
///
/// v0 `decide_order` 와 분리한 이유: v0 는 변동 가능성이 낮은 signal 기반 entry/exit 판단
/// 이고, v8 은 order-rate throttle + 포지션 stale close 가 entry gate 에 앞서 작동.
/// 함수 signature 가 비대해지는 것을 막기 위해 struct 로 묶는다.
#[derive(Debug, Clone, Copy)]
pub struct V8DecisionCtx {
    /// 현재 계정 전체 주문이 too_many (레거시 is_too_many_orders).
    pub is_too_many_orders: bool,
    /// 현재 심볼 주문이 too_many (레거시 is_symbol_too_many_orders).
    pub is_symbol_too_many_orders: bool,
    /// 1시간 내 is_too_many 가 한번이라도 true 였음 (레거시 is_recently_too_many_orders).
    pub is_recently_too_many_orders: bool,
    /// 심볼 현재 포지션 update_time (seconds since epoch). 0 = unknown.
    pub position_update_time_sec: i64,
    /// 심볼의 quanto_multiplier. close_stale_minutes_size 변환에 사용.
    pub quanto_multiplier: f64,
    /// 현재 시각 (ms). latency gate 에 쓰임.
    pub current_time_ms: i64,
    /// 현재 시각 (seconds). close_stale 판단에 쓰임.
    pub current_time_sec: i64,
}

/// V8 전용 decide_order. 레거시 `strategies::v8::decide_order` 를 그대로 반영.
///
/// # v0 대비 변경점
/// 1. **close_stale**: `position_update_time_sec > 0` 이고 `update_time < now - close_stale_minutes*60`
///    이면 **market_close** + ioc 즉시 리턴. signal/latency gate 보다 선행.
/// 2. **size_threshold_multiplier**: too_many_orders 신호에 따라 size_trigger/close_size_trigger
///    를 동적으로 1x / 2x / 4x 로 곱해 overload 시 진입 허들 상승.
/// 3. **converted_size_trigger**: `(size_trigger * mult).min(max_size_trigger/2)` 상한.
///
/// 나머지 open/close branch 는 v0 와 동일.
#[allow(clippy::too_many_arguments)]
pub fn decide_order_v8(
    signal: &SignalResult,
    trade_settings: &TradeSettings,
    ctx: &V8DecisionCtx,
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
) -> Option<OrderDecision> {
    // ── 1) close_stale short-circuit ──────────────────────────────────────
    // 포지션이 있고 (update_time > 0) 오래된 포지션이면 시장가 강제 청산.
    if ctx.position_update_time_sec > 0
        && ctx.position_update_time_sec
            < ctx.current_time_sec - trade_settings.close_stale_minutes * 60
    {
        let mut order_size = contract_order_size;
        if let Some(close_size_usdt) = trade_settings.close_stale_minutes_size {
            if close_size_usdt > 0.0 && ctx.quanto_multiplier > 0.0 {
                let sized =
                    (close_size_usdt * ctx.quanto_multiplier).max(contract_order_size as f64);
                order_size = sized as i64;
            }
        }
        return Some(OrderDecision {
            level: OrderLevel::MarketClose,
            order_tif: "ioc",
            order_size,
            only_close: true,
        });
    }

    if !signal.has_signal() {
        return None;
    }
    let side_str = signal.order_side?;
    let side = side_str.parse::<OrderSide>().ok()?;
    let _price = signal.order_price?;

    // ── 2) size_threshold_multiplier 결정 ─────────────────────────────────
    let mut size_threshold_multiplier: i64 = 1;
    if ctx.is_too_many_orders {
        size_threshold_multiplier = size_threshold_multiplier
            .saturating_mul(trade_settings.too_many_orders_size_threshold_multiplier);
    }
    if ctx.is_symbol_too_many_orders {
        size_threshold_multiplier = size_threshold_multiplier
            .saturating_mul(trade_settings.symbol_too_many_orders_size_threshold_multiplier);
    }
    // recent 까지 있으면 한번 더 상승 (레거시 time_sleep_multiplier *2 대응의 spatial 판).
    if ctx.is_recently_too_many_orders {
        size_threshold_multiplier = size_threshold_multiplier.saturating_mul(2);
    }

    // converted_size_trigger = (size_trigger * mult).min(max_size_trigger / 2)
    let converted_size_trigger = size_trigger
        .saturating_mul(size_threshold_multiplier)
        .min(max_size_trigger.saturating_div(2).max(0));
    let converted_close_size_trigger = close_size_trigger
        .saturating_mul(size_threshold_multiplier)
        .min(max_size_trigger.saturating_div(2).max(0));

    // ── 3) latency gate (v0 와 동일) ──────────────────────────────────────
    if ctx.current_time_ms - gate_bt_server_time > trade_settings.gate_last_book_ticker_latency_ms {
        return None;
    }
    let binance_data_is_updated = match binance_bt_server_time {
        Some(ts) => ctx.current_time_ms - ts <= trade_settings.binance_last_book_ticker_latency_ms,
        None => false,
    };
    // v8 은 binance 없으면 open 에서 reject 되도록만 하고 close 는 허용.
    if ctx.current_time_ms - gate_web_bt_server_time
        > trade_settings.gate_last_webbook_ticker_latency_ms
    {
        return None;
    }

    let only_close = match side {
        OrderSide::Buy => funding_rate > trade_settings.funding_rate_threshold,
        OrderSide::Sell => funding_rate < -trade_settings.funding_rate_threshold,
    };

    let close_based_on_binance_ok =
        matches!(signal.binance_mid_gap_chance_bp, Some(bp) if bp > 0.0);

    let orderbook_size_i = signal.orderbook_size as i64;

    let mut level: Option<OrderLevel> = None;
    // ── 4) LIMIT OPEN (v0 와 동일 조건 + v8 converted size_trigger + binance updated) ──
    if signal.mid_gap_chance_bp > trade_settings.mid_gap_bp_threshold
        && signal.orderbook_size > 0.0
        && signal.spread_bp > trade_settings.spread_bp_threshold
        && orderbook_size_i > converted_size_trigger
        && signal.is_binance_valid
        && binance_data_is_updated
        && orderbook_size_i <= max_size_trigger
    {
        level = Some(if only_close {
            OrderLevel::LimitClose
        } else {
            OrderLevel::LimitOpen
        });
    } else if signal.mid_gap_chance_bp > trade_settings.close_raw_mid_profit_bp
        && signal.orderbook_size > 0.0
        && signal.spread_bp > trade_settings.spread_bp_threshold
        && orderbook_size_i > converted_close_size_trigger
        && order_count_size.abs() > trade_settings.close_order_count as f64
        && close_based_on_binance_ok
    {
        level = Some(OrderLevel::LimitClose);
    }

    let level = level?;

    // close 방향 체크
    if level == OrderLevel::LimitClose {
        if usdt_position_size > 0.0 && side == OrderSide::Buy {
            return None;
        }
        if usdt_position_size < 0.0 && side == OrderSide::Sell {
            return None;
        }
    }

    let tif = match level {
        OrderLevel::LimitOpen => normalize_tif(&trade_settings.limit_open_tif),
        OrderLevel::LimitClose => normalize_tif(&trade_settings.limit_close_tif),
        OrderLevel::MarketClose => "ioc",
    };

    Some(OrderDecision {
        level,
        order_tif: tif,
        order_size: contract_order_size,
        only_close,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// v6 variant — close_stale + size_threshold_multiplier (rate-tracker 미사용),
//              is_order_net_position_close_side 시 contract_order_size *2.
// ─────────────────────────────────────────────────────────────────────────────

/// V6 variant context.
///
/// v8 과의 차이:
/// - `is_recently_too_many_orders` 가 size_threshold_multiplier 에 영향 주지 않음
///   (v6 레거시는 이 인자를 close 분기에서만 썼고 entry size 곱셈에는 v6 가 미사용).
/// - `account_position_usdt_size` (계정 전체 net 포지션) 를 별도 인자로 받음 — open 분기에서
///   `is_order_net_position_close_side` 판정에 쓰임. v8 도 같은 로직이지만 v8 ctx 는
///   exposure 에서 끌어올 수 있어 단순화돼 있다. v6 는 명시.
#[derive(Debug, Clone, Copy)]
pub struct V6DecisionCtx {
    pub is_too_many_orders: bool,
    pub is_symbol_too_many_orders: bool,
    pub position_update_time_sec: i64,
    pub quanto_multiplier: f64,
    /// 계정 전체 net usdt 포지션 (long 양수 / short 음수). 레거시 `get_net_positions_usdt_size`.
    pub account_net_position_usdt: f64,
    pub current_time_ms: i64,
    pub current_time_sec: i64,
}

/// V6 decide_order. 레거시 `strategies::v6::decide_order` 1:1.
///
/// # v8 대비 차이점
/// 1. `is_recently_too_many_orders` 인자 없음 — size threshold multiplier 가 1x/2x/4x 까지만.
/// 2. `is_order_net_position_close_side=true` 이고 LIMIT_OPEN 이면 contract_order_size *2.
/// 3. signal 없이 close_stale short-circuit 만 발동.
#[allow(clippy::too_many_arguments)]
pub fn decide_order_v6(
    signal: &SignalResult,
    trade_settings: &TradeSettings,
    ctx: &V6DecisionCtx,
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
) -> Option<OrderDecision> {
    // ── 1) close_stale short-circuit (signal 무관) ────────────────────────
    if ctx.position_update_time_sec > 0
        && ctx.position_update_time_sec
            < ctx.current_time_sec - trade_settings.close_stale_minutes * 60
    {
        let mut order_size = contract_order_size;
        if let Some(close_size_usdt) = trade_settings.close_stale_minutes_size {
            if close_size_usdt > 0.0 && ctx.quanto_multiplier > 0.0 {
                let sized =
                    (close_size_usdt * ctx.quanto_multiplier).max(contract_order_size as f64);
                order_size = sized as i64;
            }
        }
        return Some(OrderDecision {
            level: OrderLevel::MarketClose,
            order_tif: "ioc",
            order_size,
            only_close: true,
        });
    }

    if !signal.has_signal() {
        return None;
    }
    let side_str = signal.order_side?;
    let side = side_str.parse::<OrderSide>().ok()?;
    let order_price = signal.order_price?;

    // ── 2) size_threshold_multiplier (1x → 2x → 4x) ───────────────────────
    let mut size_threshold_multiplier: i64 = 1;
    if ctx.is_too_many_orders {
        size_threshold_multiplier = size_threshold_multiplier
            .saturating_mul(trade_settings.too_many_orders_size_threshold_multiplier);
    }
    if ctx.is_symbol_too_many_orders {
        size_threshold_multiplier = size_threshold_multiplier
            .saturating_mul(trade_settings.symbol_too_many_orders_size_threshold_multiplier);
    }
    let half_max = max_size_trigger.saturating_div(2).max(0);
    let converted_size_trigger = size_trigger
        .saturating_mul(size_threshold_multiplier)
        .min(half_max);
    let converted_close_size_trigger = close_size_trigger
        .saturating_mul(size_threshold_multiplier)
        .min(half_max);

    // ── 3) latency gate ──────────────────────────────────────────────────
    if ctx.current_time_ms - gate_bt_server_time > trade_settings.gate_last_book_ticker_latency_ms {
        return None;
    }
    let binance_data_is_updated = match binance_bt_server_time {
        Some(ts) => ctx.current_time_ms - ts <= trade_settings.binance_last_book_ticker_latency_ms,
        None => false,
    };
    if ctx.current_time_ms - gate_web_bt_server_time
        > trade_settings.gate_last_webbook_ticker_latency_ms
    {
        return None;
    }

    // ── 4) only_close: funding_rate 가 임계 초과면 entry 차단 ─────────────
    let only_close_funding = funding_rate > trade_settings.funding_rate_threshold
        || funding_rate < -trade_settings.funding_rate_threshold;
    let only_close = only_close_funding;

    // ── 5) is_order_net_position_close_side ──────────────────────────────
    // 계정 net 이 long 인데 sell 주문 / 심볼 포지션이 short 인데 buy 주문 → close-side.
    let is_order_net_position_close_side = (ctx.account_net_position_usdt > 0.0
        && side == OrderSide::Sell)
        || (usdt_position_size < 0.0 && side == OrderSide::Buy);

    let close_based_on_binance_ok =
        matches!(signal.binance_mid_gap_chance_bp, Some(bp) if bp > 0.0);
    let orderbook_size_i = signal.orderbook_size as i64;

    // ── 6) LIMIT OPEN 분기 ───────────────────────────────────────────────
    let mut level: Option<OrderLevel> = None;
    let mut mutated_contract_order_size = contract_order_size;
    if signal.mid_gap_chance_bp > trade_settings.mid_gap_bp_threshold
        && signal.orderbook_size > 0.0
        && signal.spread_bp > trade_settings.spread_bp_threshold
        && orderbook_size_i > converted_size_trigger
        && signal.is_binance_valid
        && binance_data_is_updated
        && orderbook_size_i <= max_size_trigger
    {
        level = Some(if only_close {
            OrderLevel::LimitClose
        } else {
            OrderLevel::LimitOpen
        });
        if is_order_net_position_close_side {
            mutated_contract_order_size = contract_order_size.saturating_mul(2);
        }
    } else {
        // ── 7) LIMIT CLOSE 분기 ──────────────────────────────────────────
        let has_long_position = usdt_position_size > 0.0;
        let gate_mid = signal.gate_mid;

        let should_exit_profit = signal.orderbook_size > 0.0
            && signal.spread_bp > trade_settings.spread_bp_threshold
            && orderbook_size_i > converted_close_size_trigger
            && signal.mid_gap_chance_bp > trade_settings.close_raw_mid_profit_bp
            && orderbook_size_i <= max_size_trigger;

        let close_order_count_exceeded =
            order_count_size.abs() > trade_settings.close_order_count as f64;

        if should_exit_profit
            && close_based_on_binance_ok
            && close_order_count_exceeded
            && binance_data_is_updated
        {
            // profit exit — limit_close (방향 일치).
            if (has_long_position && side == OrderSide::Sell)
                || (!has_long_position && side == OrderSide::Buy)
            {
                level = Some(OrderLevel::LimitClose);
            }
        } else if should_exit_profit {
            // narrative exit — best price 와의 gap 비교로 close 결정.
            let order_price_round = signal
                .gate_contract
                .order_price_round
                .parse::<f64>()
                .unwrap_or(0.0);
            match side {
                OrderSide::Buy => {
                    let avail = signal.gate_bid + order_price_round;
                    let mid_gap = (order_price - gate_mid).abs();
                    let best_gap = (signal.gate_bid - order_price).abs();
                    if order_price < gate_mid
                        && (avail >= order_price
                            || (best_gap < mid_gap && is_order_net_position_close_side))
                    {
                        level = Some(OrderLevel::LimitClose);
                    }
                }
                OrderSide::Sell => {
                    let avail = signal.gate_ask - order_price_round;
                    let mid_gap = (order_price - gate_mid).abs();
                    let best_gap = (signal.gate_ask - order_price).abs();
                    if order_price > gate_mid
                        && (avail <= order_price
                            || (best_gap < mid_gap && is_order_net_position_close_side))
                    {
                        level = Some(OrderLevel::LimitClose);
                    }
                }
            }
        }
    }

    let level = level?;

    // ── 8) close 방향 사후 체크 ──────────────────────────────────────────
    if level == OrderLevel::LimitClose {
        if usdt_position_size > 0.0 && side == OrderSide::Buy {
            return None;
        }
        if usdt_position_size < 0.0 && side == OrderSide::Sell {
            return None;
        }
    }

    let tif = match level {
        OrderLevel::LimitOpen => normalize_tif(&trade_settings.limit_open_tif),
        OrderLevel::LimitClose => normalize_tif(&trade_settings.limit_close_tif),
        OrderLevel::MarketClose => "ioc",
    };

    Some(OrderDecision {
        level,
        order_tif: tif,
        order_size: mutated_contract_order_size,
        only_close,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// v7 variant — v0 + is_order_net_position_close_side 시 size *2.
// close_stale / size_threshold_multiplier 모두 없음.
// open 조건 1차 패스 후 narrative-close 분기에서 only_close 에 따라 limit_open/close 양분기.
// ─────────────────────────────────────────────────────────────────────────────

/// V7 전용 부가 입력 (v8 의 V8DecisionCtx 처럼 ctx 묶음을 별도 struct 로).
#[derive(Debug, Clone, Copy, Default)]
pub struct V7DecisionExtras {
    /// 계정 net 포지션 (USDT). is_order_net_position_close_side 판정에 사용.
    pub account_net_position_usdt: f64,
}

/// V7 decide_order. 레거시 `strategies::v7::decide_order` 1:1.
///
/// # v0 와의 차이
/// 1. `is_order_net_position_close_side=true` 이고 LIMIT_OPEN 이면 contract_order_size *2.
/// 2. narrative-close 분기 (should_exit_profit 인데 limit_close 직접 매핑이 안 될 때) 에서
///    only_close 에 따라 limit_close ↔ limit_open 양분기.
/// 3. close_stale 없음. size_threshold_multiplier 없음.
#[allow(clippy::too_many_arguments)]
pub fn decide_order_v7(
    signal: &SignalResult,
    trade_settings: &TradeSettings,
    extras: &V7DecisionExtras,
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
) -> Option<OrderDecision> {
    if !signal.has_signal() {
        return None;
    }
    let side_str = signal.order_side?;
    let side = side_str.parse::<OrderSide>().ok()?;
    let order_price = signal.order_price?;

    // ── 1) latency gate ──────────────────────────────────────────────────
    if current_time_ms - gate_bt_server_time > trade_settings.gate_last_book_ticker_latency_ms {
        return None;
    }
    let binance_data_is_updated = match binance_bt_server_time {
        Some(ts) => current_time_ms - ts <= trade_settings.binance_last_book_ticker_latency_ms,
        None => false,
    };
    if current_time_ms - gate_web_bt_server_time
        > trade_settings.gate_last_webbook_ticker_latency_ms
    {
        return None;
    }

    let only_close_funding = funding_rate > trade_settings.funding_rate_threshold
        || funding_rate < -trade_settings.funding_rate_threshold;
    let only_close = only_close_funding;

    let is_order_net_position_close_side = (extras.account_net_position_usdt > 0.0
        && side == OrderSide::Sell)
        || (usdt_position_size < 0.0 && side == OrderSide::Buy);

    let close_based_on_binance_ok =
        matches!(signal.binance_mid_gap_chance_bp, Some(bp) if bp > 0.0);
    let orderbook_size_i = signal.orderbook_size as i64;

    let mut level: Option<OrderLevel> = None;
    let mut mutated_contract_order_size = contract_order_size;

    // ── 2) LIMIT OPEN 1차 ────────────────────────────────────────────────
    if signal.mid_gap_chance_bp > trade_settings.mid_gap_bp_threshold
        && signal.orderbook_size > 0.0
        && signal.spread_bp > trade_settings.spread_bp_threshold
        && orderbook_size_i > size_trigger
        && signal.is_binance_valid
        && binance_data_is_updated
        && orderbook_size_i <= max_size_trigger
    {
        level = Some(if only_close {
            OrderLevel::LimitClose
        } else {
            OrderLevel::LimitOpen
        });
        if is_order_net_position_close_side {
            mutated_contract_order_size = contract_order_size.saturating_mul(2);
        }
    } else {
        // ── 3) Narrative close ───────────────────────────────────────────
        let has_long_position = usdt_position_size > 0.0;
        let gate_mid = signal.gate_mid;

        // 주의: v7 은 should_exit_profit 에 max_size_trigger 상한이 없다 (v6 와 차이).
        let should_exit_profit = signal.orderbook_size > 0.0
            && signal.spread_bp > trade_settings.spread_bp_threshold
            && orderbook_size_i > close_size_trigger
            && signal.mid_gap_chance_bp > trade_settings.close_raw_mid_profit_bp;

        let close_order_count_exceeded =
            order_count_size.abs() > trade_settings.close_order_count as f64;

        if should_exit_profit
            && close_based_on_binance_ok
            && close_order_count_exceeded
            && binance_data_is_updated
        {
            if (has_long_position && side == OrderSide::Sell)
                || (!has_long_position && side == OrderSide::Buy)
            {
                level = Some(OrderLevel::LimitClose);
            }
        } else if should_exit_profit {
            let order_price_round = signal
                .gate_contract
                .order_price_round
                .parse::<f64>()
                .unwrap_or(0.0);
            // 핵심 차이: v7 은 narrative-close 트리거 시 only_close 분기로 limit_close/limit_open
            // 둘 중 하나로 set 한다.
            let on_match = if only_close {
                OrderLevel::LimitClose
            } else {
                OrderLevel::LimitOpen
            };
            match side {
                OrderSide::Buy => {
                    let avail = signal.gate_bid + order_price_round;
                    let mid_gap = (order_price - gate_mid).abs();
                    let best_gap = (signal.gate_bid - order_price).abs();
                    if order_price < gate_mid
                        && (avail >= order_price
                            || (best_gap < mid_gap && is_order_net_position_close_side))
                    {
                        level = Some(on_match);
                    }
                }
                OrderSide::Sell => {
                    let avail = signal.gate_ask - order_price_round;
                    let mid_gap = (order_price - gate_mid).abs();
                    let best_gap = (signal.gate_ask - order_price).abs();
                    if order_price > gate_mid
                        && (avail <= order_price
                            || (best_gap < mid_gap && is_order_net_position_close_side))
                    {
                        level = Some(on_match);
                    }
                }
            }
        }
    }

    let level = level?;

    // close 방향 사후 체크 (v0 와 동일)
    if level == OrderLevel::LimitClose {
        if usdt_position_size > 0.0 && side == OrderSide::Buy {
            return None;
        }
        if usdt_position_size < 0.0 && side == OrderSide::Sell {
            return None;
        }
    }

    let tif = match level {
        OrderLevel::LimitOpen => normalize_tif(&trade_settings.limit_open_tif),
        OrderLevel::LimitClose => normalize_tif(&trade_settings.limit_close_tif),
        OrderLevel::MarketClose => "ioc",
    };

    Some(OrderDecision {
        level,
        order_tif: tif,
        order_size: mutated_contract_order_size,
        only_close,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{GateContract, SignalResult};

    fn base_signal(side: &'static str, mid_gap_bp: f64) -> SignalResult {
        SignalResult {
            order_side: Some(side),
            order_price: Some(100.0),
            mid_gap_chance_bp: mid_gap_bp,
            binance_mid_gap_chance_bp: Some(2.0),
            orderbook_size: 500.0,
            spread_bp: 3.0,
            is_binance_valid: true,
            gate_mid: 100.0,
            gate_bid: 99.5,
            gate_ask: 100.5,
            binance_mid: 100.0,
            gate_web_mid: 100.0,
            gate_contract: GateContract::default(),
        }
    }

    fn ts() -> TradeSettings {
        TradeSettings {
            mid_gap_bp_threshold: 1.0,
            spread_bp_threshold: 1.0,
            close_raw_mid_profit_bp: 0.1,
            ..TradeSettings::default()
        }
    }

    #[test]
    fn no_signal_returns_none() {
        let mut sig = base_signal("buy", 5.0);
        sig.order_side = None;
        let out = decide_order(
            &sig,
            &ts(),
            1000,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            1,
            0.0,
        );
        assert!(out.is_none());
    }

    #[test]
    fn latency_gate_rejects() {
        let sig = base_signal("buy", 5.0);
        let out = decide_order(
            &sig,
            &ts(),
            100_000,        // current
            100_000 - 9999, // gate bt 10s 전 → default latency 500ms 초과
            Some(100_000),
            100_000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            1,
            0.0,
        );
        assert!(out.is_none());
    }

    #[test]
    fn binance_missing_rejects_v0() {
        let sig = base_signal("buy", 5.0);
        let out = decide_order(
            &sig,
            &ts(),
            1000,
            1000,
            None,
            1000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            1,
            0.0,
        );
        assert!(out.is_none());
    }

    #[test]
    fn triggers_limit_open_when_all_gates_pass() {
        let sig = base_signal("buy", 5.0);
        let d = decide_order(
            &sig,
            &ts(),
            1000,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            3,
            0.0,
        )
        .unwrap();
        assert_eq!(d.level, OrderLevel::LimitOpen);
        assert_eq!(d.order_size, 3);
        assert_eq!(d.order_tif, "fok");
        assert!(!d.only_close);
    }

    #[test]
    fn funding_rate_forces_only_close_for_buy() {
        let sig = base_signal("buy", 5.0);
        let mut t = ts();
        t.funding_rate_threshold = 0.001;
        let d = decide_order(
            &sig,
            &t,
            1000,
            1000,
            Some(1000),
            1000,
            0.002,
            0.0,
            100,
            10_000,
            200,
            3,
            0.0,
        )
        .unwrap();
        assert!(d.only_close);
        assert_eq!(d.level, OrderLevel::LimitClose);
    }

    #[test]
    fn close_wrong_direction_rejected() {
        // 롱 포지션이 있는데 close-buy → None.
        let mut sig = base_signal("buy", 5.0);
        sig.is_binance_valid = false; // open 조건 실패 → close branch 탐.
        let mut t = ts();
        // open 은 실패, close 조건 성공하도록
        t.close_raw_mid_profit_bp = 0.1;
        t.close_order_count = 0;
        let d = decide_order(
            &sig,
            &t,
            1000,
            1000,
            Some(1000),
            1000,
            0.0,
            10.0, // order_count_size — abs > close_order_count(0)
            100,
            10_000,
            200,
            3,
            100.0, // 롱 포지션인데 side=buy → 잘못된 방향
        );
        assert!(d.is_none());
    }

    #[test]
    fn order_side_enum_roundtrip() {
        assert_eq!("buy".parse::<OrderSide>(), Ok(OrderSide::Buy));
        assert_eq!("sell".parse::<OrderSide>(), Ok(OrderSide::Sell));
        assert!("BOOM".parse::<OrderSide>().is_err());
        assert_eq!(OrderSide::Buy.as_str(), "buy");
        assert!(OrderSide::Buy.is_buy());
    }

    #[test]
    fn order_level_is_close() {
        assert!(OrderLevel::LimitClose.is_close());
        assert!(OrderLevel::MarketClose.is_close());
        assert!(!OrderLevel::LimitOpen.is_close());
    }

    // ── v8 tests ─────────────────────────────────────────────────────────

    fn v8_ctx() -> V8DecisionCtx {
        V8DecisionCtx {
            is_too_many_orders: false,
            is_symbol_too_many_orders: false,
            is_recently_too_many_orders: false,
            position_update_time_sec: 0,
            quanto_multiplier: 1.0,
            current_time_ms: 1_000,
            current_time_sec: 1,
        }
    }

    #[test]
    fn v8_close_stale_forces_market_close() {
        let sig = base_signal("buy", 5.0);
        let mut t = ts();
        t.close_stale_minutes = 1; // 1분
        let mut ctx = v8_ctx();
        ctx.current_time_sec = 1000;
        ctx.position_update_time_sec = 1000 - 120; // 2분 전 — stale
        let d = decide_order_v8(
            &sig,
            &t,
            &ctx,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            3,
            100.0,
        )
        .unwrap();
        assert_eq!(d.level, OrderLevel::MarketClose);
        assert_eq!(d.order_tif, "ioc");
        assert!(d.only_close);
    }

    #[test]
    fn v8_close_stale_respects_minutes_size_override() {
        let sig = base_signal("buy", 5.0);
        let mut t = ts();
        t.close_stale_minutes = 1;
        t.close_stale_minutes_size = Some(500.0);
        let mut ctx = v8_ctx();
        ctx.current_time_sec = 1000;
        ctx.position_update_time_sec = 1000 - 120;
        ctx.quanto_multiplier = 2.0;
        let d = decide_order_v8(
            &sig,
            &t,
            &ctx,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            3,
            100.0,
        )
        .unwrap();
        // 500.0 * 2.0 = 1000 > contract_order_size(3) → 1000
        assert_eq!(d.order_size, 1000);
    }

    #[test]
    fn v8_no_stale_falls_through_to_signal_gate() {
        let sig = base_signal("buy", 5.0);
        let t = ts();
        let ctx = v8_ctx();
        let d = decide_order_v8(
            &sig,
            &t,
            &ctx,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            3,
            0.0,
        )
        .unwrap();
        assert_eq!(d.level, OrderLevel::LimitOpen);
    }

    #[test]
    fn v8_too_many_orders_doubles_size_threshold() {
        // orderbook_size=500, size_trigger=300 → base 조건은 500>300 OK.
        // too_many_orders 이고 multiplier=2 면 size_trigger 가 600 으로 상승해 reject.
        let mut sig = base_signal("buy", 5.0);
        sig.orderbook_size = 500.0;
        let mut t = ts();
        t.too_many_orders_size_threshold_multiplier = 2;
        let mut ctx = v8_ctx();
        ctx.is_too_many_orders = true;
        let d = decide_order_v8(
            &sig,
            &t,
            &ctx,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            300,    // size_trigger
            10_000, // max_size_trigger
            200,
            3,
            0.0,
        );
        // 300 * 2 = 600 → 500 < 600 → None (open reject)
        assert!(d.is_none());
    }

    #[test]
    fn v8_latency_gate_same_as_v0() {
        let sig = base_signal("buy", 5.0);
        let t = ts();
        let mut ctx = v8_ctx();
        ctx.current_time_ms = 100_000;
        let d = decide_order_v8(
            &sig,
            &t,
            &ctx,
            100_000 - 9999,
            Some(100_000),
            100_000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            3,
            0.0,
        );
        assert!(d.is_none());
    }

    // ── v6 tests ─────────────────────────────────────────────────────────

    fn v6_ctx() -> V6DecisionCtx {
        V6DecisionCtx {
            is_too_many_orders: false,
            is_symbol_too_many_orders: false,
            position_update_time_sec: 0,
            quanto_multiplier: 1.0,
            account_net_position_usdt: 0.0,
            current_time_ms: 1_000,
            current_time_sec: 1,
        }
    }

    #[test]
    fn v6_close_stale_emits_market_close() {
        let sig = base_signal("buy", 5.0);
        let mut t = ts();
        t.close_stale_minutes = 1;
        let mut ctx = v6_ctx();
        ctx.current_time_sec = 1000;
        ctx.position_update_time_sec = 1000 - 120; // 2분 전
        let d = decide_order_v6(
            &sig,
            &t,
            &ctx,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            3,
            100.0,
        )
        .unwrap();
        assert_eq!(d.level, OrderLevel::MarketClose);
        assert_eq!(d.order_tif, "ioc");
        assert!(d.only_close);
    }

    #[test]
    fn v6_open_doubles_size_when_net_position_close_side() {
        // 계정 long net인데 sell 시그널 → close-side → size *2.
        let sig = base_signal("sell", 5.0);
        let t = ts();
        let mut ctx = v6_ctx();
        ctx.account_net_position_usdt = 1000.0; // 계정 long
                                                // sell 시그널: orderbook_size=500 → size_trigger=100 통과해야 함.
        let d = decide_order_v6(
            &sig,
            &t,
            &ctx,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            3,   // contract_order_size
            0.0, // 심볼 자체는 포지션 0
        )
        .unwrap();
        assert_eq!(d.level, OrderLevel::LimitOpen);
        assert_eq!(d.order_size, 6); // 3 * 2 = 6
    }

    #[test]
    fn v6_too_many_orders_doubles_size_threshold() {
        let mut sig = base_signal("buy", 5.0);
        sig.orderbook_size = 500.0;
        let mut t = ts();
        t.too_many_orders_size_threshold_multiplier = 2;
        let mut ctx = v6_ctx();
        ctx.is_too_many_orders = true;
        let d = decide_order_v6(
            &sig,
            &t,
            &ctx,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            300, // size_trigger → *2 = 600 > 500 → reject
            10_000,
            200,
            3,
            0.0,
        );
        assert!(d.is_none());
    }

    #[test]
    fn v6_no_close_stale_falls_through_to_open() {
        let sig = base_signal("buy", 5.0);
        let t = ts();
        let ctx = v6_ctx();
        let d = decide_order_v6(
            &sig,
            &t,
            &ctx,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            3,
            0.0,
        )
        .unwrap();
        assert_eq!(d.level, OrderLevel::LimitOpen);
        assert_eq!(d.order_size, 3);
    }

    // ── v7 tests ─────────────────────────────────────────────────────────

    #[test]
    fn v7_open_doubles_size_when_net_position_close_side() {
        let sig = base_signal("sell", 5.0);
        let t = ts();
        let extras = V7DecisionExtras {
            account_net_position_usdt: 1000.0, // 계정 long
        };
        let d = decide_order_v7(
            &sig,
            &t,
            &extras,
            1000,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            3,
            0.0,
        )
        .unwrap();
        assert_eq!(d.level, OrderLevel::LimitOpen);
        assert_eq!(d.order_size, 6);
    }

    #[test]
    fn v7_no_close_stale_even_with_old_position() {
        // v7 은 close_stale 자체가 없으니 stale position 이어도 limit_open 분기로 가야 함.
        let sig = base_signal("buy", 5.0);
        let t = ts();
        let extras = V7DecisionExtras::default();
        // V7 은 V6/V8 와 달리 position update time 신호 무시.
        let d = decide_order_v7(
            &sig,
            &t,
            &extras,
            1000,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            3,
            0.0,
        )
        .unwrap();
        assert_eq!(d.level, OrderLevel::LimitOpen);
        // close_stale 발동 안함을 확인 (level 이 MarketClose 가 아님).
        assert_ne!(d.level, OrderLevel::MarketClose);
    }

    #[test]
    fn v7_latency_gate_rejects() {
        let sig = base_signal("buy", 5.0);
        let t = ts();
        let extras = V7DecisionExtras::default();
        let d = decide_order_v7(
            &sig,
            &t,
            &extras,
            100_000,
            100_000 - 9999,
            Some(100_000),
            100_000,
            0.0,
            0.0,
            100,
            10_000,
            200,
            3,
            0.0,
        );
        assert!(d.is_none());
    }

    #[test]
    fn v7_no_size_threshold_multiplier() {
        // v7 은 size_threshold_multiplier 가 없으니 too_many_orders 가 entry 를 막지 않음.
        // (v6 와의 차이 검증).
        let mut sig = base_signal("buy", 5.0);
        sig.orderbook_size = 500.0;
        let t = ts();
        let extras = V7DecisionExtras::default();
        let d = decide_order_v7(
            &sig,
            &t,
            &extras,
            1000,
            1000,
            Some(1000),
            1000,
            0.0,
            0.0,
            300, // size_trigger — 500 > 300 → 통과 (multiplier 없음)
            10_000,
            200,
            3,
            0.0,
        )
        .unwrap();
        assert_eq!(d.level, OrderLevel::LimitOpen);
    }
}

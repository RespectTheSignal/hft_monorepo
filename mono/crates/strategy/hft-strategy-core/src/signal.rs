//! Signal calculation — 레거시 `signal_calculator.rs` 의 `calculate_signal` /
//! `calculate_signal_v8` 1:1 포트. **순수 함수, alloc 0**.
//!
//! 핵심 로직:
//!   - gate_mid = (gate_ask + gate_bid) * 0.5
//!   - buy_price  = gate_web_ask (web book 의 ask 를 가져와 gate 현물에 매수 걸기)
//!   - sell_price = gate_web_bid
//!   - buy 시그널: buy_price < gate_mid  → 우리 쪽이 싸게 살 찬스
//!     mid_gap_chance_bp = (gate_mid - buy_price) / gate_mid * 10000
//!     orderbook_size    = gate_web_ask_size
//!   - sell 시그널: 대칭
//!   - is_binance_valid: buy 는 price < binance_bid, sell 은 price > binance_ask
//!   - v8 추가: gate_last_trade 가 gate_web_bt.event_time 보다 새 것이면 가격 대체.

/// 시그널 계산에 필요한 **최소한의 BookTicker 필드** 묶음.
///
/// 레거시 `BookTickerC` (FFI struct) 는 exchange/symbol 을 고정 길이 바이트배열로
/// 들고 다녀 hot path alignment 가 깨졌다. 계산에는 가격·사이즈·시각만 있으면 되고
/// 심볼/거래소는 호출부에서 Symbol enum 으로 이미 알고 있으므로 잘라냈다.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BookTickerSnap {
    pub bid_price: f64,
    pub ask_price: f64,
    pub bid_size: f64,
    pub ask_size: f64,
    /// 거래소가 report 한 event 시각 (ms)
    pub event_time_ms: i64,
    /// 우리가 수신한 서버 시각 (ms)
    pub server_time_ms: i64,
}

/// 시그널 계산에 필요한 최소한의 Trade 필드 묶음.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TradeSnap {
    pub price: f64,
    /// 양수=buy 체결, 음수=sell 체결 (레거시와 동일)
    pub size: f64,
    pub create_time_ms: i64,
}

/// Gate contract meta — 레거시 `GateContract` 에서 시그널에 필요한 것만 뽑음.
#[derive(Debug, Clone, Default)]
pub struct GateContract {
    /// 가격 소숫점 round (레거시는 문자열로 저장됨 — 호환)
    pub order_price_round: String,
    /// contract_size_round (문자열 호환)
    pub order_size_round: String,
    /// quanto multiplier (문자열 호환)
    pub quanto_multiplier: String,
}

/// 시그널 1회 결과. `has_signal()` 이 true 여야 호출부가 decide_order 로 넘김.
#[derive(Debug, Clone, Default)]
pub struct SignalResult {
    pub order_side: Option<&'static str>, // "buy" | "sell"
    pub order_price: Option<f64>,
    pub mid_gap_chance_bp: f64,
    pub binance_mid_gap_chance_bp: Option<f64>,
    pub orderbook_size: f64,
    pub spread_bp: f64,
    pub is_binance_valid: bool,
    pub gate_mid: f64,
    pub gate_bid: f64,
    pub gate_ask: f64,
    pub binance_mid: f64,
    pub gate_web_mid: f64,
    pub gate_contract: GateContract,
}

impl SignalResult {
    /// order side + price 가 모두 있을 때만 유효 시그널.
    #[inline]
    pub fn has_signal(&self) -> bool {
        self.order_side.is_some() && self.order_price.is_some()
    }
}

/// 분모가 0 에 수렴할 때 NaN 방지용. 레거시 `EPS=1e-12` 유지.
const EPS: f64 = 1e-12;

/// 기본 시그널 계산 (v0). 레거시 `calculate_signal` 과 동일 로직, 순수 함수.
#[inline]
pub fn calculate_signal(
    gate_bt: &BookTickerSnap,
    gate_web_bt: &BookTickerSnap,
    binance_bt: Option<&BookTickerSnap>,
    contract: &GateContract,
) -> SignalResult {
    let mut res = SignalResult {
        gate_bid: gate_bt.bid_price,
        gate_ask: gate_bt.ask_price,
        gate_contract: contract.clone(),
        ..Default::default()
    };

    let gate_mid = (gate_bt.ask_price + gate_bt.bid_price) * 0.5;
    res.gate_mid = gate_mid;
    if gate_mid.abs() < EPS {
        return res;
    }
    res.gate_web_mid = (gate_web_bt.ask_price + gate_web_bt.bid_price) * 0.5;

    // spread_bp (gate 내부 스프레드 — 유동성 지표)
    res.spread_bp = (gate_bt.ask_price - gate_bt.bid_price) / gate_mid * 10000.0;

    // Binance mid gap (reference 채널 gap — validity 검사용)
    if let Some(bin) = binance_bt {
        let bin_mid = (bin.ask_price + bin.bid_price) * 0.5;
        res.binance_mid = bin_mid;
        if bin_mid.abs() > EPS {
            let gap_bp = (gate_mid - bin_mid).abs() / bin_mid * 10000.0;
            res.binance_mid_gap_chance_bp = Some(gap_bp);
        }
    }

    // buy 시그널 — gate_web_ask 가 gate_mid 보다 낮으면 싸게 매수 가능.
    if gate_web_bt.ask_price < gate_mid {
        let buy_price = gate_web_bt.ask_price;
        res.order_side = Some("buy");
        res.order_price = Some(buy_price);
        res.mid_gap_chance_bp = (gate_mid - buy_price) / gate_mid * 10000.0;
        res.orderbook_size = gate_web_bt.ask_size;
        res.is_binance_valid = match binance_bt {
            Some(bin) => buy_price < bin.bid_price,
            None => false,
        };
    }
    // sell 시그널 대칭
    else if gate_web_bt.bid_price > gate_mid {
        let sell_price = gate_web_bt.bid_price;
        res.order_side = Some("sell");
        res.order_price = Some(sell_price);
        res.mid_gap_chance_bp = (sell_price - gate_mid) / gate_mid * 10000.0;
        res.orderbook_size = gate_web_bt.bid_size;
        res.is_binance_valid = match binance_bt {
            Some(bin) => sell_price > bin.ask_price,
            None => false,
        };
    }

    res
}

/// v6 시그널. v0 결과를 먼저 시도하고 시그널이 없으면 **gate-mid vs web-mid 비교**,
/// 그래도 없으면 **gate-mid vs binance-mid 비교** 로 side/price 를 강제로 채워준다.
///
/// 레거시 `strategies::v6::calculate_signal` 의 fallback 분기를 1:1 반영.
///
/// fallback 가격 정책:
///   - "buy" 시그널 → `gate_web_bt.bid_price` (web bid 에 매수 걸기 — 제일 안전한 가격)
///   - "sell" 시그널 → `gate_web_bt.ask_price`
///
/// 주의: fallback 으로 채워진 시그널은 `is_binance_valid=false` 그대로이고
/// `mid_gap_chance_bp` / `orderbook_size` / `spread_bp` 도 0 인 채. decide_order 가
/// 이런 경우 entry 에서 떨어뜨림. 즉 fallback 은 v6 의 close 분기를 깨우는 용도다.
#[inline]
pub fn calculate_signal_v6(
    gate_bt: &BookTickerSnap,
    gate_web_bt: &BookTickerSnap,
    binance_bt: Option<&BookTickerSnap>,
    contract: &GateContract,
) -> SignalResult {
    let mut sig = calculate_signal(gate_bt, gate_web_bt, binance_bt, contract);
    if sig.has_signal() {
        return sig;
    }

    // ── fallback: side 를 mid 비교로 결정 ──────────────────────────────────
    let gate_mid = (gate_bt.ask_price + gate_bt.bid_price) * 0.5;
    let web_mid = (gate_web_bt.ask_price + gate_web_bt.bid_price) * 0.5;

    let side = if gate_mid < web_mid {
        Some("sell")
    } else if gate_mid > web_mid {
        Some("buy")
    } else if let Some(bin) = binance_bt {
        // 레거시는 여기서 "(gate_ask + gate_bid) < (binance_ask + binance_bid) * 0.5" 의
        // 식을 썼는데 좌변이 mid 가 아닌 sum 이라 거의 항상 buy 로 가는 버그성 비교다.
        // 그러나 byte-exact parity 원칙에 따라 그대로 옮긴다.
        let lhs = gate_bt.ask_price + gate_bt.bid_price;
        let rhs = (bin.ask_price + bin.bid_price) * 0.5;
        if lhs < rhs {
            Some("buy")
        } else if lhs > rhs {
            Some("sell")
        } else {
            None
        }
    } else {
        None
    };

    if let Some(s) = side {
        let price = if s == "buy" {
            gate_web_bt.bid_price
        } else {
            gate_web_bt.ask_price
        };
        sig.order_side = Some(s);
        sig.order_price = Some(price);
    }
    sig
}

/// v8 시그널. v0 와의 차이는 `gate_last_trade` 가 gate_web_bt 보다 새 것일 때
/// web book 의 대응 side 가격을 trade 가격으로 대체한다는 점. 시그널 가격이
/// 체결 직후의 가장 최신 가격에 붙도록 해 stale quote 에 대한 방어.
#[inline]
pub fn calculate_signal_v8(
    gate_bt: &BookTickerSnap,
    gate_web_bt: &BookTickerSnap,
    binance_bt: Option<&BookTickerSnap>,
    contract: &GateContract,
    gate_last_trade: Option<TradeSnap>,
) -> SignalResult {
    // 1. 먼저 web book snap 을 복사한다 (원본 불변).
    let mut web = *gate_web_bt;

    // 2. gate_last_trade 가 web 이벤트보다 신선하면 대응 side 가격을 trade 가격으로 대체.
    //    trade.size 부호: >0=buy-taker (ask 사이드 체결) / <0=sell-taker (bid 사이드 체결).
    if let Some(t) = gate_last_trade {
        if t.create_time_ms > web.event_time_ms {
            if t.size > 0.0 {
                web.ask_price = t.price;
            } else if t.size < 0.0 {
                web.bid_price = t.price;
            }
        }
    }

    calculate_signal(gate_bt, &web, binance_bt, contract)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bt(bid: f64, bid_sz: f64, ask: f64, ask_sz: f64, evt_ms: i64) -> BookTickerSnap {
        BookTickerSnap {
            bid_price: bid,
            ask_price: ask,
            bid_size: bid_sz,
            ask_size: ask_sz,
            event_time_ms: evt_ms,
            server_time_ms: evt_ms,
        }
    }

    fn contract() -> GateContract {
        GateContract {
            order_price_round: "0.1".into(),
            order_size_round: "1".into(),
            quanto_multiplier: "0.0001".into(),
        }
    }

    #[test]
    fn no_signal_when_web_book_aligned_with_gate_mid() {
        let gate = bt(100.0, 10.0, 101.0, 10.0, 1); // mid=100.5
        let web = bt(100.0, 10.0, 101.0, 10.0, 1); // web ask=101 > mid, web bid=100 < mid
        let r = calculate_signal(&gate, &web, None, &contract());
        assert!(!r.has_signal());
        assert_eq!(r.gate_mid, 100.5);
    }

    #[test]
    fn buy_signal_when_web_ask_below_gate_mid() {
        let gate = bt(100.0, 10.0, 102.0, 10.0, 1); // mid=101
        let web = bt(99.0, 20.0, 100.5, 7.0, 1); // web ask=100.5 < 101 → buy
        let bin = bt(100.9, 10.0, 101.1, 10.0, 1);
        let r = calculate_signal(&gate, &web, Some(&bin), &contract());
        assert!(r.has_signal());
        assert_eq!(r.order_side, Some("buy"));
        assert_eq!(r.order_price, Some(100.5));
        // orderbook_size 는 web ask_size
        assert_eq!(r.orderbook_size, 7.0);
        // is_binance_valid: buy_price(100.5) < bin_bid(100.9) → true
        assert!(r.is_binance_valid);
        assert!(r.mid_gap_chance_bp > 0.0);
    }

    #[test]
    fn sell_signal_symmetric() {
        let gate = bt(100.0, 10.0, 102.0, 10.0, 1); // mid=101
        let web = bt(101.5, 15.0, 103.0, 10.0, 1); // web bid=101.5 > mid → sell
        let bin = bt(100.0, 10.0, 101.0, 10.0, 1); // bin ask=101 < 101.5 → valid
        let r = calculate_signal(&gate, &web, Some(&bin), &contract());
        assert_eq!(r.order_side, Some("sell"));
        assert_eq!(r.order_price, Some(101.5));
        assert_eq!(r.orderbook_size, 15.0);
        assert!(r.is_binance_valid);
    }

    #[test]
    fn binance_invalid_when_buy_price_not_below_bin_bid() {
        let gate = bt(100.0, 10.0, 102.0, 10.0, 1);
        let web = bt(99.0, 20.0, 100.5, 7.0, 1);
        let bin = bt(100.0, 10.0, 101.0, 10.0, 1); // buy_price(100.5) !< bin_bid(100) → invalid
        let r = calculate_signal(&gate, &web, Some(&bin), &contract());
        assert_eq!(r.order_side, Some("buy"));
        assert!(!r.is_binance_valid);
    }

    #[test]
    fn v8_uses_last_trade_when_newer() {
        let gate = bt(100.0, 10.0, 102.0, 10.0, 1);
        // web 이벤트가 오래됐고 (evt=5), trade 가 더 새것 (evt=10) & buy-taker (size>0).
        let web = bt(101.0, 20.0, 105.0, 7.0, 5); // web 원본은 ask=105 로 시그널 없음.
        let trade = TradeSnap {
            price: 100.8, // 새 ask 가 gate_mid=101 보다 낮아짐 → buy 시그널 발생.
            size: 1.0,
            create_time_ms: 10,
        };
        let r = calculate_signal_v8(&gate, &web, None, &contract(), Some(trade));
        assert_eq!(r.order_side, Some("buy"));
        assert_eq!(r.order_price, Some(100.8));
    }

    #[test]
    fn v8_ignores_stale_trade() {
        let gate = bt(100.0, 10.0, 102.0, 10.0, 100);
        let web = bt(99.0, 20.0, 100.5, 7.0, 200); // evt_ms=200
        let trade = TradeSnap {
            price: 95.0,
            size: -1.0,
            create_time_ms: 150, // 이전
        };
        let r_plain = calculate_signal(&gate, &web, None, &contract());
        let r_v8 = calculate_signal_v8(&gate, &web, None, &contract(), Some(trade));
        // 같아야 한다 (v8 이 trade 를 무시).
        assert_eq!(r_plain.order_price, r_v8.order_price);
        assert_eq!(r_plain.orderbook_size, r_v8.orderbook_size);
    }

    #[test]
    fn degenerate_gate_book_returns_empty_signal() {
        let gate = bt(0.0, 0.0, 0.0, 0.0, 1); // mid=0 → EPS 가드에 걸림
        let web = bt(1.0, 1.0, 2.0, 1.0, 1);
        let r = calculate_signal(&gate, &web, None, &contract());
        assert!(!r.has_signal());
    }

    // ── v6 fallback signal tests ─────────────────────────────────────────

    #[test]
    fn v6_inherits_signal_when_v0_fires() {
        let gate = bt(100.0, 10.0, 102.0, 10.0, 1); // mid=101
        let web = bt(99.0, 20.0, 100.5, 7.0, 1); // web ask < gate_mid → buy
        let r = calculate_signal_v6(&gate, &web, None, &contract());
        assert_eq!(r.order_side, Some("buy"));
        assert_eq!(r.order_price, Some(100.5));
    }

    #[test]
    fn v6_fallback_buy_when_gate_mid_above_web_mid() {
        // v0 시그널 안 나오는 책: web ask=101 (= gate_mid), web bid=99.
        // gate_mid=101, web_mid=100 → gate > web → fallback "buy".
        let gate = bt(100.0, 10.0, 102.0, 10.0, 1); // mid=101
        let web = bt(99.0, 1.0, 101.0, 1.0, 1); // mid=100
        let r = calculate_signal_v6(&gate, &web, None, &contract());
        assert_eq!(r.order_side, Some("buy"));
        // fallback price 는 web bid.
        assert_eq!(r.order_price, Some(99.0));
        // mid_gap_chance_bp 는 채워주지 않음 (v0 가 시그널 없을 때 default 0).
        assert_eq!(r.mid_gap_chance_bp, 0.0);
    }

    #[test]
    fn v6_fallback_sell_when_gate_mid_below_web_mid() {
        let gate = bt(100.0, 10.0, 100.0, 10.0, 1); // mid=100
                                                    // bid 는 gate_mid 와 같게 두어 v0 sell 시그널은 막고, web_mid 만 더 높여
                                                    // v6 fallback sell 분기만 타게 만든다.
        let web = bt(100.0, 1.0, 103.0, 1.0, 1); // mid=101.5
        let r = calculate_signal_v6(&gate, &web, None, &contract());
        assert_eq!(r.order_side, Some("sell"));
        assert_eq!(r.order_price, Some(103.0));
    }

    #[test]
    fn v6_fallback_returns_unsignaled_when_no_binance_and_mids_equal() {
        let gate = bt(100.0, 10.0, 100.0, 10.0, 1); // mid=100
        let web = bt(99.0, 1.0, 101.0, 1.0, 1); // mid=100 (same)
        let r = calculate_signal_v6(&gate, &web, None, &contract());
        assert!(!r.has_signal());
    }
}

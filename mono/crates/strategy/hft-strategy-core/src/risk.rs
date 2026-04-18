//! 리스크 매니지먼트 — 레거시 `handle_chance::handle_chance` + `risk_manager::check_risk_management`
//! 를 하나의 trait 기반 추상으로 재구성.
//!
//! 레거시의 `GateOrderManager` 는 거래소 클라이언트 + 포지션 캐시 + 심볼 메타 + 최근 주문 추적
//! 까지 전부 들고 있는 1500+ 줄 god struct 였다. 코어 로직은 **포지션 조회 / 최근 주문 조회
//! / 한도 조회** 이 세 가지 지점만 있으면 된다. 이를 [`PositionOracle`] trait 으로 추출 →
//! handle_chance 는 순수 함수에 가까워진다 (테스트 가능성 ↑, 락 경합 ↓).
//!
//! 성능 관점의 개선:
//!   * 레거시는 handle_chance 내부에서 `rand::thread_rng()` 호출을 반복 → syscall 없는
//!     generator 지만 내부적으로 TLS 접근 → 호출부에서 이미 뽑은 jitter 를 주입하는
//!     형태로 변경.
//!   * 포지션 iter 누적 (`total_long_usdt`, `total_short_usdt`) 은 PositionOracle 이 미리
//!     계산한 [`ExposureSnapshot`] 을 반환 → 심볼 수만큼 for-loop 반복 없이 상수 시간.

use hft_strategy_config::TradeSettings;
use serde::{Deserialize, Serialize};

use crate::decision::{OrderLevel, OrderSide};

/// 레거시 handle_chance 의 `Chance` 입력. OrderDecision 이 거래소 발주 직전 형태.
#[derive(Debug, Clone)]
pub struct Chance {
    pub symbol: String,
    pub side: OrderSide,
    pub price: f64,
    pub tif: &'static str,
    pub level: OrderLevel,
    /// 현재 주문 사이즈 (contract 단위). 최종적으로 handle_chance 가 조정할 수 있음.
    pub size: i64,
    pub bypass_time_restriction: bool,
    pub bypass_safe_limit_close: bool,
    pub bypass_max_position_size: bool,
}

/// 최근 주문 정보 (레거시 `LastOrder`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LastOrder {
    pub level: OrderLevel,
    pub side: OrderSide,
    pub price: f64,
    pub timestamp_ms: i64,
}

/// 계정 전체의 노출 스냅샷 — handle_chance 의 net_exposure 계산에 사용.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExposureSnapshot {
    pub total_long_usdt: f64,
    pub total_short_usdt: f64,
    /// 특정 심볼의 현재 포지션 (USDT). +는 롱, -는 숏, 0 은 없음.
    pub this_symbol_usdt: f64,
    /// 계정 symbol list 에 이 심볼이 포함되는가.
    pub is_account_symbol: bool,
    /// 이 심볼의 risk_limit (USDT). 0 이하면 무시.
    pub symbol_risk_limit: f64,
    /// 이 심볼의 최소 주문 사이즈 (contract).
    pub min_order_size: i64,
    /// 심볼당 최근 주문 (있으면).
    pub last_order: Option<LastOrder>,
    /// 이 심볼 포지션의 마지막 update 시각 (seconds since epoch).
    /// close_stale 판단에 사용. 0 이면 "정보 없음" — stale 검사 skip.
    pub position_update_time_sec: i64,
    /// 이 심볼의 quanto_multiplier (USDT perp 기본 1.0). close_stale size 계산에 사용.
    pub quanto_multiplier: f64,
}

/// 포지션/주문 관련 쿼리 추상. 실제 구현은 거래소 클라이언트나 SHM 리더로 swap.
pub trait PositionOracle {
    /// 이 주문을 심사하기 직전의 모든 체크 데이터를 한번에 스냅샷.
    fn snapshot(&self, symbol: &str) -> ExposureSnapshot;
    /// handle_chance 가 결정된 주문을 last_order 로 기록할 때 호출.
    fn record_last_order(&self, symbol: &str, order: LastOrder);
}

/// 리스크 매니지먼트 상수. 레거시 `handle_chance` 의 매직 넘버.
#[derive(Debug, Clone, Copy)]
pub struct RiskConfig {
    /// 한 방향 순포지션 허용 ratio. 기본 0.5 (=50%).
    pub max_position_side_ratio: f64,
    /// 레버리지 배수. 레거시 `handle_chance` 기본 1000.0, `risk_manager.rs` 기본 50.0.
    /// v2 에서는 **명시적 설정값**으로 받게 해 환경별 실수 방지.
    pub leverage: f64,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_position_side_ratio: 0.5,
            leverage: 50.0,
        }
    }
}

/// handle_chance 의 결과. 기존 `(i64, f64, bool)` 튜플을 구조체로.
#[derive(Debug, Clone, Copy)]
pub struct RiskCheck {
    /// 실제로 나갈 signed order size (±).
    pub order_size: i64,
    /// 주문 노출 USDT.
    pub usdt_order_size: f64,
    pub bypass_safe_limit_close: bool,
}

/// 호출부에서 미리 샘플링한 time_restriction jitter 값 (ms).
/// 레거시가 handle_chance 안에서 `rand::thread_rng().gen_range(400..=600)` 류를 5번 정도
/// 부르는 게 TLS 접근 + branch predictor pollution 원인이라, 한번에 draw 해서 넘긴다.
#[derive(Debug, Clone, Copy)]
pub struct TimeRestrictionJitter {
    pub same_side_price_ms: i64,
    pub limit_open_ms: i64,
    pub limit_close_ms: i64,
}

impl TimeRestrictionJitter {
    /// 레거시와 동일한 기본값 (랜덤화 범위 중앙값) 으로 즉시 fallback. 테스트용.
    pub fn midpoint(ts: &TradeSettings) -> Self {
        Self {
            same_side_price_ms: (ts.same_side_price_time_restriction_ms_min
                + ts.same_side_price_time_restriction_ms_max)
                / 2,
            limit_open_ms: (ts.limit_open_time_restriction_ms_min
                + ts.limit_open_time_restriction_ms_max)
                / 2,
            limit_close_ms: (ts.limit_close_time_restriction_ms_min
                + ts.limit_close_time_restriction_ms_max)
                / 2,
        }
    }
}

/// 핵심 리스크 체크 함수. 레거시 `handle_chance` 에서 너무 많은 지점으로 쪼개진 검사를
/// 한 함수에 모으되, `PositionOracle` + `RiskConfig` 로 추상화해 테스트 가능하게 구조화.
///
/// 시맨틱은 레거시와 동일:
///   1. 심볼이 계정 목록에 없으면 reject.
///   2. last_order 이 없으면 reject (레거시와 동일 — first-touch 방지).
///   3. 포지션 방향/close 주문 정합성.
///   4. time_restriction 재진입 방지 (동일 가격 / limit_open / limit_close).
///   5. 계정 잔고 + risk_limit + max_position_size 확인.
///   6. net exposure ratio 가 threshold 를 넘지 않는지.
///   7. 심볼당 max_position_size 초과 금지.
///
/// 실패 시 None 반환. 성공 시 `RiskCheck` 반환 — 실제 주문 사이즈가 확정된 상태.
#[allow(clippy::too_many_arguments)]
pub fn handle_chance<O: PositionOracle + ?Sized>(
    oracle: &O,
    chance: &Chance,
    trade_settings: &TradeSettings,
    risk_cfg: &RiskConfig,
    futures_account_total: f64,
    futures_account_unrealised_pnl: f64,
    current_ts_ms: i64,
    jitter: TimeRestrictionJitter,
    exposure: &ExposureSnapshot,
) -> Option<RiskCheck> {
    let symbol = &chance.symbol;

    // 1) 계정 심볼 체크
    if !exposure.is_account_symbol {
        return None;
    }

    // 2) 마지막 주문 없으면 reject (first-touch 보호 — 레거시 시맨틱)
    let last_order = exposure.last_order?;

    // 3) 포지션 방향 체크 (limit_close 전용)
    if chance.level == OrderLevel::LimitClose {
        let pos = exposure.this_symbol_usdt;
        if pos > 0.0 && chance.side == OrderSide::Buy {
            tracing::debug!(%symbol, "HandleChance reject: Long position & close-buy");
            return None;
        }
        if pos < 0.0 && chance.side == OrderSide::Sell {
            tracing::debug!(%symbol, "HandleChance reject: Short position & close-sell");
            return None;
        }
    }

    // 4) time_restriction — bypass 아니면 4단 체크
    if !chance.bypass_time_restriction {
        // 4a) 직전 주문이 동일 (level, side, price) 이고 최근이면 reject
        if last_order.level == chance.level
            && last_order.side == chance.side
            && (last_order.price - chance.price).abs() < 1e-9
            && last_order.timestamp_ms > current_ts_ms - jitter.same_side_price_ms
        {
            return None;
        }
    }

    // 5) 계정 잔고 확인
    let account_balance = futures_account_total + futures_account_unrealised_pnl;
    if account_balance <= 0.0 {
        return None;
    }

    // max_position_size 결정
    let mut max_position_size = if trade_settings.dynamic_max_position_size {
        account_balance * trade_settings.max_position_size_multiplier
    } else {
        trade_settings.max_position_size
    };
    let mut bypass_max_position_size =
        trade_settings.bypass_max_position_size || chance.bypass_max_position_size;
    if exposure.symbol_risk_limit > 0.0 && exposure.symbol_risk_limit < max_position_size {
        max_position_size = max_position_size.min(exposure.symbol_risk_limit);
        bypass_max_position_size = false;
    }

    let is_buy = chance.side.is_buy();
    let pos_usdt = exposure.this_symbol_usdt;
    let is_same_side = (pos_usdt > 0.0 && is_buy) || (pos_usdt < 0.0 && !is_buy);

    // 큰 반대포지션이면 close 주문 허용 플래그
    let mut bypass_safe_limit_close = chance.bypass_safe_limit_close;
    if !is_same_side && pos_usdt.abs() > trade_settings.order_size * 5.0 {
        bypass_safe_limit_close = true;
    }
    // 같은 방향으로 close 시도하면 reject
    if chance.level.is_close() && is_same_side {
        return None;
    }

    // 추가 time_restriction — limit_open / limit_close 각각 별도 jitter
    if !chance.bypass_time_restriction {
        if last_order.level == OrderLevel::LimitOpen
            && is_same_side
            && last_order.side == chance.side
            && last_order.timestamp_ms > current_ts_ms - jitter.limit_open_ms
        {
            return None;
        }
        if last_order.level == OrderLevel::LimitClose
            && last_order.side == chance.side
            && last_order.timestamp_ms > current_ts_ms - jitter.limit_close_ms
        {
            return None;
        }
    }

    // 사이즈 계산
    let mut order_size = chance.size.abs();
    if order_size == 0 {
        // 설정된 USDT order_size 를 기반으로 최소 사이즈 확보
        order_size = exposure.min_order_size;
    }
    order_size = order_size.max(exposure.min_order_size);

    // 주문 USDT 추정 — size * (|pos|/|size|) 기반이 없으므로 order_size 를 USDT-mapped 로
    // 간주. 실제 정확한 매핑은 호출부 (거래소 클라이언트)가 수행하므로 여기서는 레거시의
    // `get_usdt_amount_from_size` 결과를 caller 가 전달하는 것을 전제. 여기서는 설정값.
    let usdt_order_size = if trade_settings.order_size > 0.0 {
        trade_settings.order_size
    } else {
        0.0
    };

    // close 주문이면 close_order_size 로 덮어씀 (포지션 크기의 1/5 도 고려)
    let (final_order_size, final_usdt) = if chance.level.is_close() {
        let scale = (pos_usdt / 5.0).abs();
        let target_usdt = if let Some(cu) = trade_settings.close_order_size {
            cu.max(scale)
        } else {
            usdt_order_size.max(scale)
        };
        // size 는 대략 target_usdt / (pos_usdt / |pos_size|) 이지만, 포지션이 없으면 원본 유지.
        // 실제 변환은 호출부 — 여기서는 usdt 만 넘기고 size 는 원본 유지.
        (order_size, target_usdt)
    } else {
        (order_size, usdt_order_size)
    };

    // 6) net_exposure ratio 체크 (open 주문만)
    if !chance.level.is_close() {
        let (expected_long, expected_short) = if is_buy {
            (
                exposure.total_long_usdt + final_usdt,
                exposure.total_short_usdt,
            )
        } else {
            (
                exposure.total_long_usdt,
                exposure.total_short_usdt + final_usdt,
            )
        };
        let expected_net = expected_long - expected_short;
        let denom = account_balance * risk_cfg.leverage;
        if denom > 0.0 {
            if is_buy
                && expected_net > 0.0
                && expected_net / denom > risk_cfg.max_position_side_ratio
            {
                return None;
            }
            if !is_buy
                && expected_net < 0.0
                && expected_net.abs() / denom > risk_cfg.max_position_side_ratio
            {
                return None;
            }
        }
    }

    // 7) symbol max position 체크 (same-side 가 문제)
    if is_same_side && pos_usdt.abs() + final_usdt > max_position_size && !bypass_max_position_size
    {
        return None;
    }

    // signed size
    let signed_size = if is_buy {
        final_order_size.abs()
    } else {
        -final_order_size.abs()
    };

    // last_order 기록 (oracle 에 저장)
    oracle.record_last_order(
        symbol,
        LastOrder {
            level: chance.level,
            side: chance.side,
            price: chance.price,
            timestamp_ms: current_ts_ms,
        },
    );

    Some(RiskCheck {
        order_size: signed_size,
        usdt_order_size: final_usdt,
        bypass_safe_limit_close,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;

    struct MockOracle {
        snap: Mutex<HashMap<String, ExposureSnapshot>>,
        recorded: Mutex<Vec<(String, LastOrder)>>,
    }
    impl MockOracle {
        fn new() -> Self {
            Self {
                snap: Mutex::new(HashMap::new()),
                recorded: Mutex::new(Vec::new()),
            }
        }
        fn set(&self, sym: &str, exp: ExposureSnapshot) {
            self.snap.lock().insert(sym.to_string(), exp);
        }
    }
    impl PositionOracle for MockOracle {
        fn snapshot(&self, symbol: &str) -> ExposureSnapshot {
            self.snap.lock().get(symbol).cloned().unwrap_or_default()
        }
        fn record_last_order(&self, symbol: &str, o: LastOrder) {
            self.recorded.lock().push((symbol.to_string(), o));
        }
    }

    fn chance(side: OrderSide, level: OrderLevel) -> Chance {
        Chance {
            symbol: "BTC_USDT".into(),
            side,
            price: 100.0,
            tif: "fok",
            level,
            size: 3,
            bypass_time_restriction: false,
            bypass_safe_limit_close: false,
            bypass_max_position_size: false,
        }
    }

    fn exp_with_last(is_acct: bool, last: Option<LastOrder>) -> ExposureSnapshot {
        ExposureSnapshot {
            total_long_usdt: 0.0,
            total_short_usdt: 0.0,
            this_symbol_usdt: 0.0,
            is_account_symbol: is_acct,
            symbol_risk_limit: 0.0,
            min_order_size: 1,
            last_order: last,
            // close_stale 테스트에서는 사용하지 않으므로 0 (= "정보 없음") 으로 둔다.
            position_update_time_sec: 0,
            // USDT perp 기본값 1.0.
            quanto_multiplier: 1.0,
        }
    }

    fn ts_basic() -> TradeSettings {
        TradeSettings {
            order_size: 10.0,
            max_position_size: 1_000.0,
            ..TradeSettings::default()
        }
    }

    #[test]
    fn not_account_symbol_rejected() {
        let oracle = MockOracle::new();
        let c = chance(OrderSide::Buy, OrderLevel::LimitOpen);
        oracle.set(
            &c.symbol,
            exp_with_last(
                false,
                Some(LastOrder {
                    level: OrderLevel::LimitOpen,
                    side: OrderSide::Buy,
                    price: 99.0,
                    timestamp_ms: 0,
                }),
            ),
        );
        let e = oracle.snapshot(&c.symbol);
        let r = handle_chance(
            &oracle,
            &c,
            &ts_basic(),
            &RiskConfig::default(),
            100.0,
            0.0,
            1000,
            TimeRestrictionJitter::midpoint(&ts_basic()),
            &e,
        );
        assert!(r.is_none());
    }

    #[test]
    fn missing_last_order_rejected() {
        let oracle = MockOracle::new();
        let c = chance(OrderSide::Buy, OrderLevel::LimitOpen);
        let e = exp_with_last(true, None);
        let r = handle_chance(
            &oracle,
            &c,
            &ts_basic(),
            &RiskConfig::default(),
            100.0,
            0.0,
            1000,
            TimeRestrictionJitter::midpoint(&ts_basic()),
            &e,
        );
        assert!(r.is_none());
    }

    #[test]
    fn limit_close_wrong_direction_rejected() {
        let oracle = MockOracle::new();
        let c = chance(OrderSide::Buy, OrderLevel::LimitClose);
        let mut e = exp_with_last(
            true,
            Some(LastOrder {
                level: OrderLevel::LimitOpen,
                side: OrderSide::Sell,
                price: 99.0,
                timestamp_ms: 0,
            }),
        );
        e.this_symbol_usdt = 100.0; // 롱 포지션
        let r = handle_chance(
            &oracle,
            &c,
            &ts_basic(),
            &RiskConfig::default(),
            100.0,
            0.0,
            1000,
            TimeRestrictionJitter::midpoint(&ts_basic()),
            &e,
        );
        assert!(r.is_none());
    }

    #[test]
    fn time_restriction_same_price_rejects() {
        let oracle = MockOracle::new();
        let c = chance(OrderSide::Buy, OrderLevel::LimitOpen);
        // 동일 level+side+price, 250ms 전에 주문 → default jitter 중앙값 500ms 이내
        let e = exp_with_last(
            true,
            Some(LastOrder {
                level: OrderLevel::LimitOpen,
                side: OrderSide::Buy,
                price: 100.0,
                timestamp_ms: 750,
            }),
        );
        let r = handle_chance(
            &oracle,
            &c,
            &ts_basic(),
            &RiskConfig::default(),
            100.0,
            0.0,
            1000,
            TimeRestrictionJitter::midpoint(&ts_basic()),
            &e,
        );
        assert!(r.is_none());
    }

    #[test]
    fn approves_when_all_checks_pass() {
        let oracle = MockOracle::new();
        let c = chance(OrderSide::Buy, OrderLevel::LimitOpen);
        // 과거 주문 충분히 오래된 상태
        let e = exp_with_last(
            true,
            Some(LastOrder {
                level: OrderLevel::LimitOpen,
                side: OrderSide::Sell,
                price: 0.0,
                timestamp_ms: 0,
            }),
        );
        let r = handle_chance(
            &oracle,
            &c,
            &ts_basic(),
            &RiskConfig::default(),
            100.0,
            0.0,
            10_000,
            TimeRestrictionJitter::midpoint(&ts_basic()),
            &e,
        )
        .unwrap();
        assert_eq!(r.order_size, 3);
        assert_eq!(oracle.recorded.lock().len(), 1);
        let (sym, rec) = oracle.recorded.lock()[0].clone();
        assert_eq!(sym, "BTC_USDT");
        assert_eq!(rec.level, OrderLevel::LimitOpen);
        assert_eq!(rec.side, OrderSide::Buy);
    }

    #[test]
    fn net_exposure_ratio_caps_buy() {
        let oracle = MockOracle::new();
        let c = chance(OrderSide::Buy, OrderLevel::LimitOpen);
        let mut e = exp_with_last(
            true,
            Some(LastOrder {
                level: OrderLevel::LimitOpen,
                side: OrderSide::Sell,
                price: 0.0,
                timestamp_ms: 0,
            }),
        );
        e.total_long_usdt = 1_000_000.0; // 이미 매우 큼
        let risk = RiskConfig {
            leverage: 1.0,
            max_position_side_ratio: 0.5,
        };
        let r = handle_chance(
            &oracle,
            &c,
            &ts_basic(),
            &risk,
            100.0,
            0.0,
            10_000,
            TimeRestrictionJitter::midpoint(&ts_basic()),
            &e,
        );
        // account_balance=100, leverage=1, ratio_cap=0.5 → 허용 net=50.
        // 기존 long=1M → 즉시 reject.
        assert!(r.is_none());
    }
}

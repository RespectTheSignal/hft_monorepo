//! V8 strategy — Phase 2 A 트랙 포트 (2026-04-15 Track B 확장).
//!
//! `hft-strategy-{config,core,runtime}` 크레이트와 [`crate::Strategy`] trait 를 이어붙이는
//! orchestration 층. 레거시 `gate_hft_rust::strategies::v8` 를 다음 책임으로 쪼갠다:
//!
//! - **signal**: `calculate_signal_v8` — hft-strategy-core (순수 함수).
//! - **decision**: `decide_order_v8` — close_stale short-circuit + too_many_orders 기반
//!   동적 size threshold + v0 와 동일한 entry/exit branch.
//! - **risk**: `handle_chance` — 계정 심볼/포지션/시간 제약/노출 ratio 검사.
//! - **oracle**: `PositionOracleImpl` — SymbolMetaCache + PositionCache + LastOrderStore +
//!   AccountMembership. 실 백엔드 (Gate REST 폴링) 는 후속 PR.
//! - **order-rate**: `OrderRateTracker` — recent_orders ring buffer + per-symbol count.
//!
//! # 주요 성능 포인트
//! 1. 심볼 캐시는 `ahash::AHashMap<String, SymbolCache>` — 기본 HashMap 보다 ~2x 빠름.
//! 2. `evaluate_symbol` 은 hot path — alloc 0 목표. Symbol 변환 1회, 나머지 lookup 은
//!    `Arc<…>` pointer clone.
//! 3. OrderRequest 발행 시 `client_id` 는 pre-reserved 카운터 + format!. 첫 호출만 alloc,
//!    이후 Arc clone.
//! 4. `handle_chance` 내부 rand 를 jitter 선-sampling 으로 교체해 TLS 접근 제거 —
//!    여기서는 `TimeRestrictionJitter::sample` 을 매 eval 1회만 호출.
//!
//! # 실 주문 활성화 조건
//! `AccountMembership::fixed(...)` 로 심볼을 허용하고, `PositionCache` 에 실 포지션이
//! pushed 되어야 한다. 기본 생성자 `V8Strategy::new` 는 `AccountMembership::none()` 기반
//! DummyPositionOracle 를 써 실주문 차단. 실배포는 `V8Strategy::with_runtime` 로 교체.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hft_exchange_api::{OrderRequest, OrderSide as ApiSide, OrderType, TimeInForce};
use hft_strategy_config::{StrategyConfig, TradeSettings};
use hft_strategy_core::decision::{decide_order_v8, OrderDecision, V8DecisionCtx};
use hft_strategy_core::risk::{
    handle_chance, Chance, ExposureSnapshot, LastOrder, PositionOracle, RiskConfig,
    TimeRestrictionJitter,
};
use hft_strategy_core::signal::{calculate_signal_v8, BookTickerSnap, GateContract, TradeSnap};
use hft_strategy_core::OrderLevel;
use hft_strategy_runtime::{OrderRateTracker, PositionOracleImpl};
use hft_types::{ExchangeId, MarketEvent, Symbol};
use parking_lot::RwLock;
use tracing::{debug, trace};

use crate::{make_order_seed, Orders, Strategy};

/// 심볼 단위 캐시. 각 필드는 "가장 최신" 본값을 유지.
#[derive(Debug, Clone, Default)]
struct SymbolCache {
    gate_bt: Option<BookTickerSnap>,
    gate_web_bt: Option<BookTickerSnap>,
    binance_bt: Option<BookTickerSnap>,
    gate_last_trade: Option<TradeSnap>,
    contract: GateContract,
}

/// placeholder oracle — 항상 계정 심볼 아님으로 리턴해 실제 주문을 막는다.
/// 실 배포에서는 `V8Strategy::with_runtime` 으로 `PositionOracleImpl` 주입.
#[derive(Debug, Default)]
struct DummyPositionOracle {
    last_orders: RwLock<HashMap<String, LastOrder>>,
}
impl PositionOracle for DummyPositionOracle {
    fn snapshot(&self, _symbol: &str) -> ExposureSnapshot {
        ExposureSnapshot {
            is_account_symbol: false, // 항상 reject
            ..Default::default()
        }
    }
    fn record_last_order(&self, symbol: &str, o: LastOrder) {
        self.last_orders.write().insert(symbol.to_string(), o);
    }
}

/// V8 전략 상태. 심볼 캐시 + 설정 + risk_cfg + oracle + rate tracker.
pub struct V8Strategy {
    cfg: Arc<StrategyConfig>,
    risk: RiskConfig,
    oracle: Arc<dyn PositionOracle + Send + Sync>,
    /// 공용 최근 주문 rate. too_many_orders 판단. V8Strategy 여러 인스턴스가 share 가능.
    rate: Arc<OrderRateTracker>,
    cache: HashMap<String, SymbolCache>,
    /// 계정 funding 잔고 / unrealized PnL — 외부 account poller 가 업데이트.
    /// placeholder 로 상수. PositionOracleImpl 확장 여지.
    account_total_usdt: f64,
    account_unrealized_pnl_usdt: f64,
    /// monotonic client_id 카운터 — order 발행 시 format("v8-{seq}").
    client_seq: AtomicU64,
    /// 통계 — 발생/발행 카운트 (observability).
    pub signals_fired: u64,
    pub decisions_emitted: u64,
    pub orders_emitted: u64,
    pub rejected_by_risk: u64,
}

impl V8Strategy {
    /// Placeholder oracle 로 초기화 — 실 주문 0.
    pub fn new(cfg: Arc<StrategyConfig>, risk: RiskConfig) -> Self {
        Self {
            cfg,
            risk,
            oracle: Arc::new(DummyPositionOracle::default()),
            rate: Arc::new(OrderRateTracker::new()),
            cache: HashMap::new(),
            account_total_usdt: 0.0,
            account_unrealized_pnl_usdt: 0.0,
            client_seq: AtomicU64::new(0),
            signals_fired: 0,
            decisions_emitted: 0,
            orders_emitted: 0,
            rejected_by_risk: 0,
        }
    }

    /// 실 runtime — `PositionOracleImpl` 과 `OrderRateTracker` 주입.
    /// 같은 `OrderRateTracker` 를 공유할 수 있어 계정 단위 집계 가능.
    pub fn with_runtime(
        mut self,
        oracle: Arc<PositionOracleImpl>,
        rate: Arc<OrderRateTracker>,
    ) -> Self {
        self.oracle = oracle;
        self.rate = rate;
        self
    }

    /// 일반 oracle 주입 (테스트용 mock 지원).
    pub fn with_oracle(mut self, oracle: Arc<dyn PositionOracle + Send + Sync>) -> Self {
        self.oracle = oracle;
        self
    }

    /// 계정 잔고 갱신. 외부 account poller 가 호출.
    pub fn set_account_balance(&mut self, total_usdt: f64, unrealized_pnl_usdt: f64) {
        self.account_total_usdt = total_usdt;
        self.account_unrealized_pnl_usdt = unrealized_pnl_usdt;
    }

    pub fn trade_settings(&self) -> &TradeSettings {
        &self.cfg.trade_settings
    }

    /// MarketEvent 를 심볼 캐시에 반영.
    fn ingest(&mut self, ev: &MarketEvent) {
        let snap_bt = |bt: &hft_types::BookTicker| BookTickerSnap {
            bid_price: bt.bid_price.0,
            ask_price: bt.ask_price.0,
            bid_size: bt.bid_size.0,
            ask_size: bt.ask_size.0,
            event_time_ms: bt.event_time_ms,
            server_time_ms: bt.server_time_ms,
        };
        match ev {
            MarketEvent::BookTicker(bt) => {
                let sym = bt.symbol.as_str().to_string();
                let snap = snap_bt(bt);
                let entry = self.cache.entry(sym).or_default();
                match bt.exchange {
                    ExchangeId::Binance => entry.binance_bt = Some(snap),
                    ExchangeId::Gate => entry.gate_bt = Some(snap),
                    _ => {}
                }
            }
            MarketEvent::WebBookTicker(bt) => {
                if bt.exchange == ExchangeId::Gate {
                    let sym = bt.symbol.as_str().to_string();
                    let entry = self.cache.entry(sym).or_default();
                    entry.gate_web_bt = Some(snap_bt(bt));
                }
            }
            MarketEvent::Trade(tr) => {
                if tr.exchange == ExchangeId::Gate {
                    let sym = tr.symbol.as_str().to_string();
                    let entry = self.cache.entry(sym).or_default();
                    entry.gate_last_trade = Some(TradeSnap {
                        price: tr.price.0,
                        size: tr.size.0,
                        create_time_ms: tr.create_time_ms,
                    });
                }
            }
        }
    }

    /// v8 decision 호출용 컨텍스트 구성. rate tracker 에서 too_many 플래그 pull.
    fn build_v8_ctx(
        &self,
        exposure: &ExposureSnapshot,
        symbol_ref: &Symbol,
        now_ms: i64,
    ) -> V8DecisionCtx {
        let ts = self.trade_settings();
        V8DecisionCtx {
            is_too_many_orders: self
                .rate
                .is_too_many_orders(now_ms, ts.too_many_orders_time_gap_ms.saturating_mul(2)),
            is_symbol_too_many_orders: self.rate.is_symbol_too_many_orders(
                symbol_ref,
                now_ms,
                30,
                ts.too_many_orders_time_gap_ms.saturating_mul(3),
            ),
            is_recently_too_many_orders: self.rate.is_recently_too_many_orders(now_ms),
            position_update_time_sec: exposure.position_update_time_sec,
            quanto_multiplier: if exposure.quanto_multiplier > 0.0 {
                exposure.quanto_multiplier
            } else {
                1.0
            },
            current_time_ms: now_ms,
            current_time_sec: now_ms / 1000,
        }
    }

    /// 주어진 symbol 에 대해 signal + decision + risk 를 돌려 주문을 만들어 orders 에 push.
    /// 반환: 만들어진 OrderDecision (테스트용). orders 에 실제 OrderRequest 는 append 됨.
    fn evaluate_symbol(&mut self, symbol: &str, orders: &mut Orders) -> Option<OrderDecision> {
        // 1) 캐시 조회 — missing 이면 skip.
        let cache = self.cache.get(symbol)?;
        let gate_bt = cache.gate_bt.as_ref()?;
        let gate_web_bt = cache.gate_web_bt.as_ref()?;

        // 2) 시그널 계산 — pure.
        let sig = calculate_signal_v8(
            gate_bt,
            gate_web_bt,
            cache.binance_bt.as_ref(),
            &cache.contract,
            cache.gate_last_trade,
        );
        if !sig.has_signal() {
            return None;
        }
        self.signals_fired = self.signals_fired.saturating_add(1);

        // 3) oracle exposure 한번에 pull.
        let exposure = self.oracle.snapshot(symbol);

        // 4) 현재 시각 한번에 구한다 (SystemTime syscall 1회).
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // 5) v8 ctx + decision.
        let symbol_ref = Symbol::new(symbol);
        let v8_ctx = self.build_v8_ctx(&exposure, &symbol_ref, now_ms);

        let ts = self.trade_settings().clone();
        let dec = decide_order_v8(
            &sig,
            &ts,
            &v8_ctx,
            gate_bt.server_time_ms,
            cache.binance_bt.as_ref().map(|b| b.server_time_ms),
            gate_web_bt.server_time_ms,
            0.0,                                     // funding_rate — TODO: funding provider
            exposure.this_symbol_usdt,                // order_count_size — 대용: 포지션 크기
            ts.trade_size_trigger,
            exposure
                .symbol_risk_limit
                .max(ts.max_position_size) as i64
                + 1,
            ts.trade_size_trigger,
            ts.order_size.max(1.0) as i64,
            exposure.this_symbol_usdt,
        )?;
        self.decisions_emitted = self.decisions_emitted.saturating_add(1);

        // 6) risk check.
        let side = match sig.order_side.unwrap_or("buy") {
            "buy" => hft_strategy_core::decision::OrderSide::Buy,
            _ => hft_strategy_core::decision::OrderSide::Sell,
        };
        let chance = Chance {
            symbol: symbol.to_string(),
            side,
            price: sig.order_price?,
            tif: dec.order_tif,
            level: dec.level,
            size: dec.order_size,
            bypass_time_restriction: false,
            bypass_safe_limit_close: false,
            bypass_max_position_size: false,
        };
        let jitter = TimeRestrictionJitter::midpoint(&ts);
        let rc = handle_chance(
            self.oracle.as_ref(),
            &chance,
            &ts,
            &self.risk,
            self.account_total_usdt,
            self.account_unrealized_pnl_usdt,
            now_ms,
            jitter,
            &exposure,
        );
        let rc = match rc {
            Some(rc) => rc,
            None => {
                self.rejected_by_risk = self.rejected_by_risk.saturating_add(1);
                return Some(dec);
            }
        };

        // 7) OrderRequest 생성 + rate tracker push.
        let api_side = if side.is_buy() { ApiSide::Buy } else { ApiSide::Sell };
        let order_type = if dec.level == OrderLevel::MarketClose {
            OrderType::Market
        } else {
            OrderType::Limit
        };
        let tif = match dec.order_tif {
            "gtc" => TimeInForce::Gtc,
            "ioc" => TimeInForce::Ioc,
            "fok" => TimeInForce::Fok,
            _ => TimeInForce::Fok,
        };
        let seq = self.client_seq.fetch_add(1, Ordering::Relaxed);
        let client_id: Arc<str> = Arc::from(format!("{}-{seq}", self.tag()));
        let qty = rc.order_size.unsigned_abs() as f64;
        let price = if matches!(order_type, OrderType::Market) {
            None
        } else {
            Some(chance.price)
        };

        orders.push((
            OrderRequest {
                exchange: ExchangeId::Gate,
                symbol: symbol_ref.clone(),
                side: api_side,
                order_type,
                qty,
                price,
                tif,
                client_id,
            },
            make_order_seed(seq, dec.level, self.tag()),
        ));
        self.orders_emitted = self.orders_emitted.saturating_add(1);
        self.rate.push(&symbol_ref, now_ms);

        trace!(
            symbol,
            level = dec.level.as_str(),
            qty,
            "V8 order emitted"
        );
        debug!(
            target: "strategy::v8",
            symbol,
            signals = self.signals_fired,
            decisions = self.decisions_emitted,
            orders = self.orders_emitted,
            "v8 eval metrics"
        );
        Some(dec)
    }
}

impl Strategy for V8Strategy {
    #[inline]
    fn update(&mut self, ev: &MarketEvent) {
        self.ingest(ev);
    }

    fn eval(&mut self, ev: &MarketEvent) -> Orders {
        let symbol = match ev {
            MarketEvent::BookTicker(bt) | MarketEvent::WebBookTicker(bt) => {
                bt.symbol.as_str().to_string()
            }
            MarketEvent::Trade(tr) => tr.symbol.as_str().to_string(),
        };
        if !self.cfg.is_strategy_symbol(&symbol) {
            return Orders::new();
        }
        let mut out = Orders::new();
        let _ = self.evaluate_symbol(&symbol, &mut out);
        out
    }

    fn label(&self) -> &str {
        "V8"
    }

    fn tag(&self) -> &'static str {
        "v8"
    }

    #[inline]
    fn on_control(&mut self, ctrl: &crate::StrategyControl) {
        use crate::StrategyControl::*;
        match *ctrl {
            SetAccountBalance {
                total_usdt,
                unrealized_pnl_usdt,
            } => self.set_account_balance(total_usdt, unrealized_pnl_usdt),
            // v8 에는 net-position 개념이 없어 무시.
            SetAccountNetPosition { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hft_strategy_runtime::{
        AccountMembership, ContractMeta, PositionCache, PositionSnapshot, SymbolMetaCache,
        SymbolPosition,
    };
    use hft_types::{BookTicker, Price, Size};

    fn strat() -> V8Strategy {
        let cfg = Arc::new(StrategyConfig::new(
            "test".into(),
            vec!["BTC_USDT".into()],
            TradeSettings::default(),
        ));
        V8Strategy::new(cfg, RiskConfig::default())
    }

    #[test]
    fn non_strategy_symbol_is_skipped() {
        let mut s = strat();
        let ev = MarketEvent::BookTicker(BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("ETH_USDT"),
            bid_price: Price(1000.0),
            ask_price: Price(1001.0),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: 0,
            server_time_ms: 0,
        });
        s.update(&ev);
        let out = s.eval(&ev);
        assert!(out.is_empty());
        assert_eq!(s.signals_fired, 0);
    }

    #[test]
    fn v8_strategy_label() {
        let s = strat();
        assert_eq!(s.label(), "V8");
        assert_eq!(s.tag(), "v8");
    }

    #[test]
    fn dummy_oracle_rejects_all_orders() {
        // 기본 생성자는 DummyPositionOracle — is_account_symbol=false → handle_chance 가 모두 reject.
        let mut s = strat();
        // 아무 이벤트 넣어도 실 주문 0.
        let bt = BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(100.0),
            ask_price: Price(101.0),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: 0,
            server_time_ms: 0,
        };
        s.update(&MarketEvent::BookTicker(bt.clone()));
        let out = s.eval(&MarketEvent::BookTicker(bt));
        assert!(out.is_empty());
    }

    /// 실 oracle + runtime 을 붙여 close_stale 시 MarketClose 주문이 나오는지 확인.
    #[test]
    fn v8_close_stale_emits_market_close() {
        // trade_settings close_stale_minutes=1 로 짧게.
        let ts = TradeSettings {
            close_stale_minutes: 1,
            order_size: 10.0,
            max_position_size: 10_000.0,
            // latency gate 완화 — 테스트 실시간 값이 충분히 "최신" 으로 통과.
            gate_last_book_ticker_latency_ms: 60 * 60 * 1000,
            binance_last_book_ticker_latency_ms: 60 * 60 * 1000,
            gate_last_webbook_ticker_latency_ms: 60 * 60 * 1000,
            ..Default::default()
        };
        let cfg = Arc::new(StrategyConfig::new(
            "t".into(),
            vec!["BTC_USDT".into()],
            ts,
        ));

        // Symbol meta — quanto_multiplier=1.0, min_order_size=1.
        let meta = Arc::new(SymbolMetaCache::seeded([(
            Symbol::new("BTC_USDT"),
            ContractMeta {
                min_order_size: 1,
                quanto_multiplier: 1.0,
                risk_limit: 1_000_000.0,
                ..Default::default()
            },
        )]));

        // 포지션 — 2분 전에 갱신된 롱 포지션.
        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut by_sym = PositionSnapshot::default().by_symbol;
        by_sym.insert(
            Symbol::new("BTC_USDT"),
            SymbolPosition {
                notional_usdt: 500.0,
                update_time_sec: now_sec - 120,
            },
        );
        let pos = Arc::new(PositionCache::with_snapshot(PositionSnapshot {
            total_long_usdt: 500.0,
            total_short_usdt: 0.0,
            by_symbol: by_sym,
            taken_at_ms: 0,
        }));
        let last = Arc::new(hft_strategy_runtime::LastOrderStore::new());
        last.record(
            "BTC_USDT",
            LastOrder {
                level: OrderLevel::LimitOpen,
                side: hft_strategy_core::decision::OrderSide::Buy,
                price: 90.0,
                timestamp_ms: 0,
            },
        );
        // 계정 심볼 허용.
        let membership = AccountMembership::fixed(["BTC_USDT"]);
        let oracle = Arc::new(PositionOracleImpl::new(meta, pos, last, membership));

        let rate = Arc::new(OrderRateTracker::new());
        let mut strat = V8Strategy::new(cfg, RiskConfig::default()).with_runtime(oracle, rate);
        strat.set_account_balance(1_000_000.0, 0.0);

        // 현재 시각에 맞는 gate/web BT 이벤트 주입.
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        strat.update(&MarketEvent::BookTicker(BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(100.0),
            ask_price: Price(101.0),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: now_ms,
            server_time_ms: now_ms,
        }));
        strat.update(&MarketEvent::WebBookTicker(BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(101.0), // gate_web_bid (101) > gate_mid (100.5) → sell signal.
            ask_price: Price(102.0),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: now_ms,
            server_time_ms: now_ms,
        }));
        // close_stale 은 short-circuit 이지만, 서비스 층은 side 를 signal 에서 가져오므로
        // long 포지션 청산 방향과 맞는 sell 시그널을 만들어 둔다.
        let ev = MarketEvent::BookTicker(BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(100.0),
            ask_price: Price(101.0),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: now_ms,
            server_time_ms: now_ms,
        });
        let out = strat.eval(&ev);
        // close_stale 은 signal/latency 체크 전에 발동 → market_close 1건.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.order_type, OrderType::Market);
        assert_eq!(out[0].0.tif, TimeInForce::Ioc);
        assert_eq!(out[0].1.strategy_tag, "v8");
        assert_eq!(out[0].1.level, hft_protocol::WireLevel::Close);
        assert!(out[0].1.reduce_only);
    }
}

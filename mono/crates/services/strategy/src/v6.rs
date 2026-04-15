//! V6 strategy — Phase 2 A 트랙 포트 (2026-04-15 Track A 확장 #3).
//!
//! 레거시 `gate_hft_rust::strategies::v6` 의 1:1 포트.
//!
//! v8 과 비교한 차이점:
//! - **signal**: `calculate_signal_v6` (v0 기본 + fallback 분기). v8 은 last_trade
//!   기반 가격 보정. v6 는 last_trade 안 씀.
//! - **decision**: `decide_order_v6` — close_stale + size_threshold_multiplier
//!   (1x/2x/4x) + is_order_net_position_close_side 시 size *2.
//!   v8 의 `is_recently_too_many_orders` 부스트 없음.
//! - **risk**: 동일 (`handle_chance` + `RiskConfig`).
//! - **rate tracker**: 공유 가능. v6 는 `is_recently_too_many_orders` 를 size 에 안
//!   쓰지만 hand-off 후 risk 에서 쓰는 경로(미래)를 위해 동일 인터페이스 유지.
//!
//! # 안전 default
//! `V6Strategy::new` 는 `DummyPositionOracle` (account_symbol=false) → 실 주문 0건.
//! 실 배포는 `with_runtime`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hft_exchange_api::{OrderRequest, OrderSide as ApiSide, OrderType, TimeInForce};
use hft_strategy_config::{StrategyConfig, TradeSettings};
use hft_strategy_core::decision::{decide_order_v6, OrderDecision, V6DecisionCtx};
use hft_strategy_core::risk::{
    handle_chance, Chance, ExposureSnapshot, LastOrder, PositionOracle, RiskConfig,
    TimeRestrictionJitter,
};
use hft_strategy_core::signal::{calculate_signal_v6, BookTickerSnap, GateContract, TradeSnap};
use hft_strategy_core::OrderLevel;
use hft_strategy_runtime::{OrderRateTracker, PositionOracleImpl};
use hft_types::{ExchangeId, MarketEvent, Symbol};
use parking_lot::RwLock;
use tracing::{debug, trace};

use crate::{Orders, Strategy};

/// 심볼별 hot cache. v8 와 동일 구조.
#[derive(Debug, Clone, Default)]
struct SymbolCache {
    gate_bt: Option<BookTickerSnap>,
    gate_web_bt: Option<BookTickerSnap>,
    binance_bt: Option<BookTickerSnap>,
    gate_last_trade: Option<TradeSnap>,
    contract: GateContract,
}

/// 안전 default oracle — 항상 reject.
#[derive(Debug, Default)]
struct DummyPositionOracle {
    last_orders: RwLock<HashMap<String, LastOrder>>,
}
impl PositionOracle for DummyPositionOracle {
    fn snapshot(&self, _symbol: &str) -> ExposureSnapshot {
        ExposureSnapshot {
            is_account_symbol: false,
            ..Default::default()
        }
    }
    fn record_last_order(&self, symbol: &str, o: LastOrder) {
        self.last_orders.write().insert(symbol.to_string(), o);
    }
}

/// V6 전략.
pub struct V6Strategy {
    cfg: Arc<StrategyConfig>,
    risk: RiskConfig,
    oracle: Arc<dyn PositionOracle + Send + Sync>,
    rate: Arc<OrderRateTracker>,
    cache: HashMap<String, SymbolCache>,
    account_total_usdt: f64,
    account_unrealized_pnl_usdt: f64,
    /// 계정 net 포지션 USDT (v6 의 is_order_net_position_close_side 판단 용).
    /// 외부 account poller 가 set_account_net_position 으로 갱신.
    account_net_position_usdt: f64,
    client_seq: AtomicU64,
    pub signals_fired: u64,
    pub decisions_emitted: u64,
    pub orders_emitted: u64,
    pub rejected_by_risk: u64,
}

impl V6Strategy {
    pub fn new(cfg: Arc<StrategyConfig>, risk: RiskConfig) -> Self {
        Self {
            cfg,
            risk,
            oracle: Arc::new(DummyPositionOracle::default()),
            rate: Arc::new(OrderRateTracker::new()),
            cache: HashMap::new(),
            account_total_usdt: 0.0,
            account_unrealized_pnl_usdt: 0.0,
            account_net_position_usdt: 0.0,
            client_seq: AtomicU64::new(0),
            signals_fired: 0,
            decisions_emitted: 0,
            orders_emitted: 0,
            rejected_by_risk: 0,
        }
    }

    pub fn with_runtime(
        mut self,
        oracle: Arc<PositionOracleImpl>,
        rate: Arc<OrderRateTracker>,
    ) -> Self {
        self.oracle = oracle;
        self.rate = rate;
        self
    }

    pub fn with_oracle(mut self, oracle: Arc<dyn PositionOracle + Send + Sync>) -> Self {
        self.oracle = oracle;
        self
    }

    pub fn set_account_balance(&mut self, total_usdt: f64, unrealized_pnl_usdt: f64) {
        self.account_total_usdt = total_usdt;
        self.account_unrealized_pnl_usdt = unrealized_pnl_usdt;
    }

    /// 외부 poller 가 호출. v6 만의 추가 입력.
    pub fn set_account_net_position(&mut self, net_usdt: f64) {
        self.account_net_position_usdt = net_usdt;
    }

    pub fn trade_settings(&self) -> &TradeSettings {
        &self.cfg.trade_settings
    }

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

    fn build_v6_ctx(
        &self,
        exposure: &ExposureSnapshot,
        symbol_ref: &Symbol,
        now_ms: i64,
    ) -> V6DecisionCtx {
        let ts = self.trade_settings();
        V6DecisionCtx {
            is_too_many_orders: self
                .rate
                .is_too_many_orders(now_ms, ts.too_many_orders_time_gap_ms.saturating_mul(2)),
            is_symbol_too_many_orders: self.rate.is_symbol_too_many_orders(
                symbol_ref,
                now_ms,
                30,
                ts.too_many_orders_time_gap_ms.saturating_mul(3),
            ),
            position_update_time_sec: exposure.position_update_time_sec,
            quanto_multiplier: if exposure.quanto_multiplier > 0.0 {
                exposure.quanto_multiplier
            } else {
                1.0
            },
            account_net_position_usdt: self.account_net_position_usdt,
            current_time_ms: now_ms,
            current_time_sec: now_ms / 1000,
        }
    }

    fn evaluate_symbol(&mut self, symbol: &str, orders: &mut Orders) -> Option<OrderDecision> {
        let cache = self.cache.get(symbol)?;
        let gate_bt = cache.gate_bt.as_ref()?;
        let gate_web_bt = cache.gate_web_bt.as_ref()?;

        // v6 signal (v0 + fallback)
        let sig = calculate_signal_v6(
            gate_bt,
            gate_web_bt,
            cache.binance_bt.as_ref(),
            &cache.contract,
        );
        if !sig.has_signal() {
            return None;
        }
        self.signals_fired = self.signals_fired.saturating_add(1);

        let exposure = self.oracle.snapshot(symbol);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let symbol_ref = Symbol::new(symbol);
        let ctx = self.build_v6_ctx(&exposure, &symbol_ref, now_ms);

        let ts = self.trade_settings();
        let dec = decide_order_v6(
            &sig,
            ts,
            &ctx,
            gate_bt.server_time_ms,
            cache.binance_bt.as_ref().map(|b| b.server_time_ms),
            gate_web_bt.server_time_ms,
            0.0,
            exposure.this_symbol_usdt,
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
        let jitter = TimeRestrictionJitter::midpoint(ts);
        let rc = handle_chance(
            self.oracle.as_ref(),
            &chance,
            ts,
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
        let client_id: Arc<str> = Arc::from(format!("v6-{seq}"));
        let qty = rc.order_size.unsigned_abs() as f64;
        let price = if matches!(order_type, OrderType::Market) {
            None
        } else {
            Some(chance.price)
        };

        orders.push(OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: symbol_ref.clone(),
            side: api_side,
            order_type,
            qty,
            price,
            tif,
            client_id,
        });
        self.orders_emitted = self.orders_emitted.saturating_add(1);
        self.rate.push(&symbol_ref, now_ms);

        trace!(
            symbol,
            level = dec.level.as_str(),
            qty,
            "V6 order emitted"
        );
        debug!(
            target: "strategy::v6",
            symbol,
            signals = self.signals_fired,
            decisions = self.decisions_emitted,
            orders = self.orders_emitted,
            "v6 eval metrics"
        );
        Some(dec)
    }
}

impl Strategy for V6Strategy {
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
        "V6"
    }

    #[inline]
    fn on_control(&mut self, ctrl: &crate::StrategyControl) {
        use crate::StrategyControl::*;
        match *ctrl {
            SetAccountBalance {
                total_usdt,
                unrealized_pnl_usdt,
            } => self.set_account_balance(total_usdt, unrealized_pnl_usdt),
            SetAccountNetPosition { net_usdt } => self.set_account_net_position(net_usdt),
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
    use ahash::AHashMap as AMap;
    use hft_types::{BookTicker, Price, Size};

    fn strat() -> V6Strategy {
        let cfg = Arc::new(StrategyConfig::new(
            "test".into(),
            vec!["BTC_USDT".into()],
            TradeSettings::default(),
        ));
        V6Strategy::new(cfg, RiskConfig::default())
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
    fn v6_strategy_label() {
        let s = strat();
        assert_eq!(s.label(), "V6");
    }

    #[test]
    fn dummy_oracle_rejects_all_orders() {
        let mut s = strat();
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

    /// 실 oracle + runtime → close_stale 시 MarketClose.
    #[test]
    fn v6_close_stale_emits_market_close() {
        let mut ts = TradeSettings::default();
        ts.close_stale_minutes = 1;
        ts.order_size = 10.0;
        ts.max_position_size = 10_000.0;
        ts.gate_last_book_ticker_latency_ms = 60 * 60 * 1000;
        ts.binance_last_book_ticker_latency_ms = 60 * 60 * 1000;
        ts.gate_last_webbook_ticker_latency_ms = 60 * 60 * 1000;
        let cfg = Arc::new(StrategyConfig::new(
            "t".into(),
            vec!["BTC_USDT".into()],
            ts,
        ));
        let meta = Arc::new(SymbolMetaCache::seeded([(
            Symbol::new("BTC_USDT"),
            ContractMeta {
                min_order_size: 1,
                quanto_multiplier: 1.0,
                risk_limit: 1_000_000.0,
                ..Default::default()
            },
        )]));
        let now_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut by_sym = AMap::new();
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
        let membership = AccountMembership::fixed(["BTC_USDT"]);
        let oracle = Arc::new(PositionOracleImpl::new(meta, pos, last, membership));
        let rate = Arc::new(OrderRateTracker::new());
        let mut strat = V6Strategy::new(cfg, RiskConfig::default()).with_runtime(oracle, rate);
        strat.set_account_balance(1_000_000.0, 0.0);
        strat.set_account_net_position(500.0); // 계정 long 500

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
        // web book — fallback 분기 또는 v0 buy 시그널 트리거.
        strat.update(&MarketEvent::WebBookTicker(BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(99.0),
            ask_price: Price(100.0),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: now_ms,
            server_time_ms: now_ms,
        }));
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
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].order_type, OrderType::Market);
        assert_eq!(out[0].tif, TimeInForce::Ioc);
    }
}

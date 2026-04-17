//! V7 strategy — Phase 2 A 트랙 포트 (2026-04-15 Track A 확장 #3).
//!
//! 레거시 `gate_hft_rust::strategies::v7` 의 1:1 포트.
//!
//! v6/v8 와 비교한 차이점:
//! - **signal**: `calculate_signal` (v0 기본). fallback 없음. last_trade 보정 없음.
//! - **decision**: `decide_order_v7` —
//!     * close_stale 없음
//!     * size_threshold_multiplier 없음 (size_trigger 그대로 사용)
//!     * is_order_net_position_close_side 시 size *2 (v6 와 동일)
//!     * narrative-close 분기에서 only_close 에 따라 limit_close ↔ limit_open 양분기.
//! - **rate tracker 미사용**: V7 자체 로직에서 too_many_orders 신호를 안 본다.
//!   다만 외부 인스턴스 (account-wide) 로 push 만 하도록 with_rate 도 제공.
//!
//! # 안전 default
//! `V7Strategy::new` → `DummyPositionOracle` (account_symbol=false) → 실 주문 0건.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hft_exchange_api::{OrderRequest, OrderSide as ApiSide, OrderType, TimeInForce};
use hft_strategy_config::{StrategyConfig, TradeSettings};
use hft_strategy_core::decision::{decide_order_v7, OrderDecision, V7DecisionExtras};
use hft_strategy_core::risk::{
    handle_chance, Chance, ExposureSnapshot, LastOrder, PositionOracle, RiskConfig,
    TimeRestrictionJitter,
};
use hft_strategy_core::signal::{calculate_signal, BookTickerSnap, GateContract, TradeSnap};
use hft_strategy_core::OrderLevel;
use hft_strategy_runtime::{OrderRateTracker, PositionOracleImpl};
use hft_telemetry::{counter_inc, CounterKey};
use hft_types::{ExchangeId, MarketEvent, Symbol};
use parking_lot::RwLock;
use tracing::{debug, trace};

use crate::{make_order_seed, Orders, Strategy};

#[derive(Debug, Clone, Default)]
struct SymbolCache {
    gate_bt: Option<BookTickerSnap>,
    gate_web_bt: Option<BookTickerSnap>,
    binance_bt: Option<BookTickerSnap>,
    /// 보관만 — v7 signal 은 사용하지 않음. 통계/ingest 호환성 유지.
    gate_last_trade: Option<TradeSnap>,
    contract: GateContract,
}

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

pub struct V7Strategy {
    cfg: Arc<StrategyConfig>,
    risk: RiskConfig,
    oracle: Arc<dyn PositionOracle + Send + Sync>,
    /// rate tracker 는 v7 자체 로직엔 안 쓰지만 account-wide push 를 위해 보유.
    rate: Option<Arc<OrderRateTracker>>,
    cache: HashMap<String, SymbolCache>,
    account_total_usdt: f64,
    account_unrealized_pnl_usdt: f64,
    account_net_position_usdt: f64,
    client_seq: AtomicU64,
    pub signals_fired: u64,
    pub decisions_emitted: u64,
    pub orders_emitted: u64,
    pub rejected_by_risk: u64,
}

impl V7Strategy {
    pub fn new(cfg: Arc<StrategyConfig>, risk: RiskConfig) -> Self {
        Self {
            cfg,
            risk,
            oracle: Arc::new(DummyPositionOracle::default()),
            rate: None,
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

    /// Oracle + (선택) rate tracker 주입.
    pub fn with_runtime(mut self, oracle: Arc<PositionOracleImpl>) -> Self {
        self.oracle = oracle;
        self
    }

    /// 다른 strategy 와 공유하는 account-wide rate tracker 등록.
    pub fn with_rate(mut self, rate: Arc<OrderRateTracker>) -> Self {
        self.rate = Some(rate);
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

    fn evaluate_symbol(&mut self, symbol: &str, orders: &mut Orders) -> Option<OrderDecision> {
        let cache = self.cache.get(symbol)?;
        let gate_bt = cache.gate_bt.as_ref()?;
        let gate_web_bt = cache.gate_web_bt.as_ref()?;

        let sig = calculate_signal(
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

        let extras = V7DecisionExtras {
            account_net_position_usdt: self.account_net_position_usdt,
        };
        let ts = self.trade_settings().clone();
        let dec = decide_order_v7(
            &sig,
            &ts,
            &extras,
            now_ms,
            gate_bt.server_time_ms,
            cache.binance_bt.as_ref().map(|b| b.server_time_ms),
            gate_web_bt.server_time_ms,
            0.0,
            exposure.this_symbol_usdt,
            ts.trade_size_trigger,
            exposure.symbol_risk_limit.max(ts.max_position_size) as i64 + 1,
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

        let api_side = if side.is_buy() {
            ApiSide::Buy
        } else {
            ApiSide::Sell
        };
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

        let symbol_ref = Symbol::new(symbol);
        orders.push((
            OrderRequest {
                exchange: ExchangeId::Gate,
                symbol: symbol_ref.clone(),
                side: api_side,
                order_type,
                qty,
                price,
                reduce_only: dec.level.is_close(),
                tif,
                client_seq: seq,
                origin_ts_ns: 0,
                client_id,
            },
            make_order_seed(seq, dec.level, self.tag()),
        ));
        self.orders_emitted = self.orders_emitted.saturating_add(1);
        if let Some(rt) = self.rate.as_ref() {
            rt.push(&symbol_ref, now_ms);
        }

        trace!(symbol, level = dec.level.as_str(), qty, "V7 order emitted");
        debug!(
            target: "strategy::v7",
            symbol,
            signals = self.signals_fired,
            decisions = self.decisions_emitted,
            orders = self.orders_emitted,
            "v7 eval metrics"
        );
        Some(dec)
    }
}

impl Strategy for V7Strategy {
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
        "V7"
    }

    fn tag(&self) -> &'static str {
        "v7"
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
            OrderResult(_)
            | WsPositionUpdate { .. }
            | WsBalanceUpdate { .. }
            | WsOrderUpdate { .. } => {}
        }
    }

    fn on_order_result(&mut self, info: &crate::OrderResultInfo) {
        counter_inc(CounterKey::OrderResultReceived);
        if matches!(info.status, crate::ResultStatus::Rejected) {
            counter_inc(CounterKey::OrderResultRejected);
        }
        tracing::info!(
            target: "strategy::v7",
            client_seq = info.client_seq,
            status = ?info.status,
            exchange_order_id = %info.exchange_order_id,
            text_tag = %info.text_tag,
            gateway_ts_ns = info.gateway_ts_ns,
            "v7 received order result"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hft_types::{BookTicker, Price, Size};

    fn strat() -> V7Strategy {
        let cfg = Arc::new(StrategyConfig::new(
            "test".into(),
            vec!["BTC_USDT".into()],
            TradeSettings::default(),
        ));
        V7Strategy::new(cfg, RiskConfig::default())
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
    fn v7_strategy_label() {
        let s = strat();
        assert_eq!(s.label(), "V7");
        assert_eq!(s.tag(), "v7");
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
}

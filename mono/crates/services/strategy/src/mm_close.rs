//! MM 잔여 포지션 정리 전략.
//!
//! 현재 oracle 은 심볼별 **순포지션**(`this_symbol_usdt`)만 노출하므로, 레거시 Python 의
//! "동일 심볼 gross long/short 동시 정리"는 그대로 복제할 수 없다. 대신 본 variant 는
//! 한 eval 에서 **여러 심볼의 long/short 잔여 포지션을 동시에 정리**하는 운영용 close
//! 전략으로 정의한다.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ahash::AHashMap;
use hft_exchange_api::{OrderRequest, OrderSide as ApiSide, OrderType};
use hft_strategy_config::StrategyConfig;
use hft_strategy_core::decision::{OrderLevel, OrderSide};
use hft_strategy_core::risk::{
    handle_chance, Chance, ExposureSnapshot, LastOrder, PositionOracle, RiskConfig,
    TimeRestrictionJitter,
};
use hft_strategy_runtime::{OrderRateTracker, PositionOracleImpl};
use hft_telemetry::{counter_inc, CounterKey};
use hft_time::{Clock, SystemClock};
use hft_types::{ExchangeId, MarketEvent, Symbol};
use parking_lot::RwLock;
use tracing::{debug, trace};

use crate::close_v1::{base_close_size, normalize_tif, tif_to_api};
use crate::{make_order_seed, Orders, Strategy};

#[derive(Debug, Clone, Copy, Default)]
struct QuoteState {
    bid: f64,
    ask: f64,
}

#[derive(Debug, Default)]
struct DummyPositionOracle {
    last_orders: RwLock<AHashMap<String, LastOrder>>,
}

impl PositionOracle for DummyPositionOracle {
    fn snapshot(&self, _symbol: &str) -> ExposureSnapshot {
        ExposureSnapshot {
            is_account_symbol: false,
            ..Default::default()
        }
    }

    fn record_last_order(&self, symbol: &str, order: LastOrder) {
        self.last_orders.write().insert(symbol.to_string(), order);
    }
}

/// MM 잔여 포지션 정리 전략.
pub struct MmCloseStrategy {
    cfg: Arc<StrategyConfig>,
    risk: RiskConfig,
    oracle: Arc<dyn PositionOracle + Send + Sync>,
    rate: Arc<OrderRateTracker>,
    quotes: AHashMap<String, QuoteState>,
    account_total_usdt: f64,
    account_unrealized_pnl_usdt: f64,
    client_seq: AtomicU64,
    last_close_attempt_ms: AHashMap<String, i64>,
    close_interval_ms: i64,
    pub orders_emitted: u64,
    pub rejected_by_risk: u64,
}

impl MmCloseStrategy {
    pub fn new(cfg: Arc<StrategyConfig>, risk: RiskConfig) -> Self {
        let close_interval_ms = cfg
            .trade_settings
            .same_side_price_time_restriction_ms_min
            .max(0);
        Self {
            cfg,
            risk,
            oracle: Arc::new(DummyPositionOracle::default()),
            rate: Arc::new(OrderRateTracker::new()),
            quotes: AHashMap::new(),
            account_total_usdt: 0.0,
            account_unrealized_pnl_usdt: 0.0,
            client_seq: AtomicU64::new(0),
            last_close_attempt_ms: AHashMap::new(),
            close_interval_ms,
            orders_emitted: 0,
            rejected_by_risk: 0,
        }
    }

    pub fn with_runtime(mut self, oracle: Arc<PositionOracleImpl>) -> Self {
        self.oracle = oracle;
        self
    }

    pub fn with_rate(mut self, rate: Arc<OrderRateTracker>) -> Self {
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

    fn ingest(&mut self, ev: &MarketEvent) {
        let quote = match ev {
            MarketEvent::BookTicker(bt) | MarketEvent::WebBookTicker(bt) => bt,
            MarketEvent::Trade(_) => return,
        };
        self.quotes.insert(
            quote.symbol.as_str().to_string(),
            QuoteState {
                bid: quote.bid_price.0,
                ask: quote.ask_price.0,
            },
        );
    }

    /// 순포지션 크기에 맞춰 tapering 된 close size 를 계산한다.
    fn tapered_close_size(&self, exposure: &ExposureSnapshot) -> i64 {
        let base = base_close_size(&self.cfg.trade_settings);
        let residual = exposure.this_symbol_usdt.abs().ceil() as i64;
        let min_size = exposure.min_order_size.max(1);
        if residual <= 0 {
            min_size
        } else {
            residual.min(base).max(min_size)
        }
    }

    fn evaluate_symbol(&mut self, symbol: &str, now_ms: i64, orders: &mut Orders) {
        if let Some(last_ms) = self.last_close_attempt_ms.get(symbol).copied() {
            if now_ms.saturating_sub(last_ms) < self.close_interval_ms {
                return;
            }
        }

        let exposure = self.oracle.snapshot(symbol);
        if !exposure.is_account_symbol || exposure.this_symbol_usdt.abs() < f64::EPSILON {
            return;
        }

        let quote = match self.quotes.get(symbol).copied() {
            Some(q) if q.bid > 0.0 && q.ask > 0.0 => q,
            _ => return,
        };

        let (side, price) = if exposure.this_symbol_usdt > 0.0 {
            (OrderSide::Sell, quote.bid)
        } else {
            (OrderSide::Buy, quote.ask)
        };
        if !price.is_finite() || price <= 0.0 {
            return;
        }

        let mut warmed = exposure;
        if warmed.last_order.is_none() {
            warmed.last_order = Some(LastOrder {
                level: OrderLevel::LimitClose,
                side,
                price,
                timestamp_ms: 0,
            });
        }

        let tif = normalize_tif(&self.cfg.trade_settings.limit_close_tif);
        let chance = Chance {
            symbol: symbol.to_string(),
            side,
            price,
            tif,
            level: OrderLevel::LimitClose,
            size: self.tapered_close_size(&warmed),
            bypass_time_restriction: false,
            bypass_safe_limit_close: false,
            bypass_max_position_size: false,
        };
        let jitter = TimeRestrictionJitter::midpoint(&self.cfg.trade_settings);
        let rc = match handle_chance(
            self.oracle.as_ref(),
            &chance,
            &self.cfg.trade_settings,
            &self.risk,
            self.account_total_usdt,
            self.account_unrealized_pnl_usdt,
            now_ms,
            jitter,
            &warmed,
        ) {
            Some(rc) => rc,
            None => {
                self.rejected_by_risk = self.rejected_by_risk.saturating_add(1);
                return;
            }
        };

        let seq = self.client_seq.fetch_add(1, Ordering::Relaxed);
        let symbol_ref = Symbol::new(symbol);
        orders.push((
            OrderRequest {
                exchange: ExchangeId::Gate,
                symbol: symbol_ref.clone(),
                side: if side.is_buy() {
                    ApiSide::Buy
                } else {
                    ApiSide::Sell
                },
                order_type: OrderType::Limit,
                qty: rc.order_size.unsigned_abs() as f64,
                price: Some(price),
                reduce_only: true,
                tif: tif_to_api(tif),
                client_seq: seq,
                origin_ts_ns: 0,
                client_id: Arc::from(format!("{}-{seq}", self.tag())),
            },
            make_order_seed(seq, OrderLevel::LimitClose, self.tag()),
        ));
        self.orders_emitted = self.orders_emitted.saturating_add(1);
        self.last_close_attempt_ms
            .insert(symbol.to_string(), now_ms);
        self.rate.push(&symbol_ref, now_ms);

        trace!(symbol, qty = rc.order_size, price, "mm_close order emitted");
        debug!(
            target: "strategy::mm_close",
            symbol,
            orders = self.orders_emitted,
            rejected = self.rejected_by_risk,
            "mm_close eval metrics"
        );
    }
}

impl Strategy for MmCloseStrategy {
    fn update(&mut self, ev: &MarketEvent) {
        self.ingest(ev);
    }

    fn eval(&mut self, ev: &MarketEvent) -> Orders {
        if !self.cfg.is_strategy_symbol(ev.symbol().as_str()) {
            return Orders::new();
        }
        let clock = SystemClock::new();
        let now_ms = clock.now_ms();
        let mut out = Orders::new();
        let symbols = self.cfg.symbols.clone();
        for symbol in symbols {
            self.evaluate_symbol(&symbol, now_ms, &mut out);
        }
        out
    }

    fn label(&self) -> &str {
        "MmClose"
    }

    fn tag(&self) -> &'static str {
        "mm_close"
    }

    fn on_control(&mut self, ctrl: &crate::StrategyControl) {
        use crate::StrategyControl::*;

        match *ctrl {
            SetAccountBalance {
                total_usdt,
                unrealized_pnl_usdt,
            } => self.set_account_balance(total_usdt, unrealized_pnl_usdt),
            SetAccountNetPosition { .. } => {}
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
            target: "strategy::mm_close",
            client_seq = info.client_seq,
            status = ?info.status,
            exchange_order_id = %info.exchange_order_id,
            text_tag = %info.text_tag,
            gateway_ts_ns = info.gateway_ts_ns,
            "mm_close received order result"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hft_strategy_config::TradeSettings;
    use hft_strategy_runtime::{
        AccountMembership, ContractMeta, LastOrderStore, PositionCache, PositionOracleImpl,
        PositionSnapshot, SymbolMetaCache, SymbolPosition,
    };
    use hft_types::{BookTicker, Price, Size};

    fn build_oracle(items: &[(&str, f64, i64)]) -> Arc<PositionOracleImpl> {
        let meta = Arc::new(SymbolMetaCache::seeded(items.iter().map(|(sym, _, _)| {
            (
                Symbol::new(*sym),
                ContractMeta {
                    min_order_size: 1,
                    risk_limit: 1_000_000.0,
                    ..Default::default()
                },
            )
        })));

        let mut by_symbol = AHashMap::new();
        let mut total_long = 0.0;
        let mut total_short = 0.0;
        for (sym, notional, update_time_sec) in items {
            by_symbol.insert(
                Symbol::new(*sym),
                SymbolPosition {
                    notional_usdt: *notional,
                    update_time_sec: *update_time_sec,
                },
            );
            if *notional > 0.0 {
                total_long += *notional;
            } else {
                total_short += notional.abs();
            }
        }

        let positions = Arc::new(PositionCache::with_snapshot(PositionSnapshot {
            total_long_usdt: total_long,
            total_short_usdt: total_short,
            by_symbol,
            taken_at_ms: 0,
        }));
        let last_orders = Arc::new(LastOrderStore::new());
        for (sym, notional, _) in items {
            let side = if *notional >= 0.0 {
                OrderSide::Buy
            } else {
                OrderSide::Sell
            };
            last_orders.record(
                *sym,
                LastOrder {
                    level: OrderLevel::LimitOpen,
                    side,
                    price: 100.0,
                    timestamp_ms: 0,
                },
            );
        }

        Arc::new(PositionOracleImpl::new(
            meta,
            positions,
            last_orders,
            AccountMembership::fixed(items.iter().map(|(sym, _, _)| sym.to_string())),
        ))
    }

    fn quote(symbol: &str) -> MarketEvent {
        let clock = SystemClock::new();
        let now_ms = clock.now_ms();
        MarketEvent::BookTicker(BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new(symbol),
            bid_price: Price(100.0),
            ask_price: Price(101.0),
            bid_size: Size(1.0),
            ask_size: Size(1.0),
            event_time_ms: now_ms,
            server_time_ms: now_ms,
        })
    }

    #[test]
    fn mm_close_taper_dust() {
        let cfg = Arc::new(StrategyConfig::new(
            "test".into(),
            vec!["BTC_USDT".into()],
            TradeSettings {
                order_size: 5.0,
                close_order_size: Some(5.0),
                max_position_size: 1_000_000.0,
                ..Default::default()
            },
        ));
        let oracle = build_oracle(&[("BTC_USDT", 2.0, 0)]);
        let mut strat = MmCloseStrategy::new(cfg, RiskConfig::default())
            .with_runtime(oracle)
            .with_rate(Arc::new(OrderRateTracker::new()));
        strat.set_account_balance(1_000_000.0, 0.0);
        let ev = quote("BTC_USDT");
        strat.update(&ev);
        let out = strat.eval(&ev);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.side, ApiSide::Sell);
        assert_eq!(out[0].0.qty, 2.0);
    }

    #[test]
    fn mm_close_both_directions() {
        let cfg = Arc::new(StrategyConfig::new(
            "test".into(),
            vec!["BTC_USDT".into(), "ETH_USDT".into()],
            TradeSettings {
                order_size: 5.0,
                close_order_size: Some(5.0),
                max_position_size: 1_000_000.0,
                ..Default::default()
            },
        ));
        let oracle = build_oracle(&[("BTC_USDT", 10.0, 0), ("ETH_USDT", -8.0, 0)]);
        let mut strat = MmCloseStrategy::new(cfg, RiskConfig::default())
            .with_runtime(oracle)
            .with_rate(Arc::new(OrderRateTracker::new()));
        strat.set_account_balance(1_000_000.0, 0.0);

        let eth = quote("ETH_USDT");
        let btc = quote("BTC_USDT");
        strat.update(&eth);
        strat.update(&btc);
        let out = strat.eval(&btc);

        assert_eq!(out.len(), 2);
        assert!(out
            .iter()
            .any(|o| o.0.symbol.as_str() == "BTC_USDT" && o.0.side == ApiSide::Sell));
        assert!(out
            .iter()
            .any(|o| o.0.symbol.as_str() == "ETH_USDT" && o.0.side == ApiSide::Buy));
    }
}

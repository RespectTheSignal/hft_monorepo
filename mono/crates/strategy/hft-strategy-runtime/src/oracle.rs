//! `PositionOracleImpl` — [`hft_strategy_core::risk::PositionOracle`] 을
//! 실 캐시들로 구성한 구현체.
//!
//! 구성:
//! - 심볼 메타 → [`SymbolMetaCache`] (risk_limit, min_order_size)
//! - 포지션 → [`PositionCache`] (long/short/심볼별)
//! - 최근 주문 → [`LastOrderStore`]
//! - 계정 심볼 판단 → [`AccountMembership`] (함수형 판별)
//!
//! 모든 필드는 `Arc<T>`. strategy hot path 에서 `self.oracle.as_ref().snapshot(sym)`
//! 으로 호출되며 내부는 3~4 번의 락-프리 lookup 이 전부.

use std::sync::Arc;

use hft_strategy_core::risk::{ExposureSnapshot, LastOrder, PositionOracle};
use hft_types::Symbol;

use crate::last_orders::LastOrderStore;
use crate::positions::PositionCache;
use crate::symbol_meta::SymbolMetaCache;

/// `symbol` → account_symbol 여부 판정자.
///
/// 레거시는 `GateOrderManager::account_symbols: HashSet<String>` 를 직접 읽었다.
/// 본 crate 는 함수 closure 로 받아 테스트에서 간단히 substitute 가능하게 한다.
pub struct AccountMembership(Arc<dyn Fn(&str) -> bool + Send + Sync>);

impl AccountMembership {
    /// 콜백에서 `bool` 을 계산. `Arc<dyn Fn>` 으로 감싸 공유.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        Self(Arc::new(f))
    }

    /// 고정 셋 (HashSet-backed). 가장 흔한 구성.
    pub fn fixed<S: AsRef<str> + Send + Sync + 'static>(
        symbols: impl IntoIterator<Item = S>,
    ) -> Self {
        use ahash::AHashSet;
        let set: AHashSet<String> = symbols.into_iter().map(|s| s.as_ref().to_string()).collect();
        Self::new(move |sym| set.contains(sym))
    }

    /// 항상 false (실주문 차단).
    pub fn none() -> Self {
        Self::new(|_| false)
    }

    /// 테스트 편의.
    #[inline]
    pub fn contains(&self, symbol: &str) -> bool {
        (self.0)(symbol)
    }
}

impl std::fmt::Debug for AccountMembership {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountMembership").finish_non_exhaustive()
    }
}

/// 실 `PositionOracle` 구현체. 모든 필드 `Arc` — 수많은 strategy 스레드에서 공유.
#[derive(Debug, Clone)]
pub struct PositionOracleImpl {
    meta: Arc<SymbolMetaCache>,
    positions: Arc<PositionCache>,
    last_orders: Arc<LastOrderStore>,
    membership: Arc<AccountMembership>,
}

impl PositionOracleImpl {
    /// 기본 생성자 — 모든 하위 캐시를 받아 조립.
    pub fn new(
        meta: Arc<SymbolMetaCache>,
        positions: Arc<PositionCache>,
        last_orders: Arc<LastOrderStore>,
        membership: AccountMembership,
    ) -> Self {
        Self {
            meta,
            positions,
            last_orders,
            membership: Arc::new(membership),
        }
    }

    /// 테스트 / 단독 사용용 — 모든 캐시를 기본값으로 만들고 membership 만 주입.
    pub fn from_membership(membership: AccountMembership) -> Self {
        Self::new(
            Arc::new(SymbolMetaCache::new()),
            Arc::new(PositionCache::new()),
            Arc::new(LastOrderStore::new()),
            membership,
        )
    }

    /// 내부 meta 캐시 참조 (feed / refresh 용).
    pub fn meta(&self) -> Arc<SymbolMetaCache> {
        self.meta.clone()
    }
    /// 내부 포지션 캐시 참조.
    pub fn positions(&self) -> Arc<PositionCache> {
        self.positions.clone()
    }
    /// 내부 최근 주문 store 참조.
    pub fn last_orders(&self) -> Arc<LastOrderStore> {
        self.last_orders.clone()
    }
}

impl PositionOracle for PositionOracleImpl {
    fn snapshot(&self, symbol: &str) -> ExposureSnapshot {
        let sym = Symbol::new(symbol);
        let pos = self.positions.snapshot();
        let meta = self.meta.get(&sym);
        let last = self.last_orders.get(symbol);
        let sym_pos = pos.symbol_position(&sym);
        ExposureSnapshot {
            total_long_usdt: pos.total_long_usdt,
            total_short_usdt: pos.total_short_usdt,
            this_symbol_usdt: sym_pos.map(|p| p.notional_usdt).unwrap_or(0.0),
            is_account_symbol: self.membership.contains(symbol),
            symbol_risk_limit: meta.as_ref().map(|m| m.risk_limit).unwrap_or(0.0),
            min_order_size: meta.as_ref().map(|m| m.min_order_size).unwrap_or(0),
            last_order: last,
            position_update_time_sec: sym_pos.map(|p| p.update_time_sec).unwrap_or(0),
            quanto_multiplier: meta.as_ref().map(|m| m.quanto_multiplier).unwrap_or(1.0),
        }
    }

    fn record_last_order(&self, symbol: &str, order: LastOrder) {
        self.last_orders.record(symbol, order);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::positions::{PositionSnapshot, SymbolPosition};
    use crate::symbol_meta::ContractMeta;
    use ahash::AHashMap;
    use hft_strategy_core::decision::{OrderLevel, OrderSide};

    fn build_oracle() -> PositionOracleImpl {
        let meta = Arc::new(SymbolMetaCache::seeded([(
            Symbol::new("BTC_USDT"),
            ContractMeta {
                risk_limit: 100_000.0,
                min_order_size: 2,
                ..Default::default()
            },
        )]));
        let mut by_sym = AHashMap::new();
        by_sym.insert(
            Symbol::new("BTC_USDT"),
            SymbolPosition {
                notional_usdt: 500.0,
                update_time_sec: 0,
            },
        );
        let pos = Arc::new(PositionCache::with_snapshot(PositionSnapshot {
            total_long_usdt: 1_000.0,
            total_short_usdt: 200.0,
            by_symbol: by_sym,
            taken_at_ms: 1,
        }));
        let last = Arc::new(LastOrderStore::new());
        let membership = AccountMembership::fixed(["BTC_USDT"]);
        PositionOracleImpl::new(meta, pos, last, membership)
    }

    #[test]
    fn snapshot_pulls_from_all_caches() {
        let o = build_oracle();
        let s = o.snapshot("BTC_USDT");
        assert!(s.is_account_symbol);
        assert_eq!(s.symbol_risk_limit, 100_000.0);
        assert_eq!(s.min_order_size, 2);
        assert_eq!(s.this_symbol_usdt, 500.0);
        assert_eq!(s.total_long_usdt, 1_000.0);
        assert_eq!(s.total_short_usdt, 200.0);
        assert!(s.last_order.is_none());
    }

    #[test]
    fn snapshot_on_unknown_symbol_returns_defaults() {
        let o = build_oracle();
        let s = o.snapshot("ETH_USDT");
        assert!(!s.is_account_symbol);
        assert_eq!(s.symbol_risk_limit, 0.0);
        assert_eq!(s.this_symbol_usdt, 0.0);
    }

    #[test]
    fn record_last_order_visible_in_next_snapshot() {
        let o = build_oracle();
        o.record_last_order(
            "BTC_USDT",
            LastOrder {
                level: OrderLevel::LimitOpen,
                side: OrderSide::Buy,
                price: 50_000.0,
                timestamp_ms: 123,
            },
        );
        let s = o.snapshot("BTC_USDT");
        let l = s.last_order.expect("last_order must surface");
        assert_eq!(l.price, 50_000.0);
        assert_eq!(l.timestamp_ms, 123);
    }

    #[test]
    fn membership_none_blocks_accounts() {
        let mut o = build_oracle();
        o.membership = Arc::new(AccountMembership::none());
        assert!(!o.snapshot("BTC_USDT").is_account_symbol);
    }
}

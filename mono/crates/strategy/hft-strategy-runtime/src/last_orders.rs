//! 심볼별 가장 최근 주문 기록.
//!
//! 레거시 `GateOrderManager::last_orders: RwLock<HashMap<String, LastOrder>>` 를
//! [`DashMap`] 기반으로 바꿔 경합 구간을 잘게 쪼갠다. record/read 모두 O(1) 평균.

use dashmap::DashMap;
use hft_strategy_core::risk::LastOrder;

/// 심볼 (String) → LastOrder.
///
/// 심볼은 해시 키로만 쓰므로 String 으로 저장 (Symbol 의 Arc<str> 복사 비용을
/// 줄이고 싶다면 향후 `Symbol` 로 교체 가능; 본 v1 은 legacy wire 와 동일하게 String).
#[derive(Debug, Default)]
pub struct LastOrderStore {
    inner: DashMap<String, LastOrder, ahash::RandomState>,
}

impl LastOrderStore {
    /// 빈 store.
    pub fn new() -> Self {
        Self {
            inner: DashMap::with_hasher(ahash::RandomState::new()),
        }
    }

    /// 기록 — 같은 심볼 재주문 시 overwrite. `level` 을 포함해 OpenClose 구분.
    pub fn record(&self, symbol: impl Into<String>, order: LastOrder) {
        self.inner.insert(symbol.into(), order);
    }

    /// 읽기 — `LastOrder` copy (Copy 가 아닌 경우 Clone) 로 dashmap guard 를
    /// 빠르게 반환. LastOrder 는 작은 POD 이므로 clone 은 저렴.
    pub fn get(&self, symbol: &str) -> Option<LastOrder> {
        self.inner.get(symbol).map(|r| r.value().clone())
    }

    /// 엔트리 수.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// 비어있는지.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// 테스트/리셋 — 전체 clear.
    pub fn clear(&self) {
        self.inner.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hft_strategy_core::decision::{OrderLevel, OrderSide};

    #[test]
    fn record_then_get_roundtrip() {
        let s = LastOrderStore::new();
        let o = LastOrder {
            level: OrderLevel::LimitOpen,
            side: OrderSide::Buy,
            price: 10.0,
            timestamp_ms: 100,
        };
        s.record("BTC_USDT", o.clone());
        let got = s.get("BTC_USDT").unwrap();
        assert_eq!(got.level, OrderLevel::LimitOpen);
        assert_eq!(got.side, OrderSide::Buy);
        assert!((got.price - 10.0).abs() < f64::EPSILON);
        assert_eq!(got.timestamp_ms, 100);
    }

    #[test]
    fn overwrite_latest_wins() {
        let s = LastOrderStore::new();
        for i in 0..10 {
            s.record(
                "X",
                LastOrder {
                    level: OrderLevel::LimitOpen,
                    side: OrderSide::Buy,
                    price: i as f64,
                    timestamp_ms: i,
                },
            );
        }
        assert_eq!(s.get("X").unwrap().timestamp_ms, 9);
    }

    #[test]
    fn unknown_symbol_is_none() {
        let s = LastOrderStore::new();
        assert!(s.get("does-not-exist").is_none());
    }
}

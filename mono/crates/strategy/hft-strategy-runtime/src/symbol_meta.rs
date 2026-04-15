//! 심볼 메타데이터 캐시.
//!
//! 레거시 `GateOrderManager` 의 `symbol_info: HashMap<String, ContractInfo>` 를
//! DashMap + `ArcSwap<…>` 조합으로 풀어냈다. 실 백엔드는 `SymbolMetaProvider`
//! trait 을 구현해 주입한다 (Gate REST, Binance exchangeInfo, …).
//!
//! # 성능
//! - 읽기 경로는 DashMap::get 한번 + `Arc<ContractMeta>` clone(포인터 증분) 만 —
//!   strategy hot path 에서 락 경합 없음.
//! - 기록 경로는 refresh 가 주기적으로 일괄 업데이트. refresh 는 async.

use std::sync::Arc;

use dashmap::DashMap;
use hft_types::Symbol;

/// 거래 가능 심볼 1개의 정적 메타.
///
/// 레거시의 `ContractInfo` 대비 — 이름을 통일하고 f64 로 고정 (레거시는
/// 문자열 필드가 섞여있어 hot path 마다 parse 했음).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContractMeta {
    /// 가격 호가 단위. `0.1` → 소수 1자리 반올림.
    pub order_price_round: f64,
    /// 수량 호가 단위 (contract).
    pub order_size_round: f64,
    /// quanto multiplier — 계약 1개가 표시하는 기초자산 수량 (USDT perp 기준 1.0).
    pub quanto_multiplier: f64,
    /// 심볼별 리스크 한도 (USDT notional).
    pub risk_limit: f64,
    /// 최소 주문 수량 (contract).
    pub min_order_size: i64,
}

impl Default for ContractMeta {
    fn default() -> Self {
        Self {
            order_price_round: 0.0001,
            order_size_round: 1.0,
            quanto_multiplier: 1.0,
            risk_limit: 0.0,
            min_order_size: 1,
        }
    }
}

/// 심볼 메타 백엔드.
///
/// 실 구현체 예:
/// - `GateSymbolMetaFetcher` — `GET /api/v4/futures/usdt/contracts` 폴링.
/// - `BinanceSymbolMetaFetcher` — `GET /fapi/v1/exchangeInfo`.
///
/// 구현체는 1회 호출로 전체 심볼 목록을 돌려준다 (차분 동기화는 상위 주기 호출로).
#[async_trait::async_trait]
pub trait SymbolMetaProvider: Send + Sync {
    /// (symbol, meta) 목록을 반환. 실패 시 `Err` — 상위가 재시도 판단.
    async fn fetch_all(&self) -> anyhow::Result<Vec<(Symbol, ContractMeta)>>;
}

/// DashMap 기반 in-mem 캐시. `Arc<SymbolMetaCache>` 를 다수 소비자가 공유.
#[derive(Debug, Default)]
pub struct SymbolMetaCache {
    inner: DashMap<Symbol, Arc<ContractMeta>, ahash::RandomState>,
}

impl SymbolMetaCache {
    /// 빈 캐시.
    pub fn new() -> Self {
        Self {
            inner: DashMap::with_hasher(ahash::RandomState::new()),
        }
    }

    /// 테스트 생성자: 고정 엔트리로 seed.
    pub fn seeded(items: impl IntoIterator<Item = (Symbol, ContractMeta)>) -> Self {
        let c = Self::new();
        for (s, m) in items {
            c.upsert(s, m);
        }
        c
    }

    /// 특정 심볼 lookup. `Arc<ContractMeta>` 를 돌려주므로 비용은 포인터 증분.
    #[inline]
    pub fn get(&self, symbol: &Symbol) -> Option<Arc<ContractMeta>> {
        self.inner.get(symbol).map(|r| r.value().clone())
    }

    /// 한 엔트리 upsert.
    pub fn upsert(&self, symbol: Symbol, meta: ContractMeta) {
        self.inner.insert(symbol, Arc::new(meta));
    }

    /// 엔트리 수.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// 비어있는지.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// provider 로 전체 refresh. 성공 시 기존 엔트리는 유지 + 새 엔트리로 덮어쓰기.
    /// 기존에만 있고 신규에 없던 심볼은 **유지** (거래 정지 단계에서도 last meta 참조).
    pub async fn refresh<P: SymbolMetaProvider + ?Sized>(
        &self,
        provider: &P,
    ) -> anyhow::Result<usize> {
        let items = provider.fetch_all().await?;
        let n = items.len();
        for (s, m) in items {
            self.upsert(s, m);
        }
        tracing::info!(loaded = n, total = self.len(), "SymbolMetaCache refreshed");
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedProvider(Vec<(Symbol, ContractMeta)>);
    #[async_trait::async_trait]
    impl SymbolMetaProvider for FixedProvider {
        async fn fetch_all(&self) -> anyhow::Result<Vec<(Symbol, ContractMeta)>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn seeded_cache_is_readable() {
        let c = SymbolMetaCache::seeded([(
            Symbol::new("BTC_USDT"),
            ContractMeta {
                min_order_size: 1,
                risk_limit: 1_000_000.0,
                ..Default::default()
            },
        )]);
        let m = c.get(&Symbol::new("BTC_USDT")).unwrap();
        assert_eq!(m.min_order_size, 1);
        assert!(c.get(&Symbol::new("ETH_USDT")).is_none());
    }

    #[tokio::test]
    async fn refresh_populates_and_merges() {
        let c = SymbolMetaCache::new();
        let p1 = FixedProvider(vec![(
            Symbol::new("A"),
            ContractMeta {
                min_order_size: 1,
                ..Default::default()
            },
        )]);
        let p2 = FixedProvider(vec![(
            Symbol::new("B"),
            ContractMeta {
                min_order_size: 2,
                ..Default::default()
            },
        )]);
        assert_eq!(c.refresh(&p1).await.unwrap(), 1);
        assert_eq!(c.refresh(&p2).await.unwrap(), 1);
        // A 는 유지, B 추가 — 사이즈 2.
        assert_eq!(c.len(), 2);
        assert!(c.get(&Symbol::new("A")).is_some());
        assert!(c.get(&Symbol::new("B")).is_some());
    }

    #[tokio::test]
    async fn refresh_updates_existing_entry() {
        let c = SymbolMetaCache::seeded([(
            Symbol::new("A"),
            ContractMeta {
                min_order_size: 1,
                ..Default::default()
            },
        )]);
        let p = FixedProvider(vec![(
            Symbol::new("A"),
            ContractMeta {
                min_order_size: 42,
                ..Default::default()
            },
        )]);
        c.refresh(&p).await.unwrap();
        assert_eq!(c.get(&Symbol::new("A")).unwrap().min_order_size, 42);
    }
}

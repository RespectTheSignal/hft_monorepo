//! 계정 포지션 캐시.
//!
//! 계정 전체의 long/short USDT notional 과 심볼별 노출을 모아 둔다. 레거시
//! `GateOrderManager::total_long_usdt / total_short_usdt / this_symbol_usdt` 를
//! 단일 스냅샷 구조체로 추려내고, 업데이트는 백엔드가 주기적으로 push.
//!
//! # 왜 ArcSwap?
//! - strategy hot path 가 매 이벤트마다 snapshot 을 읽는다 (락 잡으면 안 됨).
//! - 업데이트는 REST 폴러가 1~5Hz 로 수행 — 읽기 ≫ 쓰기.
//! - `ArcSwap<PositionSnapshot>` 은 load 시 포인터 로드 1회 뿐, lock-free.

use std::sync::Arc;

use ahash::AHashMap;
use arc_swap::ArcSwap;
use hft_types::Symbol;

/// 심볼별 세부 포지션 상태. close_stale 판단에 `update_time_sec` 이 필요하므로
/// 단순 f64 에서 struct 로 확장. signed notional + 갱신 시각.
#[derive(Debug, Clone, Copy, Default)]
pub struct SymbolPosition {
    /// Signed USDT notional (long +, short -).
    pub notional_usdt: f64,
    /// 거래소가 report 한 position update timestamp (seconds since epoch).
    /// 0 은 "값 없음" — close_stale 판단에서 skip.
    pub update_time_sec: i64,
}

/// 현 시점 포지션 상태 스냅샷. 교체 단위 = 전체 덩어리.
#[derive(Debug, Clone, Default)]
pub struct PositionSnapshot {
    /// 계정 전체 long notional (USDT).
    pub total_long_usdt: f64,
    /// 계정 전체 short notional (USDT).
    pub total_short_usdt: f64,
    /// 심볼별 포지션 상세. 값이 없는 심볼 = 포지션 없음.
    pub by_symbol: AHashMap<Symbol, SymbolPosition>,
    /// 이 스냅샷의 생성 시각 (ms since epoch, 캘리브레이션/디버깅용).
    pub taken_at_ms: i64,
}

impl PositionSnapshot {
    /// 심볼별 signed notional 조회. 없으면 0.
    #[inline]
    pub fn symbol_notional(&self, symbol: &Symbol) -> f64 {
        self.by_symbol
            .get(symbol)
            .map(|p| p.notional_usdt)
            .unwrap_or(0.0)
    }

    /// 심볼별 update time (seconds). 없으면 0.
    #[inline]
    pub fn symbol_update_time_sec(&self, symbol: &Symbol) -> i64 {
        self.by_symbol
            .get(symbol)
            .map(|p| p.update_time_sec)
            .unwrap_or(0)
    }

    /// 전체 SymbolPosition 조회.
    #[inline]
    pub fn symbol_position(&self, symbol: &Symbol) -> Option<SymbolPosition> {
        self.by_symbol.get(symbol).copied()
    }
}

/// 포지션 백엔드 — Gate REST `GET /positions/{symbol}` 폴링, 또는 SHM 공유.
///
/// 구현체는 단일 스냅샷을 만들어 돌려준다. 실패 시 `Err` — 호출자는 직전 캐시를
/// 유지한다 (PositionCache 는 실패를 ingest 하지 않음).
#[async_trait::async_trait]
pub trait PositionProvider: Send + Sync {
    /// 현재 포지션을 한번에 읽어낸 스냅샷.
    async fn fetch(&self) -> anyhow::Result<PositionSnapshot>;
}

/// 포지션 캐시. 내부는 `ArcSwap<PositionSnapshot>` 단일 슬롯.
#[derive(Debug)]
pub struct PositionCache {
    slot: ArcSwap<PositionSnapshot>,
}

impl PositionCache {
    /// 비어있는 스냅샷으로 초기화.
    pub fn new() -> Self {
        Self {
            slot: ArcSwap::from_pointee(PositionSnapshot::default()),
        }
    }

    /// 명시적 스냅샷으로 초기화 (테스트/재시작 복구).
    pub fn with_snapshot(s: PositionSnapshot) -> Self {
        Self {
            slot: ArcSwap::from_pointee(s),
        }
    }

    /// 가장 최근 스냅샷의 `Arc` — 읽기 경로 (lock-free).
    #[inline]
    pub fn snapshot(&self) -> Arc<PositionSnapshot> {
        self.slot.load_full()
    }

    /// 업데이트 — 구조체 전체 교체.
    pub fn store(&self, s: PositionSnapshot) {
        self.slot.store(Arc::new(s));
    }

    /// provider 를 호출해 성공 시 교체, 실패 시 기존 유지 후 Err.
    pub async fn refresh<P: PositionProvider + ?Sized>(&self, provider: &P) -> anyhow::Result<()> {
        let s = provider.fetch().await?;
        self.store(s);
        Ok(())
    }
}

impl Default for PositionCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct P(PositionSnapshot);
    #[async_trait::async_trait]
    impl PositionProvider for P {
        async fn fetch(&self) -> anyhow::Result<PositionSnapshot> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn refresh_replaces_snapshot() {
        let c = PositionCache::new();
        assert_eq!(c.snapshot().total_long_usdt, 0.0);
        let mut m = AHashMap::new();
        m.insert(
            Symbol::new("BTC_USDT"),
            SymbolPosition {
                notional_usdt: 1234.0,
                update_time_sec: 100,
            },
        );
        let s = PositionSnapshot {
            total_long_usdt: 10_000.0,
            total_short_usdt: 5_000.0,
            by_symbol: m,
            taken_at_ms: 42,
        };
        let p = P(s.clone());
        c.refresh(&p).await.unwrap();
        let got = c.snapshot();
        assert_eq!(got.total_long_usdt, 10_000.0);
        assert_eq!(got.symbol_notional(&Symbol::new("BTC_USDT")), 1234.0);
        assert_eq!(got.symbol_update_time_sec(&Symbol::new("BTC_USDT")), 100);
        assert_eq!(got.symbol_notional(&Symbol::new("ETH_USDT")), 0.0);
        assert_eq!(got.symbol_update_time_sec(&Symbol::new("ETH_USDT")), 0);
    }
}

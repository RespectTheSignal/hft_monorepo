//! `hft-strategy-runtime` — 실 주문을 위한 [`PositionOracle`] 구현과 그에 필요한
//! 계정/심볼 메타 캐시를 제공한다.
//!
//! # 왜 별도 crate 인가
//! 레거시 `GateOrderManager` (2705L) 는 REST/WebSocket/캐시/리스크 로직이 한 덩어리로
//! 섞여 있어 서브시스템별로 다시 쪼개야 **테스트 가능성** 과 **거래소별 재사용** 이
//! 살아난다. 이 crate 는 다음 경계로 분리한다:
//!
//! - [`symbol_meta::SymbolMetaCache`] — 심볼 메타(risk_limit, min_order_size, 라운드 등)
//!   캐시. 백엔드는 콜백 trait (`SymbolMetaProvider`) 로 추상화해 Gate/Binance/…
//!   별 REST 폴링을 각자 공급.
//! - [`positions::PositionCache`] — 계정별 (계정심볼, long/short/가격) 스냅샷. 역시
//!   `PositionProvider` trait 으로 백엔드 분리.
//! - [`last_orders::LastOrderStore`] — 심볼별 최근 주문 기록. 동시성 dashmap.
//! - [`oracle::PositionOracleImpl`] — 위 3 구성요소 + `is_account_symbol` 판단을
//!   조합해 [`hft_strategy_core::risk::PositionOracle`] 을 구현.
//!
//! # 현 단계 (Phase 2 Track A 후속, 2026-04-15)
//! Gate/Binance 의 실제 REST 폴러는 아직 없다. 본 crate 는 **trait 경계 + 인메모리
//! 구현 + 단위 테스트** 까지만 공급한다. 실 폴러는 후속 PR 에서 각 exchange client
//! crate 로 주입. V8 전략은 `PositionOracleImpl::new(providers)` 로 즉시 작동.
//!
//! # Thread safety
//! 내부 자료구조는 모두 `Arc<T>` + `DashMap` 또는 `ArcSwap` 기반으로 락 경합 최소화.
//! 모든 public 타입은 `Send + Sync + Clone`.

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

pub mod last_orders;
pub mod oracle;
pub mod order_rate;
pub mod positions;
pub mod symbol_meta;

pub use last_orders::LastOrderStore;
pub use oracle::{AccountMembership, PositionOracleImpl};
pub use order_rate::OrderRateTracker;
pub use positions::{PositionCache, PositionProvider, PositionSnapshot, SymbolPosition};
pub use symbol_meta::{ContractMeta, SymbolMetaCache, SymbolMetaProvider};

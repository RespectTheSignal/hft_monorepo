//! hft-account — 복수 서브계정 관리, 마진 모니터링, 계정 유틸리티.
//!
//! # 설계
//! - [`manager::AccountManager`] : login_name → Credentials 매핑 레지스트리.
//! - [`margin::MarginManager`] : 모든 등록 계정의 잔고/포지션 스냅샷 수집 (읽기 전용).
//! - [`login_expand::expand_logins`] : `sigma001~sigma010` 형식 range expression 지원.
//! - [`ip_detect::detect_local_ip`] : 로컬 호스트 egress IP 감지.

#![deny(rust_2018_idioms)]

pub mod accounts;
pub mod ip_detect;
pub mod keys;
pub mod login_expand;
pub mod manager;
pub mod margin;

pub use accounts::{AccountEntry, AccountsConfig};
pub use login_expand::expand_logins;
pub use manager::AccountManager;
pub use margin::{AccountMarginSnapshot, MarginManager, MarginSummary};

//! Redis key naming 규칙.
//!
//! prefix = `{REDIS_STATE_PREFIX}:{login_name}:`
//! 기본 prefix = `gate_hft:state:{login_name}:`
//!
//! Keys:
//! - `{prefix}last_order:{symbol}` — `LastOrder` JSON, TTL 3600s
//! - `{prefix}session` — `StrategySession` JSON, TTL 86400s
//! - `{prefix}session:heartbeat_ms` — epoch ms, TTL 60s

/// 기본 Redis state prefix.
pub const DEFAULT_STATE_PREFIX: &str = "gate_hft:state";
/// 심볼별 최근 주문 key prefix.
pub const LAST_ORDER_PREFIX: &str = "last_order:";
/// 세션 메타데이터 key suffix.
pub const SESSION_KEY: &str = "session";
/// 세션 heartbeat key suffix.
pub const SESSION_HEARTBEAT_KEY: &str = "session:heartbeat_ms";
/// 최근 주문 TTL.
pub const LAST_ORDER_TTL_SECS: u64 = 3_600;
/// 세션 메타데이터 TTL.
pub const SESSION_TTL_SECS: u64 = 86_400;
/// 세션 heartbeat TTL.
pub const SESSION_HEARTBEAT_TTL_SECS: u64 = 60;

/// login_name 을 포함한 prefix 를 만든다.
pub fn state_prefix(base: &str, login_name: &str) -> String {
    format!("{base}:{login_name}:")
}

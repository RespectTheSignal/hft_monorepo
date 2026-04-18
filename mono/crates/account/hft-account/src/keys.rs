//! 계정/서브계정 관련 키 규칙.
//!
//! 현재 D-2 배치에서는 Redis 직접 연동은 하지 않지만, login_name 기반 state key naming
//! 규칙을 한 곳에 문서화해 둔다. 후속 Phase 에서 `hft-state` 와 공유할 때 기준점으로 쓴다.

/// 기본 state prefix.
pub const DEFAULT_STATE_PREFIX: &str = "gate_hft:state";

/// `{prefix}:{login_name}:` 형태의 계정 prefix 를 만든다.
pub fn account_prefix(prefix: &str, login_name: &str) -> String {
    format!("{prefix}:{login_name}:")
}

/// last-order 키 suffix.
pub fn last_order_key(prefix: &str, login_name: &str, symbol: &str) -> String {
    format!("{}last_order:{symbol}", account_prefix(prefix, login_name))
}

/// 세션 메타 키.
pub fn session_key(prefix: &str, login_name: &str) -> String {
    format!("{}session", account_prefix(prefix, login_name))
}

/// 세션 heartbeat 키.
pub fn session_heartbeat_key(prefix: &str, login_name: &str) -> String {
    format!("{}session:heartbeat_ms", account_prefix(prefix, login_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_prefix_uses_login_name() {
        assert_eq!(
            account_prefix(DEFAULT_STATE_PREFIX, "sub01"),
            "gate_hft:state:sub01:"
        );
    }

    #[test]
    fn derived_keys_follow_convention() {
        assert_eq!(
            last_order_key(DEFAULT_STATE_PREFIX, "sub01", "BTC_USDT"),
            "gate_hft:state:sub01:last_order:BTC_USDT"
        );
        assert_eq!(
            session_key(DEFAULT_STATE_PREFIX, "sub01"),
            "gate_hft:state:sub01:session"
        );
        assert_eq!(
            session_heartbeat_key(DEFAULT_STATE_PREFIX, "sub01"),
            "gate_hft:state:sub01:session:heartbeat_ms"
        );
    }
}

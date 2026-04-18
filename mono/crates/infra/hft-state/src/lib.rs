//! Redis 기반 strategy state backend.
//!
//! Redis 가 없거나 연결에 실패해도 전략은 정상 동작해야 하므로, 모든 API 는
//! "연결 실패 = warn + no-op" 패턴을 따른다.

#![deny(rust_2018_idioms)]

pub mod keys;

use redis::aio::MultiplexedConnection;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use hft_strategy_core::risk::LastOrder;
use hft_strategy_runtime::LastOrderStore;

/// 전략 세션 메타데이터.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StrategySession {
    pub variant: String,
    pub login_name: String,
    pub start_time_ms: i64,
    pub pid: u32,
    pub hostname: String,
    pub last_heartbeat_ms: i64,
}

/// Redis 기반 state backend.
///
/// 연결 실패 시 모든 연산은 no-op 로 degrade 한다.
pub struct RedisState {
    conn: Option<MultiplexedConnection>,
    prefix: String,
}

impl RedisState {
    /// 환경변수 기반 초기화.
    ///
    /// - `REDIS_URL`
    /// - `REDIS_STATE_PREFIX` (default: `gate_hft:state`)
    pub async fn from_env(login_name: &str) -> Self {
        let url = std::env::var("REDIS_URL").unwrap_or_default();
        let base = std::env::var("REDIS_STATE_PREFIX")
            .unwrap_or_else(|_| keys::DEFAULT_STATE_PREFIX.to_string());
        Self::from_url_and_prefix(login_name, &url, &base).await
    }

    /// 연결 상태를 반환한다.
    pub fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    /// 심볼별 최근 주문을 Redis 에 snapshot 한다.
    pub async fn save_last_orders(&mut self, store: &LastOrderStore) {
        let prefix = self.prefix.clone();
        let Some(conn) = self.conn.as_mut() else {
            return;
        };

        let mut pipe = redis::pipe();
        let mut staged = 0usize;
        for entry in store.iter() {
            let key = format!("{prefix}{}{}", keys::LAST_ORDER_PREFIX, entry.key());
            let value = match serde_json::to_string(entry.value()) {
                Ok(json) => json,
                Err(e) => {
                    warn!(symbol = %entry.key(), error = %e, "Redis last_order serialize failed");
                    continue;
                }
            };
            pipe.cmd("SET")
                .arg(&key)
                .arg(&value)
                .arg("EX")
                .arg(keys::LAST_ORDER_TTL_SECS);
            staged += 1;
        }

        if staged == 0 {
            return;
        }

        let result: redis::RedisResult<()> = pipe.query_async(conn).await;
        if let Err(e) = result {
            warn!(error = %e, staged, "Redis save_last_orders failed");
        }
    }

    /// Redis 에서 `LastOrderStore` 를 복원한다.
    pub async fn restore_last_orders(&mut self, store: &LastOrderStore) {
        let last_order_prefix = self.key(keys::LAST_ORDER_PREFIX);
        let pattern = format!("{last_order_prefix}*");
        let Some(conn) = self.conn.as_mut() else {
            return;
        };

        let keys_result: redis::RedisResult<Vec<String>> =
            redis::cmd("KEYS").arg(&pattern).query_async(conn).await;
        let keys = match keys_result {
            Ok(keys) => keys,
            Err(e) => {
                warn!(error = %e, "Redis KEYS scan failed");
                return;
            }
        };

        if keys.is_empty() {
            info!("Redis: no last_order keys found — starting fresh");
            return;
        }

        let values_result: redis::RedisResult<Vec<Option<String>>> =
            redis::cmd("MGET").arg(&keys).query_async(conn).await;
        let values = match values_result {
            Ok(values) => values,
            Err(e) => {
                warn!(error = %e, "Redis MGET failed");
                return;
            }
        };

        let mut restored = 0u32;
        for (key, value) in keys.iter().zip(values.iter()) {
            let Some(symbol) = key.strip_prefix(&last_order_prefix) else {
                continue;
            };
            let Some(json) = value else {
                continue;
            };

            match serde_json::from_str::<LastOrder>(json) {
                Ok(order) => {
                    store.record(symbol.to_owned(), order);
                    restored = restored.saturating_add(1);
                }
                Err(e) => {
                    warn!(symbol, error = %e, "Redis last_order deserialize failed");
                }
            }
        }

        info!(restored, "Redis: last_orders restored");
    }

    /// 세션 메타데이터를 저장한다.
    pub async fn save_session(&mut self, session: &StrategySession) {
        let key = self.key(keys::SESSION_KEY);
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        let value = match serde_json::to_string(session) {
            Ok(json) => json,
            Err(e) => {
                warn!(error = %e, "Redis session serialize failed");
                return;
            }
        };

        let result: redis::RedisResult<()> = redis::cmd("SET")
            .arg(&key)
            .arg(&value)
            .arg("EX")
            .arg(keys::SESSION_TTL_SECS)
            .query_async(conn)
            .await;
        if let Err(e) = result {
            warn!(error = %e, "Redis save_session failed");
        }
    }

    /// 세션 heartbeat 를 갱신한다.
    pub async fn heartbeat(&mut self, now_ms: i64) {
        let key = self.key(keys::SESSION_HEARTBEAT_KEY);
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        let result: redis::RedisResult<()> = redis::cmd("SET")
            .arg(&key)
            .arg(now_ms)
            .arg("EX")
            .arg(keys::SESSION_HEARTBEAT_TTL_SECS)
            .query_async(conn)
            .await;
        if let Err(e) = result {
            warn!(error = %e, "Redis heartbeat failed");
        }
    }

    /// 이전 세션을 읽는다.
    pub async fn load_session(&mut self) -> Option<StrategySession> {
        let key = self.key(keys::SESSION_KEY);
        let conn = self.conn.as_mut()?;
        let result: redis::RedisResult<Option<String>> =
            redis::cmd("GET").arg(&key).query_async(conn).await;
        match result {
            Ok(Some(json)) => match serde_json::from_str(&json) {
                Ok(session) => Some(session),
                Err(e) => {
                    warn!(error = %e, "Redis session deserialize failed");
                    None
                }
            },
            Ok(None) => None,
            Err(e) => {
                warn!(error = %e, "Redis load_session failed");
                None
            }
        }
    }

    fn key(&self, suffix: &str) -> String {
        format!("{}{}", self.prefix, suffix)
    }

    async fn from_url_and_prefix(login_name: &str, url: &str, base: &str) -> Self {
        let prefix = keys::state_prefix(base, login_name);
        if url.is_empty() {
            info!("REDIS_URL not set — state persistence disabled");
            return Self { conn: None, prefix };
        }

        match redis::Client::open(url) {
            Ok(client) => match client.get_multiplexed_async_connection().await {
                Ok(conn) => {
                    info!(prefix, "Redis state connected");
                    let _ = client;
                    Self {
                        conn: Some(conn),
                        prefix,
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Redis connect failed — state persistence disabled");
                    let _ = client;
                    Self { conn: None, prefix }
                }
            },
            Err(e) => {
                warn!(error = %e, "Redis client init failed — state persistence disabled");
                Self { conn: None, prefix }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use hft_strategy_core::decision::{OrderLevel, OrderSide};

    #[tokio::test]
    async fn test_from_env_no_url() {
        let state = RedisState::from_url_and_prefix("sub01", "", keys::DEFAULT_STATE_PREFIX).await;
        assert!(!state.is_connected());
    }

    #[tokio::test]
    async fn test_key_prefix() {
        let state = RedisState::from_url_and_prefix("sub01", "", keys::DEFAULT_STATE_PREFIX).await;
        assert_eq!(state.key(keys::SESSION_KEY), "gate_hft:state:sub01:session");
    }

    #[tokio::test]
    async fn test_save_restore_disabled_noop() {
        let mut state =
            RedisState::from_url_and_prefix("sub01", "", keys::DEFAULT_STATE_PREFIX).await;
        let store = LastOrderStore::new();
        store.record(
            "BTC_USDT",
            LastOrder {
                level: OrderLevel::LimitOpen,
                side: OrderSide::Buy,
                price: 100.0,
                timestamp_ms: 123,
            },
        );
        state.save_last_orders(&store).await;
        store.clear();
        state.restore_last_orders(&store).await;
        assert!(store.is_empty());
    }

    #[test]
    fn test_last_order_serde_roundtrip() {
        let order = LastOrder {
            level: OrderLevel::LimitClose,
            side: OrderSide::Sell,
            price: 98.5,
            timestamp_ms: 456,
        };
        let json = serde_json::to_string(&order).expect("serialize last_order");
        let decoded: LastOrder = serde_json::from_str(&json).expect("deserialize last_order");
        assert_eq!(decoded.level, order.level);
        assert_eq!(decoded.side, order.side);
        assert_eq!(decoded.price, order.price);
        assert_eq!(decoded.timestamp_ms, order.timestamp_ms);
    }

    #[test]
    fn test_strategy_session_serde() {
        let session = StrategySession {
            variant: "v8".to_string(),
            login_name: "sub01".to_string(),
            start_time_ms: 1_700_000_000_000,
            pid: 42,
            hostname: "hft-vm".to_string(),
            last_heartbeat_ms: 1_700_000_030_000,
        };
        let json = serde_json::to_string(&session).expect("serialize session");
        let decoded: StrategySession = serde_json::from_str(&json).expect("deserialize session");
        assert_eq!(decoded, session);
    }
}

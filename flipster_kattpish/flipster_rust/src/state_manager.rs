use anyhow::{Context, Result};
use redis::Client;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct StateManager {
    client: Client,
    key_prefix: String,
}

impl StateManager {
    pub fn new(redis_url: &str, key_prefix: &str) -> Result<Self> {
        let client = Client::open(redis_url).context("Failed to create Redis client")?;
        Ok(Self {
            client,
            key_prefix: key_prefix.to_string(),
        })
    }

    fn key_for_login(&self, login_name: &str) -> String {
        format!("{}:{}", self.key_prefix, login_name)
    }

    fn key_login_status(login_name: &str) -> String {
        format!("login_status:{}", login_name)
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }

    pub fn update_state(
        &self,
        login_name: &str,
        is_healthy: bool,
        too_many_request: bool,
        num_positions: usize,
        login_base_url: String,
    ) -> Result<()> {
        let mut conn = self
            .client
            .get_connection()
            .context("Failed to connect to Redis")?;
        let key = self.key_for_login(login_name);
        let updated_at_ms = Self::now_ms();

        let mut pipe = redis::pipe();
        pipe.hset(&key, "login_name", login_name)
            .hset(&key, "is_healthy", if is_healthy { 1 } else { 0 })
            .hset(
                &key,
                "too_many_request",
                if too_many_request { 1 } else { 0 },
            )
            .hset(&key, "num_positions", num_positions as i64)
            .hset(&key, "updated_at_ms", updated_at_ms);

        // login_status:{login_name} with JSON value
        let login_status_key = Self::key_login_status(login_name);
        let login_status_value = json!({
            "updated_at_ms": updated_at_ms,
            "login_base_url": login_base_url,
        });
        pipe.set(&login_status_key, login_status_value.to_string());

        pipe.query::<()>(&mut conn)
            .context("Failed to write state to Redis")?;
        Ok(())
    }
}

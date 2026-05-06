use crate::error::{Error, Result};
use crate::ilp::BookTickerWriter;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct PublisherConfig {
    pub qdb_host: String,
    pub qdb_ilp_port: u16,
    pub topics_per_conn: usize,
    pub max_symbols: Option<usize>,
}

impl PublisherConfig {
    /// Reads QDB_HOST / QDB_ILP_PORT (defaults: 211.181.122.102 / 9009),
    /// optional TOPICS_PER_CONN and MAX_SYMBOLS overrides for the binary.
    pub fn from_env(default_topics_per_conn: usize) -> Self {
        dotenvy::dotenv().ok();
        let qdb_host = std::env::var("QDB_HOST")
            .unwrap_or_else(|_| "211.181.122.102".into());
        let qdb_ilp_port = std::env::var("QDB_ILP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9009);
        let topics_per_conn = std::env::var("TOPICS_PER_CONN")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n: &usize| n > 0)
            .unwrap_or(default_topics_per_conn);
        let max_symbols = std::env::var("MAX_SYMBOLS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n: &usize| n > 0);
        Self {
            qdb_host,
            qdb_ilp_port,
            topics_per_conn,
            max_symbols,
        }
    }

    pub fn writer(&self, table: &str) -> Result<BookTickerWriter> {
        BookTickerWriter::spawn(self.qdb_host.clone(), self.qdb_ilp_port, table.to_string())
    }
}

pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

/// Parse a stringified number (e.g. "0.123") to f64. Returns None on empty/NaN.
pub fn parse_f64(s: &str) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    s.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Backoff schedule for reconnect loops: 1, 2, 4, 8, 16s capped at 30s.
pub fn backoff_secs(attempt: u32) -> u64 {
    let base = 1u64 << attempt.min(5);
    base.min(30)
}

pub fn install_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// REST GET helper that returns parsed JSON or an `Error::Api` on non-2xx.
pub async fn http_get_json(client: &reqwest::Client, url: &str) -> Result<serde_json::Value> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Api {
            status: status.as_u16(),
            message: body,
        });
    }
    Ok(resp.json().await?)
}

pub async fn http_post_json(
    client: &reqwest::Client,
    url: &str,
) -> Result<serde_json::Value> {
    let resp = client.post(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Api {
            status: status.as_u16(),
            message: body,
        });
    }
    Ok(resp.json().await?)
}

pub async fn http_post_json_body(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value> {
    let resp = client.post(url).json(body).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        return Err(Error::Api {
            status: status.as_u16(),
            message: txt,
        });
    }
    Ok(resp.json().await?)
}

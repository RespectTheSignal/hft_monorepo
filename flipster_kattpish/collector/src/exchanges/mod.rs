pub mod binance;
pub mod flipster;
pub mod bybit;
pub mod bitget;
pub mod gate;

use crate::ilp::IlpWriter;
use crate::model::{BookTick, ExchangeName};
use crate::reconnect::Backoff;
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

#[async_trait]
pub trait ExchangeCollector: Send + Sync {
    fn name(&self) -> ExchangeName;
    fn ws_url(&self) -> String;
    /// Build the subscribe message(s) for the given symbol list.
    fn subscribe_msgs(&self, symbols: &[String]) -> Vec<String>;
    /// Parse a raw text frame into zero or more bookticks.
    fn parse_frame(&self, frame: &str) -> Vec<BookTick>;
    /// Optional extra HTTP headers for the WebSocket handshake (auth, etc.).
    /// Default: none.
    fn handshake_headers(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Drives a single collector in an infinite reconnect loop.
pub async fn run_collector<C: ExchangeCollector + 'static>(
    collector: C,
    symbols: Vec<String>,
    writer: IlpWriter,
    tx: broadcast::Sender<BookTick>,
) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Error as TError;
    use tokio_tungstenite::tungstenite::Message;

    let name = collector.name();
    // flipster has a stricter rate limit — let the backoff reach 5 min so we
    // don't hammer it during a lockout; other venues are fine at 60s cap.
    let max_secs = if matches!(collector.name(), ExchangeName::Flipster) { 300 } else { 60 };
    let mut backoff = Backoff::new(max_secs);
    loop {
        let url = collector.ws_url();
        info!(exchange = %name, url = %url, "connecting");
        let req_result = url.as_str().into_client_request();
        let req = match req_result {
            Ok(mut r) => {
                for (k, v) in collector.handshake_headers() {
                    if let (Ok(hk), Ok(hv)) = (
                        http::HeaderName::from_bytes(k.as_bytes()),
                        http::HeaderValue::from_str(&v),
                    ) {
                        r.headers_mut().insert(hk, hv);
                    }
                }
                r
            }
            Err(e) => {
                error!(exchange = %name, error = %e, "bad ws url");
                tokio::time::sleep(backoff.next_delay()).await;
                continue;
            }
        };
        // Tungstenite exposes the response headers on HTTP errors (429, etc.)
        // so we can honor Cloudflare's `x-ratelimit-reset` / `Retry-After`
        // instead of blind exponential backoff.
        match connect_async(req).await {
            Err(TError::Http(resp)) => {
                let status = resp.status();
                let now = chrono::Utc::now().timestamp();
                // x-ratelimit-reset is a unix epoch integer
                let reset_in = resp
                    .headers()
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(|ts| (ts - now).max(1));
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<i64>().ok());
                let hint = retry_after.or(reset_in);
                error!(
                    exchange = %name,
                    status = %status,
                    reset_in_sec = hint.unwrap_or(-1),
                    "http error; honoring server-provided wait hint if any"
                );
                let wait = if let Some(sec) = hint {
                    // add 2s safety cushion + small jitter
                    std::time::Duration::from_secs((sec + 2).max(1) as u64)
                        + std::time::Duration::from_millis(
                            rand::random::<u64>() % 1000,
                        )
                } else {
                    backoff.next_delay()
                };
                writer
                    .write_health(name.as_str(), "rate_limited", &status.to_string())
                    .await
                    .ok();
                warn!(
                    exchange = %name,
                    wait_sec = wait.as_secs(),
                    "waiting (server-hinted)"
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            Ok((mut ws, _)) => {
                backoff.reset();
                writer
                    .write_health(name.as_str(), "connected", "")
                    .await
                    .ok();
                for m in collector.subscribe_msgs(&symbols) {
                    if let Err(e) = ws.send(Message::Text(m)).await {
                        warn!(exchange = %name, error = %e, "subscribe send failed");
                        break;
                    }
                }
                while let Some(msg) = ws.next().await {
                    match msg {
                        Ok(Message::Text(txt)) => {
                            for tick in collector.parse_frame(&txt) {
                                let _ = tx.send(tick.clone());
                                if let Err(e) = writer.write_tick(&tick).await {
                                    warn!(exchange = %name, error = %e, "ilp write failed");
                                }
                            }
                        }
                        Ok(Message::Ping(p)) => {
                            let _ = ws.send(Message::Pong(p)).await;
                        }
                        Ok(Message::Close(_)) => {
                            warn!(exchange = %name, "remote closed");
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            error!(exchange = %name, error = %e, "ws error");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                error!(exchange = %name, error = %e, "connect failed");
            }
        }
        writer
            .write_health(name.as_str(), "disconnected", "")
            .await
            .ok();
        let d = backoff.next_delay();
        warn!(exchange = %name, delay_ms = d.as_millis() as u64, "reconnecting");
        tokio::time::sleep(d).await;
    }
}

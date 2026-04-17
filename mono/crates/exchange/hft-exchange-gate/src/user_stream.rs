//! Gate.io Futures private user stream.
//!
//! 주문/체결/포지션/잔고 이벤트를 private WS 로 수신한다. 전략 쪽은 이 스트림을
//! REST 폴링의 저지연 보강 경로로 사용하고, 폴링은 fallback 으로 유지한다.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::de::Error as _;
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, trace, warn};
use url::Url;

use hft_exchange_api::{Backoff, CancellationToken};
use hft_exchange_common::{deserialize_f64_lenient, deserialize_i64_lenient};
use hft_exchange_rest::hmac_sha512_hex;
use hft_time::{Clock, SystemClock};

const USER_STREAM_URL: &str = "wss://fx-ws.gateio.ws/v4/ws/usdt";
const PING_INTERVAL: Duration = Duration::from_secs(20);
const READ_TIMEOUT: Duration = Duration::from_secs(65);

/// user stream 이벤트 소비 콜백.
pub type UserStreamCallback = Arc<dyn Fn(UserStreamEvent) + Send + Sync + 'static>;

/// Gate private WS 수신기.
#[derive(Debug, Clone)]
pub struct GateUserStream {
    api_key: String,
    api_secret: String,
}

impl GateUserStream {
    /// 키/시크릿으로 새 user stream 생성.
    pub fn new(api_key: String, api_secret: String) -> Self {
        Self {
            api_key,
            api_secret,
        }
    }

    /// WS 연결 → 인증 → 구독 → 이벤트 수신 루프.
    ///
    /// 연결 끊김은 내부에서 backoff 후 재연결한다. cancel 이 오면 즉시 종료한다.
    pub async fn run(&self, on_event: UserStreamCallback, cancel: CancellationToken) -> Result<()> {
        let mut backoff = Backoff::new(1_000, 30_000);
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }

            let attempt = backoff.attempts();
            info!(attempt, "gate user stream connecting");
            let outcome = tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                r = self.run_once(on_event.clone(), cancel.child_token()) => r,
            };

            match outcome {
                Ok(()) => {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    warn!("gate user stream disconnected cleanly, reconnecting");
                    backoff.reset();
                }
                Err(e) => warn!(error = %e, "gate user stream error; reconnecting with backoff"),
            }

            let delay_ms = backoff.next_ms();
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            }
        }
    }

    async fn run_once(
        &self,
        on_event: UserStreamCallback,
        cancel: CancellationToken,
    ) -> Result<()> {
        let url = Url::parse(USER_STREAM_URL)?;
        let (ws, resp) = connect_async(url.as_str()).await?;
        info!(status = ?resp.status(), "gate user stream connected");

        let (mut write, mut read) = ws.split();
        let ts = current_epoch_secs();
        for (idx, channel) in [
            "futures.orders",
            "futures.usertrades",
            "futures.positions",
            "futures.balances",
        ]
        .iter()
        .enumerate()
        {
            let sub = self.subscribe_message(channel, ts + idx as i64, &["!all"]);
            write
                .send(Message::Text(sub.to_string()))
                .await
                .with_context(|| format!("subscribe send failed: {channel}"))?;
        }
        info!("gate user stream subscribed");

        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                _ = ping.tick() => {
                    let ping_msg = serde_json::json!({
                        "time": current_epoch_secs(),
                        "channel": "futures.ping",
                    });
                    if let Err(e) = write.send(Message::Text(ping_msg.to_string())).await {
                        return Err(anyhow!("user stream ping send failed: {e}"));
                    }
                    continue;
                }
                res = tokio::time::timeout(READ_TIMEOUT, read.next()) => res,
            };

            let msg = match msg {
                Ok(Some(Ok(msg))) => msg,
                Ok(Some(Err(e))) => return Err(e.into()),
                Ok(None) => return Ok(()),
                Err(_) => return Err(anyhow!("gate user stream read timeout")),
            };

            match msg {
                Message::Text(text) => {
                    if let Err(e) = self.handle_text_frame(text.as_ref(), &on_event) {
                        warn!(error = %e, "gate user stream frame handling failed");
                    }
                }
                Message::Ping(payload) => {
                    trace!("gate user stream ping → pong");
                    write.send(Message::Pong(payload)).await?;
                }
                Message::Pong(_) => {}
                Message::Binary(bin) => {
                    trace!(len = bin.len(), "gate user stream binary frame ignored");
                }
                Message::Close(frame) => {
                    warn!(?frame, "gate user stream server close");
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    fn subscribe_message(&self, channel: &str, time: i64, payload: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "time": time,
            "channel": channel,
            "event": "subscribe",
            "payload": payload,
            "auth": {
                "method": "api_key",
                "KEY": self.api_key.as_str(),
                "SIGN": sign_ws_auth(&self.api_secret, channel, "subscribe", time),
            }
        })
    }

    fn handle_text_frame(&self, text: &str, on_event: &UserStreamCallback) -> Result<()> {
        let frame: UserStreamFrame<'_> =
            serde_json::from_str(text).context("parse gate user stream frame")?;

        if let Some(kind) = frame.frame_type {
            if kind == "service_upgrade" {
                warn!(
                    msg = frame.msg.unwrap_or(""),
                    "gate user stream service upgrade"
                );
                return Ok(());
            }
        }

        if let Some(err) = frame.error {
            warn!(
                channel = frame.channel.unwrap_or("unknown"),
                event = frame.event.unwrap_or("unknown"),
                error = err.get(),
                "gate user stream frame reported error"
            );
            return Ok(());
        }

        let Some(channel) = frame.channel else {
            return Ok(());
        };
        let event = frame.event.unwrap_or_default();
        if channel == "futures.pong" || channel == "futures.ping" {
            return Ok(());
        }
        if event != "update" && event != "all" {
            debug!(channel, event, "gate user stream non-update frame ignored");
            return Ok(());
        }

        let Some(result) = frame.result else {
            return Ok(());
        };

        match channel {
            "futures.orders" => {
                for payload in decode_payloads::<OrderUpdatePayload>(result)? {
                    on_event(UserStreamEvent::OrderUpdate(payload));
                }
            }
            "futures.usertrades" => {
                for payload in decode_payloads::<UserTradePayload>(result)? {
                    on_event(UserStreamEvent::UserTrade(payload));
                }
            }
            "futures.positions" => {
                for payload in decode_payloads::<PositionUpdatePayload>(result)? {
                    on_event(UserStreamEvent::PositionUpdate(payload));
                }
            }
            "futures.balances" => {
                for payload in decode_payloads::<BalanceUpdatePayload>(result)? {
                    on_event(UserStreamEvent::BalanceUpdate(payload));
                }
            }
            other => debug!(
                channel = other,
                "gate user stream unsupported channel ignored"
            ),
        }
        Ok(())
    }
}

/// user stream 이벤트.
#[derive(Debug, Clone)]
pub enum UserStreamEvent {
    OrderUpdate(OrderUpdatePayload),
    UserTrade(UserTradePayload),
    PositionUpdate(PositionUpdatePayload),
    BalanceUpdate(BalanceUpdatePayload),
}

/// futures.orders payload.
#[derive(Debug, Clone, Deserialize)]
pub struct OrderUpdatePayload {
    #[serde(deserialize_with = "deserialize_i64_lenient")]
    pub id: i64,
    pub contract: String,
    #[serde(deserialize_with = "deserialize_i64_lenient")]
    pub size: i64,
    #[serde(deserialize_with = "deserialize_i64_lenient")]
    pub left: i64,
    pub price: String,
    pub status: String,
    #[serde(default)]
    pub is_close: bool,
    #[serde(default)]
    pub fill_price: String,
    #[serde(deserialize_with = "deserialize_f64_lenient")]
    pub create_time: f64,
    #[serde(default, deserialize_with = "deserialize_f64_default_lenient")]
    pub finish_time: f64,
    #[serde(default)]
    pub text: String,
}

/// futures.positions payload.
#[derive(Debug, Clone, Deserialize)]
pub struct PositionUpdatePayload {
    pub contract: String,
    #[serde(deserialize_with = "deserialize_i64_lenient")]
    pub size: i64,
    pub entry_price: String,
    #[serde(deserialize_with = "deserialize_u32_lenient")]
    pub leverage: u32,
    pub margin: String,
    pub mode: String,
    pub realised_pnl: String,
    pub unrealised_pnl: String,
    pub liq_price: String,
    pub mark_price: String,
    #[serde(deserialize_with = "deserialize_i64_lenient")]
    pub update_id: i64,
    #[serde(default, deserialize_with = "deserialize_i64_lenient_opt_default")]
    pub update_time: i64,
}

/// futures.balances payload.
#[derive(Debug, Clone, Deserialize)]
pub struct BalanceUpdatePayload {
    pub balance: String,
    pub change: String,
    pub fund_type: String,
    #[serde(deserialize_with = "deserialize_i64_lenient")]
    pub time_ms: i64,
}

/// futures.usertrades payload.
#[derive(Debug, Clone, Deserialize)]
pub struct UserTradePayload {
    pub contract: String,
    #[serde(deserialize_with = "deserialize_i64_lenient")]
    pub size: i64,
    pub price: String,
    #[serde(deserialize_with = "deserialize_i64_lenient")]
    pub order_id: i64,
    #[serde(deserialize_with = "deserialize_i64_lenient")]
    pub create_time_ms: i64,
    pub role: String,
    pub fee: String,
}

#[derive(Deserialize)]
struct UserStreamFrame<'a> {
    #[serde(borrow, default)]
    channel: Option<&'a str>,
    #[serde(borrow, default)]
    event: Option<&'a str>,
    #[serde(borrow, default)]
    result: Option<&'a RawValue>,
    #[serde(borrow, default)]
    error: Option<&'a RawValue>,
    #[serde(rename = "type", borrow, default)]
    frame_type: Option<&'a str>,
    #[serde(borrow, default)]
    msg: Option<&'a str>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

fn decode_payloads<T>(raw: &RawValue) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let parsed: OneOrMany<T> = serde_json::from_str(raw.get())?;
    Ok(match parsed {
        OneOrMany::One(item) => vec![item],
        OneOrMany::Many(items) => items,
    })
}

/// WS auth signature.
pub fn sign_ws_auth(secret: &str, channel: &str, event: &str, time: i64) -> String {
    let message = format!("channel={channel}&event={event}&time={time}");
    hmac_sha512_hex(secret.as_bytes(), message.as_bytes())
}

fn current_epoch_secs() -> i64 {
    SystemClock::default().now_ms() / 1000
}

fn deserialize_u32_lenient<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_i64_lenient(deserializer)?;
    u32::try_from(value).map_err(D::Error::custom)
}

fn deserialize_f64_default_lenient<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        Str(String),
        F64(f64),
        I64(i64),
    }

    let value = Option::<StringOrNumber>::deserialize(deserializer)?;
    Ok(match value {
        None => 0.0,
        Some(StringOrNumber::Str(s)) => s.parse::<f64>().map_err(D::Error::custom)?,
        Some(StringOrNumber::F64(v)) => v,
        Some(StringOrNumber::I64(v)) => v as f64,
    })
}

fn deserialize_i64_lenient_opt_default<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        Str(String),
        I64(i64),
        F64(f64),
    }

    let value = Option::<StringOrNumber>::deserialize(deserializer)?;
    Ok(match value {
        None => 0,
        Some(StringOrNumber::Str(s)) => s.parse::<i64>().map_err(D::Error::custom)?,
        Some(StringOrNumber::I64(v)) => v,
        Some(StringOrNumber::F64(v)) => v as i64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_ws_auth() {
        let sign = sign_ws_auth("secret-test", "futures.orders", "subscribe", 1_710_000_000);
        assert_eq!(
            sign,
            "ac20fba1855c2f8969d111054f5e646ec08ef6acaab82fa49d9dfbd2d5a342205de159521b2bde3f9ecb2ec1a96d2ed6a60ac6e5a72c154632c340c7945d6606"
        );
    }

    #[test]
    fn test_deserialize_order_update() {
        let raw = r#"{
            "id": 123,
            "contract": "BTC_USDT",
            "size": "2",
            "left": 1,
            "price": "65000.5",
            "status": "open",
            "is_close": true,
            "fill_price": "64999.0",
            "create_time": 1710000000,
            "finish_time": 1710000012,
            "text": "v8-42"
        }"#;
        let payload: OrderUpdatePayload = serde_json::from_str(raw).expect("order payload");
        assert_eq!(payload.id, 123);
        assert_eq!(payload.contract, "BTC_USDT");
        assert_eq!(payload.left, 1);
        assert!(payload.is_close);
    }

    #[test]
    fn test_deserialize_position_update() {
        let raw = r#"{
            "contract": "BTC_USDT",
            "size": "-3",
            "entry_price": "65100.0",
            "leverage": "20",
            "margin": "123.4",
            "mode": "single",
            "realised_pnl": "1.2",
            "unrealised_pnl": "-2.3",
            "liq_price": "70000",
            "mark_price": "65050.5",
            "update_id": 99,
            "update_time": 1710000100
        }"#;
        let payload: PositionUpdatePayload = serde_json::from_str(raw).expect("position payload");
        assert_eq!(payload.size, -3);
        assert_eq!(payload.leverage, 20);
        assert_eq!(payload.update_id, 99);
        assert_eq!(payload.update_time, 1_710_000_100);
    }

    #[test]
    fn test_deserialize_balance_update() {
        let raw = r#"{
            "balance": "1024.5",
            "change": "-5.5",
            "fund_type": "fee",
            "time_ms": "1710000000123"
        }"#;
        let payload: BalanceUpdatePayload = serde_json::from_str(raw).expect("balance payload");
        assert_eq!(payload.balance, "1024.5");
        assert_eq!(payload.time_ms, 1_710_000_000_123);
    }

    #[test]
    fn test_deserialize_user_trade() {
        let raw = r#"{
            "contract": "BTC_USDT",
            "size": "1",
            "price": "65010.0",
            "order_id": "1234",
            "create_time_ms": 1710000000456,
            "role": "maker",
            "fee": "0.1"
        }"#;
        let payload: UserTradePayload = serde_json::from_str(raw).expect("user trade payload");
        assert_eq!(payload.order_id, 1234);
        assert_eq!(payload.role, "maker");
    }
}

//! Telegram Bot API helper.
//!
//! monitoring-agent 등 비핫패스 서비스에서 Telegram 알림을 보낼 때 사용한다.

#![deny(rust_2018_idioms)]

use anyhow::{Context, Result};
use serde_json::json;
use std::time::Duration;

/// Telegram Bot API 를 통한 알림 전송기.
///
/// `TELEGRAM_BOT_TOKEN` + `TELEGRAM_CHAT_ID` 환경변수로 설정한다.
#[derive(Clone)]
pub struct TelegramNotifier {
    bot_token: String,
    chat_id: String,
    client: reqwest::Client,
}

impl TelegramNotifier {
    /// 환경변수에서 초기화한다.
    ///
    /// 토큰 또는 chat id 가 없으면 `None` 을 반환해 알림을 비활성화한다.
    pub fn from_env() -> Option<Self> {
        let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").ok()?;
        let chat_id = std::env::var("TELEGRAM_CHAT_ID").ok()?;
        Self::new(bot_token, chat_id).ok()
    }

    /// 명시적 token/chat id 로 notifier 를 생성한다.
    pub fn new(bot_token: String, chat_id: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("telegram reqwest client build")?;
        Ok(Self {
            bot_token,
            chat_id,
            client,
        })
    }

    /// 메시지를 전송한다.
    ///
    /// 호출자는 실패를 warn 로만 다루고 계속 진행해야 한다.
    pub async fn send(&self, text: &str) -> Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        let body = self.build_body(text);

        let resp = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .context("telegram send request")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("telegram send failed: status={status}, body={body}");
        }

        Ok(())
    }

    fn build_body(&self, text: &str) -> serde_json::Value {
        json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "HTML",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LK: OnceLock<Mutex<()>> = OnceLock::new();
        LK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn from_env_missing_returns_none() {
        let _g = env_lock();
        std::env::remove_var("TELEGRAM_BOT_TOKEN");
        std::env::remove_var("TELEGRAM_CHAT_ID");
        assert!(TelegramNotifier::from_env().is_none());
    }

    #[test]
    fn build_body_uses_html_parse_mode() {
        let notifier =
            TelegramNotifier::new("bot-token".into(), "chat-id".into()).expect("notifier");
        let body = notifier.build_body("<b>alert</b>");
        assert_eq!(body["chat_id"], "chat-id");
        assert_eq!(body["text"], "<b>alert</b>");
        assert_eq!(body["parse_mode"], "HTML");
    }
}

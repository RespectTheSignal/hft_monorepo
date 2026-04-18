//! 멀티 서브계정 JSON 설정 로딩.
//!
//! JSON 스키마 (`HFT_ACCOUNTS_FILE`):
//! ```json
//! {
//!   "accounts": [
//!     {
//!       "login_name": "sigma01",
//!       "api_key": "...",
//!       "api_secret": "...",
//!       "symbols": ["BTC_USDT", "ETH_USDT"],
//!       "leverage": 50.0,
//!       "enabled": true
//!     }
//!   ]
//! }
//! ```
//!
//! `HFT_ACCOUNTS_FILE` 미설정 시 기존 `GATE_API_KEY` / `GATE_API_SECRET` 단일 계정
//! fallback 을 사용한다.

use std::path::Path;

use ahash::AHashSet;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 개별 서브계정 엔트리.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountEntry {
    pub login_name: String,
    pub api_key: String,
    pub api_secret: String,
    /// 이 계정이 운영하는 심볼 리스트. 비어있으면 전체 심볼.
    #[serde(default)]
    pub symbols: Vec<String>,
    /// 레버리지 배수. 미설정 시 글로벌 RiskConfig leverage 를 따른다.
    pub leverage: Option<f64>,
    /// false 이면 로딩만 하고 실제 polling/trading 은 하지 않는다.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// 멀티계정 설정 파일 최상위 구조.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountsConfig {
    pub accounts: Vec<AccountEntry>,
}

/// `HFT_ACCOUNTS_FILE` → JSON → `AccountsConfig` 로드.
///
/// 환경변수 미설정이면 `None` 을 반환한다.
pub fn load_accounts_from_env() -> Result<Option<AccountsConfig>> {
    let path = match std::env::var("HFT_ACCOUNTS_FILE") {
        Ok(path) if !path.trim().is_empty() => path,
        _ => return Ok(None),
    };
    let config = load_accounts_from_file(Path::new(&path))
        .with_context(|| format!("load accounts from {path}"))?;
    Ok(Some(config))
}

/// JSON 파일에서 `AccountsConfig` 를 로드한다.
pub fn load_accounts_from_file(path: &Path) -> Result<AccountsConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read accounts file: {}", path.display()))?;
    let config: AccountsConfig = serde_json::from_str(&content).context("parse accounts JSON")?;

    let mut seen = AHashSet::new();
    for entry in &config.accounts {
        anyhow::ensure!(
            !entry.login_name.is_empty(),
            "account entry has empty login_name"
        );
        anyhow::ensure!(
            !entry.api_key.is_empty(),
            "account '{}' has empty api_key",
            entry.login_name
        );
        anyhow::ensure!(
            !entry.api_secret.is_empty(),
            "account '{}' has empty api_secret",
            entry.login_name
        );
        anyhow::ensure!(
            seen.insert(entry.login_name.clone()),
            "duplicate login_name: '{}'",
            entry.login_name
        );
    }

    Ok(config)
}

/// 기존 단일 계정 환경변수에서 fallback 엔트리를 만든다.
pub fn single_account_fallback() -> Option<AccountEntry> {
    let api_key = std::env::var("GATE_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())?;
    let api_secret = std::env::var("GATE_API_SECRET")
        .ok()
        .filter(|v| !v.is_empty())?;
    let login_name =
        std::env::var("HFT_STRATEGY_LOGIN_NAME").unwrap_or_else(|_| "default".to_string());
    Some(AccountEntry {
        login_name,
        api_key,
        api_secret,
        symbols: Vec::new(),
        leverage: None,
        enabled: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    fn sample_json() -> &'static str {
        r#"{
          "accounts": [
            {
              "login_name": "sigma01",
              "api_key": "k1",
              "api_secret": "s1",
              "symbols": ["BTC_USDT"],
              "leverage": 20.0,
              "enabled": true
            }
          ]
        }"#
    }

    #[test]
    fn load_accounts_from_file_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("accounts.json");
        std::fs::write(&path, sample_json()).expect("write json");
        let config = load_accounts_from_file(&path).expect("load config");
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].login_name, "sigma01");
        assert_eq!(config.accounts[0].symbols, vec!["BTC_USDT"]);
        assert_eq!(config.accounts[0].leverage, Some(20.0));
    }

    #[test]
    fn duplicate_login_name_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("dup.json");
        let body = r#"{
          "accounts": [
            { "login_name": "dup", "api_key": "k1", "api_secret": "s1" },
            { "login_name": "dup", "api_key": "k2", "api_secret": "s2" }
          ]
        }"#;
        std::fs::write(&path, body).expect("write json");
        assert!(load_accounts_from_file(&path).is_err());
    }

    #[test]
    fn default_enabled_is_true() {
        let body = r#"{
          "accounts": [
            { "login_name": "sigma01", "api_key": "k1", "api_secret": "s1" }
          ]
        }"#;
        let config: AccountsConfig = serde_json::from_str(body).expect("parse");
        assert_eq!(config.accounts.len(), 1);
        assert!(config.accounts[0].enabled);
    }
}

//! 멀티 서브계정 credential 레지스트리.
//!
//! `AccountManager` 는 login_name → (`Credentials`, `GateAccountClient`, symbols)
//! 매핑을 보유한다.

use ahash::AHashMap;
use anyhow::{Context, Result};
use hft_exchange_gate::GateAccountClient;
use hft_exchange_rest::{Credentials, RestClient};
use tracing::{info, warn};

use crate::accounts::{load_accounts_from_env, single_account_fallback, AccountEntry};

/// 개별 계정 핸들.
#[derive(Clone)]
pub struct AccountHandle {
    /// 계정 식별자.
    pub login_name: String,
    /// Gate REST 클라이언트.
    pub client: GateAccountClient,
    /// 계정이 담당하는 심볼 목록. 비어있으면 전체.
    pub symbols: Vec<String>,
    /// 계정별 레버리지 override.
    pub leverage: Option<f64>,
    /// 다른 모듈이 재사용할 수 있는 credentials.
    pub credentials: Credentials,
}

/// 멀티 서브계정 레지스트리.
pub struct AccountManager {
    accounts: AHashMap<String, AccountHandle>,
    /// 등록 순서를 보존한다.
    order: Vec<String>,
}

impl AccountManager {
    /// 환경변수 기반으로 계정 레지스트리를 구성한다.
    pub fn from_env() -> Result<Self> {
        if let Some(config) = load_accounts_from_env()? {
            let entries: Vec<AccountEntry> = config
                .accounts
                .into_iter()
                .filter(|entry| entry.enabled)
                .collect();
            if entries.is_empty() {
                warn!(
                    target: "hft_account::manager",
                    "HFT_ACCOUNTS_FILE loaded but all accounts disabled"
                );
            }
            return Self::from_entries(&entries);
        }

        if let Some(entry) = single_account_fallback() {
            info!(
                target: "hft_account::manager",
                login = %entry.login_name,
                "single-account fallback (GATE_API_KEY/SECRET)"
            );
            return Self::from_entries(&[entry]);
        }

        warn!(
            target: "hft_account::manager",
            "no account credentials found — AccountManager is empty"
        );
        Ok(Self {
            accounts: AHashMap::new(),
            order: Vec::new(),
        })
    }

    /// `AccountEntry` 슬라이스에서 레지스트리를 구성한다.
    pub fn from_entries(entries: &[AccountEntry]) -> Result<Self> {
        let mut accounts = AHashMap::with_capacity(entries.len());
        let mut order = Vec::with_capacity(entries.len());

        for entry in entries {
            let http = RestClient::new()
                .with_context(|| format!("RestClient for '{}'", entry.login_name))?;
            let creds = Credentials::new(entry.api_key.as_str(), entry.api_secret.as_str());
            creds
                .validate()
                .with_context(|| format!("validate creds for '{}'", entry.login_name))?;

            let handle = AccountHandle {
                login_name: entry.login_name.clone(),
                client: GateAccountClient::new(creds.clone(), http),
                symbols: entry.symbols.clone(),
                leverage: entry.leverage,
                credentials: creds,
            };
            accounts.insert(entry.login_name.clone(), handle);
            if !order.iter().any(|login| login == &entry.login_name) {
                order.push(entry.login_name.clone());
            }
        }

        info!(
            target: "hft_account::manager",
            count = accounts.len(),
            logins = ?order,
            "AccountManager initialized"
        );

        Ok(Self { accounts, order })
    }

    /// login_name 으로 계정 핸들을 조회한다.
    pub fn get(&self, login_name: &str) -> Option<&AccountHandle> {
        self.accounts.get(login_name)
    }

    /// 활성 계정 목록을 등록 순서대로 반환한다.
    pub fn enabled_accounts(&self) -> Vec<&AccountHandle> {
        self.order
            .iter()
            .filter_map(|name| self.accounts.get(name))
            .collect()
    }

    /// 등록된 계정 수.
    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    /// 빈 레지스트리인지.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// 등록된 login_name 목록.
    pub fn login_names(&self) -> &[String] {
        &self.order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(name: &str) -> AccountEntry {
        AccountEntry {
            login_name: name.to_string(),
            api_key: "test_key".to_string(),
            api_secret: "test_secret".to_string(),
            symbols: vec!["BTC_USDT".to_string()],
            leverage: Some(20.0),
            enabled: true,
        }
    }

    #[test]
    fn from_entries_basic() {
        let entries = vec![sample_entry("acct1"), sample_entry("acct2")];
        let mgr = AccountManager::from_entries(&entries).expect("manager");
        assert_eq!(mgr.len(), 2);
        assert!(mgr.get("acct1").is_some());
        assert!(mgr.get("acct2").is_some());
        assert!(mgr.get("acct3").is_none());
        assert_eq!(mgr.login_names(), &["acct1", "acct2"]);
    }

    #[test]
    fn empty_entries() {
        let mgr = AccountManager::from_entries(&[]).expect("manager");
        assert!(mgr.is_empty());
        assert_eq!(mgr.enabled_accounts().len(), 0);
    }

    #[test]
    fn duplicate_login_name_last_wins_but_order_is_stable() {
        let entries = vec![sample_entry("dup"), sample_entry("dup")];
        let mgr = AccountManager::from_entries(&entries).expect("manager");
        assert_eq!(mgr.len(), 1);
        assert_eq!(mgr.login_names(), &["dup"]);
    }
}

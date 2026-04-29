// Account manager for loading API keys from JSON files
// Similar to Python's AccountManager

use crate::elog;
use anyhow::{Context, Result};
use hex;
use hmac::{Hmac, Mac};
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha512;
use std::collections::HashMap;
use std::fs;

const GATE_API_BASE: &str = "https://api.gateio.ws/api/v4";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAccountKey {
    pub state: i32,
    pub mode: i32,
    pub name: String,
    pub user_id: i32,
    pub perms: Vec<serde_json::Value>,
    pub ip_whitelist: Vec<String>,
    pub secret: String,
    pub key: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MainAccountSecret {
    pub api_key: String,
    #[serde(rename = "secret_key")]
    pub secret: String,
    pub uid: i32,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateSubAccount {
    pub user_id: i32,
    pub login_name: String,
    #[serde(flatten)]
    pub _extra: serde_json::Value,
}

pub struct AccountManager {
    sub_account_keys: HashMap<i32, SubAccountKey>,
    sub_account_uid_to_subaccount: HashMap<i32, SubAccountKey>, // Python과 동일: self.sub_account_uid_to_subaccount
    main_account_uid_to_secrets: HashMap<i32, MainAccountSecret>,
    main_account_uid_to_subaccounts: HashMap<i32, Vec<GateSubAccount>>,
    sub_account_uid_to_login_name: HashMap<i32, String>,
    current_ip: String,
    client: Client,
}

impl AccountManager {
    pub fn new(
        sub_account_keys_path: &str,
        main_account_secrets_path: &str,
        current_ip: Option<String>,
    ) -> Result<Self> {
        let current_ip = current_ip.unwrap_or_else(|| {
            Self::get_ip().unwrap_or_else(|| {
                std::env::var("CURRENT_IP").unwrap_or_else(|_| "127.0.0.1".to_string())
            })
        });

        let mut manager = Self {
            sub_account_keys: HashMap::new(),
            sub_account_uid_to_subaccount: HashMap::new(), // Python과 동일
            main_account_uid_to_secrets: HashMap::new(),
            main_account_uid_to_subaccounts: HashMap::new(),
            sub_account_uid_to_login_name: HashMap::new(),
            current_ip,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
        };

        manager.load_settings(sub_account_keys_path, main_account_secrets_path)?;

        Ok(manager)
    }

    /// Get current IP address by calling Gate API (same as Python get_ip())
    fn get_ip() -> Option<String> {
        // Use test API key to get IP from error message
        let test_key = "09f15231133216fc086682db38bdfd76";
        let test_secret = "147e8b78728ff50a2b93d9b47a4ba64cf005eeeff0aaa29667d08d3cce5d1af5";

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;

        let url = format!("{}/account/detail", GATE_API_BASE);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();

        let signature_string = format!("GET\n/api/v4/account/detail\n\n{}\n{}", "", timestamp);
        let mut mac = Hmac::<Sha512>::new_from_slice(test_secret.as_bytes()).ok()?;
        mac.update(signature_string.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());

        let response = client
            .get(&url)
            .header("KEY", test_key)
            .header("Timestamp", &timestamp)
            .header("SIGN", &signature)
            .send()
            .ok()?;

        // Extract IP from error message
        if let Ok(text) = response.text() {
            let re = Regex::new(r": (\d+\.\d+\.\d+\.\d+)").ok()?;
            if let Some(caps) = re.captures(&text) {
                return Some(caps.get(1)?.as_str().to_string());
            }
        }

        None
    }

    fn load_settings(
        &mut self,
        sub_account_keys_path: &str,
        main_account_secrets_path: &str,
    ) -> Result<()> {
        // Flipster mode: skip Gate API lookups entirely; synthesize single-account maps.
        if crate::flipster_cookie_store::global().is_some() {
            self.load_sub_account_keys(sub_account_keys_path)?;
            self.sub_account_uid_to_subaccount =
                self.load_sub_account_keys_for_mapping(sub_account_keys_path)?;
            // Map UID 0 → "flipster" (matches our dummy files).
            self.sub_account_uid_to_login_name
                .insert(0, "flipster".to_string());
            // Empty main-account maps — Flipster is cookie-auth, no master-sub hierarchy.
            return Ok(());
        }

        // Python: self.sub_account_keys = self._load_sub_account_keys()
        self.load_sub_account_keys(sub_account_keys_path)?;

        // Python: self.sub_account_uid_to_subaccount = self._load_sub_account_keys() (같은 메서드를 두 번 호출)
        self.sub_account_uid_to_subaccount =
            self.load_sub_account_keys_for_mapping(sub_account_keys_path)?;

        // Python: self.main_account_uid_to_secrets = self._load_main_account_secrets()
        self.main_account_uid_to_secrets =
            self.load_main_account_secrets(main_account_secrets_path)?;

        // Python: self.main_account_uid_to_subaccounts = self._load_main_account_uid_to_subaccounts()
        self.load_main_account_uid_to_subaccounts()?;

        // Python: self.sub_account_uid_to_login_name = self._load_sub_account_uid_to_login_name()
        self.load_sub_account_uid_to_login_name();

        Ok(())
    }

    fn load_sub_account_keys_for_mapping(&self, path: &str) -> Result<HashMap<i32, SubAccountKey>> {
        // Python의 _load_sub_account_keys()와 동일 (두 번째 호출용)
        if !std::path::Path::new(path).exists() {
            return Err(anyhow::anyhow!("Subaccount keys file not found: {}", path));
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read subaccount keys file: {}", path))?;

        let json: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse subaccount keys JSON: {}", path))?;

        let mut result = HashMap::new();
        for (_, value) in json.as_object().unwrap() {
            if let Ok(subaccount) = serde_json::from_value::<SubAccountKey>(value.clone()) {
                // Only load if current IP is in whitelist
                if subaccount.ip_whitelist.contains(&self.current_ip) {
                    result.insert(subaccount.user_id, subaccount);
                }
            }
        }

        Ok(result)
    }

    fn load_main_account_uid_to_subaccounts(&mut self) -> Result<()> {
        // Python: _load_main_account_uid_to_subaccounts()
        // Python: subaccounts = self._load_subaccounts_from_main_account(main_account_uid)
        // Python은 에러 시 빈 리스트를 반환하므로 Result가 아닌 Vec를 반환
        for (main_uid, secret) in &self.main_account_uid_to_secrets {
            let subaccounts = self.load_subaccounts_from_main_account(*main_uid, secret)?;
            self.main_account_uid_to_subaccounts
                .insert(*main_uid, subaccounts);
        }
        Ok(())
    }

    fn load_sub_account_uid_to_login_name(&mut self) {
        // Python: _load_sub_account_uid_to_login_name()
        for (_, subaccounts) in &self.main_account_uid_to_subaccounts {
            for subaccount in subaccounts {
                self.sub_account_uid_to_login_name
                    .insert(subaccount.user_id, subaccount.login_name.clone());
            }
        }
    }

    fn load_sub_account_keys(&mut self, path: &str) -> Result<()> {
        if !std::path::Path::new(path).exists() {
            return Err(anyhow::anyhow!("Subaccount keys file not found: {}", path));
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read subaccount keys file: {}", path))?;

        let json: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse subaccount keys JSON: {}", path))?;

        for (_, value) in json.as_object().unwrap() {
            if let Ok(subaccount) = serde_json::from_value::<SubAccountKey>(value.clone()) {
                // Only load if current IP is in whitelist
                if subaccount.ip_whitelist.contains(&self.current_ip) {
                    self.sub_account_keys.insert(subaccount.user_id, subaccount);
                }
            }
        }

        Ok(())
    }

    fn load_main_account_secrets(&mut self, path: &str) -> Result<HashMap<i32, MainAccountSecret>> {
        if !std::path::Path::new(path).exists() {
            return Err(anyhow::anyhow!(
                "Main account secrets file not found: {}",
                path
            ));
        }

        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read main account secrets file: {}", path))?;

        let json: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse main account secrets JSON: {}", path))?;

        let mut secrets = HashMap::new();
        for (_, value) in json.as_object().unwrap() {
            if let Ok(secret) = serde_json::from_value::<MainAccountSecret>(value.clone()) {
                secrets.insert(secret.uid, secret);
            }
        }

        Ok(secrets)
    }

    fn load_subaccounts_from_main_account(
        &self,
        main_account_uid: i32,
        secret: &MainAccountSecret,
    ) -> Result<Vec<GateSubAccount>> {
        // Python: try-except로 에러 시 빈 리스트 반환
        match self._load_subaccounts_from_main_account_internal(main_account_uid, secret) {
            Ok(subaccounts) => Ok(subaccounts),
            Err(e) => {
                elog!("Main account: {} error: {}", main_account_uid, e);
                Ok(Vec::new()) // Python과 동일: 빈 리스트 반환
            }
        }
    }

    fn _load_subaccounts_from_main_account_internal(
        &self,
        main_account_uid: i32,
        secret: &MainAccountSecret,
    ) -> Result<Vec<GateSubAccount>> {
        let url = format!("{}/sub_accounts", GATE_API_BASE);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();

        // Gate.io API v4 서명 형식: method\nurl\nquery_string\nhashed_payload\ntimestamp
        // Python gate_api와 동일한 형식 사용
        let method = "GET";
        let path = "/api/v4/sub_accounts";
        let query_string = ""; // GET 요청에 query string 없음
        let payload_string = ""; // GET 요청에 payload 없음

        // Payload의 SHA512 해시 계산 (Python gate_api와 동일)
        use sha2::Digest;
        let mut hasher = Sha512::new();
        hasher.update(payload_string.as_bytes());
        let hashed_payload = hex::encode(hasher.finalize());

        // 서명 문자열 생성: method\npath\nquery_string\nhashed_payload\ntimestamp
        let signature_string = format!(
            "{}\n{}\n{}\n{}\n{}",
            method, path, query_string, hashed_payload, timestamp
        );

        // HMAC-SHA512 서명 생성
        let mut mac = Hmac::<Sha512>::new_from_slice(secret.secret.as_bytes())
            .map_err(|_| anyhow::anyhow!("Failed to create HMAC"))?;
        mac.update(signature_string.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());

        let response = self
            .client
            .get(&url)
            .header("KEY", &secret.api_key)
            .header("Timestamp", &timestamp)
            .header("SIGN", &signature)
            .send()
            .with_context(|| {
                format!(
                    "Failed to fetch subaccounts for main account {}",
                    main_account_uid
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response
                .text()
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(anyhow::anyhow!("Gate API error ({}): {}", status, text));
        }

        let subaccounts: Vec<GateSubAccount> = response
            .json()
            .with_context(|| "Failed to parse subaccounts response")?;

        Ok(subaccounts)
    }

    pub fn get_sub_account_uid_from_login_name(&self, login_name: &str) -> Option<i32> {
        self.sub_account_uid_to_login_name
            .iter()
            .find(|(_, name)| name == &login_name)
            .map(|(uid, _)| *uid)
    }

    pub fn get_sub_account_key_from_login_name(&self, login_name: &str) -> Option<SubAccountKey> {
        // Python: return self.sub_account_uid_to_subaccount.get(subaccount_uid, None)
        let uid = self.get_sub_account_uid_from_login_name(login_name)?;
        self.sub_account_uid_to_subaccount.get(&uid).cloned()
    }

    // Python: get_sub_account_key_from_subaccount_uid
    pub fn get_sub_account_key_from_subaccount_uid(
        &self,
        subaccount_uid: i32,
    ) -> Option<SubAccountKey> {
        // Python: return self.sub_account_uid_to_subaccount.get(subaccount_uid, None)
        self.sub_account_uid_to_subaccount
            .get(&subaccount_uid)
            .cloned()
    }

    // Python: get_login_name_from_subaccount_uid
    pub fn get_login_name_from_subaccount_uid(&self, subaccount_uid: i32) -> Option<String> {
        self.sub_account_uid_to_login_name
            .get(&subaccount_uid)
            .cloned()
    }

    // Python: get_main_account_secret_from_subaccount_login_name
    pub fn get_main_account_secret_from_subaccount_login_name(
        &self,
        login_name: &str,
    ) -> Option<MainAccountSecret> {
        for (main_account_uid, subaccounts) in &self.main_account_uid_to_subaccounts {
            for subaccount in subaccounts {
                if subaccount.login_name == login_name {
                    return self
                        .main_account_uid_to_secrets
                        .get(main_account_uid)
                        .cloned();
                }
            }
        }
        None
    }

    // Python: get_main_account_secret_from_subaccount_uid
    pub fn get_main_account_secret_from_subaccount_uid(
        &self,
        subaccount_uid: i32,
    ) -> Option<MainAccountSecret> {
        let login_name = self.get_login_name_from_subaccount_uid(subaccount_uid)?;
        self.get_main_account_secret_from_subaccount_login_name(&login_name)
    }

    // Python: get_main_account_secret_from_main_account_uid
    pub fn get_main_account_secret_from_main_account_uid(
        &self,
        main_account_uid: i32,
    ) -> Option<MainAccountSecret> {
        self.main_account_uid_to_secrets
            .get(&main_account_uid)
            .cloned()
    }
}

use crate::config::Config;
use crate::error::{Error, Result};
use crate::flipster::auth;
use crate::flipster::models::*;
use reqwest::Client;

pub struct FlipsterRestClient {
    client: Client,
    config: Config,
}

impl FlipsterRestClient {
    pub fn new(config: Config) -> Self {
        Self {
            client: Client::new(),
            config,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    // -- internal helpers ---------------------------------------------------

    async fn get_public(&self, path: &str) -> Result<serde_json::Value> {
        let url = format!("{}{}", self.config.base_url, path);
        let resp = self.client.get(&url).send().await?;
        Self::handle_response(resp).await
    }

    async fn get_authenticated(&self, path: &str) -> Result<serde_json::Value> {
        let url = format!("{}{}", self.config.base_url, path);
        let expires = auth::make_expires(60);
        let signature = auth::sign(&self.config.api_secret, "GET", path, expires, None);

        let resp = self
            .client
            .get(&url)
            .header("api-key", &self.config.api_key)
            .header("api-expires", expires.to_string())
            .header("api-signature", &signature)
            .send()
            .await?;

        Self::handle_response(resp).await
    }

    async fn handle_response(resp: reqwest::Response) -> Result<serde_json::Value> {
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

    /// Deserialise a JSON value that may be a single object or an array.
    fn normalise_to_vec<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<Vec<T>> {
        match value {
            serde_json::Value::Array(_) => Ok(serde_json::from_value(value)?),
            serde_json::Value::Object(_) => {
                let item: T = serde_json::from_value(value)?;
                Ok(vec![item])
            }
            _ => Err(Error::Other("unexpected JSON shape".into())),
        }
    }

    // -- public API ---------------------------------------------------------

    /// `GET /api/v1/public/time` — returns server time in nanoseconds.
    pub async fn get_server_time(&self) -> Result<i64> {
        let value = self.get_public("/api/v1/public/time").await?;
        let st: ServerTime = serde_json::from_value(value)?;
        st.server_time
            .parse::<i64>()
            .map_err(|e| Error::Other(format!("parse server time: {e}")))
    }

    /// `GET /api/v1/market/contract` — all contracts (or single if symbol given).
    pub async fn get_contracts(&self, symbol: Option<&str>) -> Result<Vec<Contract>> {
        let path = match symbol {
            Some(s) => format!("/api/v1/market/contract?symbol={s}"),
            None => "/api/v1/market/contract".into(),
        };
        let value = self.get_authenticated(&path).await?;
        Self::normalise_to_vec(value)
    }

    /// `GET /api/v1/market/ticker` — tickers for all or a single symbol.
    pub async fn get_tickers(&self, symbol: Option<&str>) -> Result<Vec<Ticker>> {
        let path = match symbol {
            Some(s) => format!("/api/v1/market/ticker?symbol={s}"),
            None => "/api/v1/market/ticker".into(),
        };
        let value = self.get_authenticated(&path).await?;
        Self::normalise_to_vec(value)
    }
}

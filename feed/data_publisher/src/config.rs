use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    pub api_secret: String,
    pub base_url: String,
    pub ws_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();
        Ok(Self {
            api_key: std::env::var("FLIPSTER_API_KEY")
                .map_err(|_| Error::Config("FLIPSTER_API_KEY not set".into()))?,
            api_secret: std::env::var("FLIPSTER_API_SECRET")
                .map_err(|_| Error::Config("FLIPSTER_API_SECRET not set".into()))?,
            base_url: std::env::var("FLIPSTER_BASE_URL")
                .unwrap_or_else(|_| "https://trading-api.flipster.io".into()),
            ws_url: std::env::var("FLIPSTER_WS_URL")
                .unwrap_or_else(|_| "wss://trading-api.flipster.io/api/v1/stream".into()),
        })
    }
}

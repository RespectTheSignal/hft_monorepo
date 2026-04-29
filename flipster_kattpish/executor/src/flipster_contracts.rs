//! Flipster instrument specs (max leverage, tick size, notional limits).
//!
//! Sourced from `https://api.flipster.io/api/v2/snapshot/specs` which the
//! web app itself queries on every page load. Authenticates via the same
//! cookie set the rest of the executor uses. Refreshed once at startup —
//! these values change rarely.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct InstrumentSpec {
    pub max_leverage: u32,
    pub tick_size: f64,
    pub notional_max_order: f64,
    pub notional_max_position: f64,
}

impl Default for InstrumentSpec {
    fn default() -> Self {
        Self {
            max_leverage: 10,
            tick_size: 0.0,
            notional_max_order: f64::INFINITY,
            notional_max_position: f64::INFINITY,
        }
    }
}

#[derive(Debug, Default)]
pub struct ContractMap {
    map: HashMap<String, InstrumentSpec>,
}

impl ContractMap {
    pub fn get(&self, symbol: &str) -> InstrumentSpec {
        self.map.get(symbol).cloned().unwrap_or_default()
    }

    pub fn max_leverage(&self, symbol: &str) -> u32 {
        self.get(symbol).max_leverage
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
}

#[derive(Deserialize)]
struct Wrapper {
    t: Wrapper2,
}
#[derive(Deserialize)]
struct Wrapper2 {
    instruments: Snap,
}
#[derive(Deserialize)]
struct Snap {
    s: HashMap<String, RawInstrument>,
}
#[derive(Deserialize)]
#[allow(non_snake_case)]
struct RawInstrument {
    #[serde(default)]
    initMarginRate: Option<String>,
    #[serde(default)]
    tickSize: Option<String>,
    #[serde(default)]
    notionalMaxOrderAmount: Option<String>,
    #[serde(default)]
    notionalMaxPosition: Option<String>,
}

pub async fn load(http: &reqwest::Client, cookie_hdr: &str) -> Result<ContractMap> {
    let resp = http
        .get("https://api.flipster.io/api/v2/snapshot/specs")
        .header(
            "User-Agent",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36",
        )
        .header("Origin", "https://flipster.io")
        .header("Cookie", cookie_hdr)
        .send()
        .await
        .context("flipster_contracts: GET specs")?;
    let status = resp.status();
    let text = resp.text().await.context("flipster_contracts: read body")?;
    if !status.is_success() {
        anyhow::bail!("flipster_contracts: status {} body {}", status, text.chars().take(200).collect::<String>());
    }
    let parsed: Wrapper = serde_json::from_str(&text)
        .context("flipster_contracts: parse JSON")?;
    let mut map = HashMap::new();
    for (sym, raw) in parsed.t.instruments.s {
        let imr = raw
            .initMarginRate
            .as_ref()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.1);
        let max_leverage = if imr > 0.0 {
            (1.0 / imr).floor().max(1.0) as u32
        } else {
            10
        };
        let tick_size = raw
            .tickSize
            .as_ref()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let notional_max_order = raw
            .notionalMaxOrderAmount
            .as_ref()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(f64::INFINITY);
        let notional_max_position = raw
            .notionalMaxPosition
            .as_ref()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(f64::INFINITY);
        map.insert(
            sym,
            InstrumentSpec {
                max_leverage,
                tick_size,
                notional_max_order,
                notional_max_position,
            },
        );
    }
    Ok(ContractMap { map })
}

//! Gate contract specs — multipliers and minimum order sizes. Loaded once
//! at startup from the public futures API.

use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Copy)]
pub struct ContractSpec {
    pub multiplier: f64,
    pub order_size_min: i64,
}

pub type ContractMap = Arc<HashMap<String, ContractSpec>>;

/// Fetch the contract spec table from Gate's public REST API.
/// Failure returns an empty map; the executor falls back to multiplier=1
/// and min=1 for unknown contracts (same as Python).
pub async fn load(http: &reqwest::Client) -> ContractMap {
    const URL: &str = "https://api.gateio.ws/api/v4/futures/usdt/contracts";
    let mut out: HashMap<String, ContractSpec> = HashMap::new();
    let resp = match http.get(URL).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "[init] gate contracts fetch failed");
            return Arc::new(out);
        }
    };
    let arr: Vec<serde_json::Value> = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "[init] gate contracts JSON parse failed");
            return Arc::new(out);
        }
    };
    for c in arr {
        let Some(name) = c.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let multiplier = c
            .get("quanto_multiplier")
            .and_then(parse_f64)
            .unwrap_or(1.0);
        let order_size_min = c
            .get("order_size_min")
            .and_then(parse_i64)
            .unwrap_or(1);
        out.insert(
            name.to_string(),
            ContractSpec {
                multiplier,
                order_size_min,
            },
        );
    }
    tracing::info!(count = out.len(), "[init] gate contracts loaded");
    Arc::new(out)
}

fn parse_f64(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
}
fn parse_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
}

/// USD notional → number of Gate contracts (signed unspecified — caller
/// derives sign from side). Mirrors the Python `gate_size_from_usd`.
pub fn size_from_usd(specs: &ContractMap, contract: &str, usd: f64, price: f64) -> i64 {
    let spec = match specs.get(contract) {
        Some(s) => *s,
        None => {
            return 1;
        }
    };
    if price <= 0.0 {
        return spec.order_size_min;
    }
    let notional_per_contract = price * spec.multiplier;
    if notional_per_contract <= 0.0 {
        return spec.order_size_min;
    }
    let contracts = (usd / notional_per_contract).round() as i64;
    contracts.max(spec.order_size_min)
}

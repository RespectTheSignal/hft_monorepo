// Build synthetic GateContract map for Flipster symbols.
//
// Reads scripts/symbols_v9.json (shared with Python bot) and produces gate_hft's
// GateContract shape so downstream code works unchanged.
//
// Position size scaling: quanto_multiplier = 1 / POSITION_SIZE_SCALE.

use crate::flipster_private_ws::POSITION_QUANTO_MULTIPLIER;
use crate::gate_order_manager::GateContract;
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Deserialize)]
struct SymbolEntry {
    base: String,
    #[serde(default = "default_max_leverage")]
    max_leverage: i32,
    #[serde(default)]
    tick: serde_json::Value,
}

fn default_max_leverage() -> i32 {
    10
}

pub fn symbols_v9_path() -> PathBuf {
    symbols_path()
}

fn symbols_path() -> PathBuf {
    if let Ok(p) = env::var("SYMBOLS_JSON") {
        return PathBuf::from(p);
    }
    // try several common locations relative to CARGO_MANIFEST_DIR
    let mani = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for rel in ["../scripts/symbols_v9.json", "scripts/symbols_v9.json"] {
        let p = mani.join(rel);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("scripts/symbols_v9.json")
}

pub fn load_into(contracts: &ArcSwap<HashMap<String, GateContract>>) -> Result<()> {
    let path = symbols_path();
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let entries: Vec<SymbolEntry> =
        serde_json::from_str(&data).context("parse symbols_v9.json")?;

    let mut map = HashMap::with_capacity(entries.len());
    for e in entries {
        let name = format!("{}USDT.PERP", e.base);
        // tick can be number or string; normalize to string.
        let tick_str = match &e.tick {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            _ => "0.0000000001".to_string(),
        };
        let c = GateContract {
            name: name.clone(),
            contract_type: "direct".to_string(),
            quanto_multiplier: format!("{:.10}", POSITION_QUANTO_MULTIPLIER),
            leverage_min: "1".to_string(),
            leverage_max: e.max_leverage.to_string(),
            mark_price: "0".to_string(),
            funding_rate: "0".to_string(),
            order_size_min: 1,
            order_size_max: 1_000_000_000,
            order_price_round: tick_str,
        };
        map.insert(name, c);
    }
    contracts.store(Arc::new(map));
    Ok(())
}

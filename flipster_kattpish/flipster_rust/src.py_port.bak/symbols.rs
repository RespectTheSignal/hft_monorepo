// Load symbol universe from scripts/symbols_v9.json (shared with Python bot).

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct SymbolMeta {
    pub base: String,
    #[serde(default = "default_max_leverage")]
    pub max_leverage: i32,
    /// Tick size — symbols_v9.json stores as JSON number (e.g. 0.1), accept both.
    #[serde(default, deserialize_with = "de_tick")]
    pub tick: Option<f64>,
}

fn default_max_leverage() -> i32 {
    10
}

fn de_tick<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<f64>, D::Error> {
    use serde::Deserialize;
    let v = serde_json::Value::deserialize(de)?;
    Ok(match v {
        serde_json::Value::Null => None,
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    })
}

#[derive(Debug, Clone)]
pub struct SymbolRegistry {
    /// BASE -> meta (e.g. "BTC" -> ...)
    by_base: HashMap<String, SymbolMeta>,
}

impl SymbolRegistry {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let data = fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        let list: Vec<SymbolMeta> = serde_json::from_str(&data).context("parse symbols_v9.json")?;
        let mut by_base = HashMap::with_capacity(list.len());
        for m in list {
            by_base.insert(m.base.clone(), m);
        }
        Ok(Self { by_base })
    }

    pub fn perp_symbols(&self) -> Vec<String> {
        self.by_base
            .keys()
            .map(|b| format!("{}USDT.PERP", b))
            .collect()
    }

    pub fn max_leverage(&self, perp_symbol: &str) -> i32 {
        let base = base_of(perp_symbol);
        self.by_base
            .get(base)
            .map(|m| m.max_leverage)
            .unwrap_or(10)
    }

    pub fn tick(&self, perp_symbol: &str) -> Option<f64> {
        let base = base_of(perp_symbol);
        self.by_base.get(base).and_then(|m| m.tick)
    }

    pub fn len(&self) -> usize {
        self.by_base.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_base.is_empty()
    }
}

/// "BTCUSDT.PERP" -> "BTC"
pub fn base_of(perp_symbol: &str) -> &str {
    let s = perp_symbol.trim_end_matches(".PERP");
    s.trim_end_matches("USDT")
}

/// Round to the nearest valid tick. direction: "up" (ceil), "down" (floor), else nearest.
pub fn tick_round(tick: Option<f64>, price: f64, direction: &str) -> Result<f64> {
    let t = match tick {
        Some(t) if t > 0.0 => t,
        _ => return Err(anyhow!("missing or invalid tick")),
    };
    let n = price / t;
    let rounded = match direction {
        "up" => n.ceil(),
        "down" => n.floor(),
        _ => n.round(),
    };
    Ok(rounded * t)
}

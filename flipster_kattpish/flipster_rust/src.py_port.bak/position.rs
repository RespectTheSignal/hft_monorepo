// Position / Order state mirroring Python WSPositionTracker.

use serde_json::Value;
use std::collections::HashMap;

/// One open position (key = "SYMBOL/SLOT" e.g. "BTCUSDT.PERP/0").
/// Fields come from `private/positions` WS topic as string-typed JSON values.
#[derive(Clone, Debug, Default)]
pub struct FlipsterPosition {
    /// Raw per-field map (avgEntryPrice, position, initMarginReserved, …).
    pub fields: HashMap<String, Value>,
}

impl FlipsterPosition {
    pub fn update(&mut self, other: &serde_json::Map<String, Value>) {
        for (k, v) in other {
            self.fields.insert(k.clone(), v.clone());
        }
    }

    pub fn init_margin_reserved(&self) -> f64 {
        parse_str_field(self.fields.get("initMarginReserved")).unwrap_or(0.0)
    }

    pub fn position_qty(&self) -> Option<f64> {
        parse_str_field(self.fields.get("position"))
    }

    pub fn avg_entry_price(&self) -> Option<f64> {
        parse_str_field(self.fields.get("avgEntryPrice"))
    }

    /// ("Long"|"Short", abs_qty, entry_price); None if no position.
    pub fn side_qty_entry(&self) -> Option<(&'static str, f64, f64)> {
        let qty = self.position_qty()?;
        let ep = self.avg_entry_price()?;
        if qty == 0.0 {
            return None;
        }
        let side = if qty > 0.0 { "Long" } else { "Short" };
        Some((side, qty.abs(), ep))
    }

    pub fn is_open(&self) -> bool {
        self.init_margin_reserved() > 0.0
    }
}

/// One resting/open order (key = orderId).
#[derive(Clone, Debug, Default)]
pub struct FlipsterOrder {
    pub fields: HashMap<String, Value>,
}

impl FlipsterOrder {
    pub fn update(&mut self, other: &serde_json::Map<String, Value>) {
        for (k, v) in other {
            self.fields.insert(k.clone(), v.clone());
        }
    }

    pub fn symbol(&self) -> Option<String> {
        self.fields
            .get("symbol")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    pub fn price(&self) -> Option<f64> {
        parse_str_field(self.fields.get("price"))
    }

    pub fn amount(&self) -> Option<f64> {
        parse_str_field(self.fields.get("amount"))
    }

    pub fn status(&self) -> Option<String> {
        self.fields
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    pub fn side(&self) -> Option<String> {
        self.fields
            .get("side")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }
}

/// Flipster WS sends numbers as JSON strings. Parse tolerantly.
fn parse_str_field(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        return s.parse().ok();
    }
    None
}

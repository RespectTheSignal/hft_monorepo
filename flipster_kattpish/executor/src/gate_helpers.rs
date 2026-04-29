//! Gate response unpacking — mirrors Python `_extract_gate_fill`.

use gate_client::OrderResponse;

/// (fill_price, abs_filled_size) from a Gate order response.
/// `fill_price <= 0` means not filled regardless of size.
pub fn extract_fill(resp: &OrderResponse) -> (f64, i64) {
    // Gate JSON is `{ ok, status, data: { code, message, data: {...order...} } }`.
    let inner = resp.data.get("data").and_then(|v| v.as_object());
    let Some(inner) = inner else {
        return (0.0, 0);
    };
    let fp = inner
        .get("fill_price")
        .and_then(value_as_f64)
        .unwrap_or(0.0);
    let order_size = inner
        .get("size")
        .and_then(value_as_i64)
        .map(|n| n.unsigned_abs() as i64)
        .unwrap_or(0);
    let left = inner
        .get("left")
        .and_then(value_as_i64)
        .map(|n| n.unsigned_abs() as i64)
        .unwrap_or(0);
    let filled = (order_size - left).max(0);
    (fp, filled)
}

fn value_as_f64(v: &serde_json::Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
}

fn value_as_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
}

/// Order id (`id_string` or `id`) from a Gate response, if any.
pub fn extract_order_id(resp: &OrderResponse) -> Option<String> {
    let inner = resp.data.get("data")?;
    inner
        .get("id_string")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            inner
                .get("id")
                .and_then(|v| v.as_i64())
                .map(|n| n.to_string())
        })
}

/// `status` field from Gate order response (e.g. "open", "finished").
pub fn extract_status(resp: &OrderResponse) -> &str {
    resp.data
        .get("data")
        .and_then(|v| v.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

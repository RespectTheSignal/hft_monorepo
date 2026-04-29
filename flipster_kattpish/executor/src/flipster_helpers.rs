//! Helpers that orchestrate multiple flipster-client calls with WS state
//! checks. Keeps the entry/exit code in `executor.rs` linear.

use serde_json::Value;

use crate::flipster_ws::SharedState;
use flipster_client::FlipsterClient;

/// (avg_price, size, slot) extracted from a Flipster open/close response.
/// Mirrors Python's `_extract_flipster_fill`.
pub fn extract_fill(resp: &Value) -> (f64, f64, Option<u32>) {
    let pos = resp.get("position").unwrap_or(&Value::Null);
    let order = resp.get("order").unwrap_or(&Value::Null);
    let avg = first_f64(&[pos.get("avgPrice"), order.get("avgPrice")]).unwrap_or(0.0);
    let size = first_f64(&[pos.get("size"), order.get("size")])
        .map(|s| s.abs())
        .unwrap_or(0.0);
    let slot = first_u32(&[pos.get("slot"), order.get("slot")]);
    (avg, size, slot)
}

fn first_f64(opts: &[Option<&Value>]) -> Option<f64> {
    for o in opts {
        if let Some(v) = o {
            if let Some(n) = v.as_f64() {
                return Some(n);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.parse::<f64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn first_u32(opts: &[Option<&Value>]) -> Option<u32> {
    for o in opts {
        if let Some(v) = o {
            if let Some(n) = v.as_u64() {
                return Some(n as u32);
            }
            if let Some(s) = v.as_str() {
                if let Ok(n) = s.parse::<u32>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Manual LIMIT-IOC entry: place LIMIT at `signal_price`; if not crossed
/// immediately, wait `wait_s` seconds then cancel + verify final state via
/// the WS-backed position-size check. Returns `(fill_response, order_id)`:
///   - fill_response: Some(json) if we have a position; None for clean abort.
///   - order_id: present if we placed an order (may be None on synthesis).
///
/// Mirrors Python `flipster_limit_ioc_entry`.
pub async fn limit_ioc_entry(
    fc: &FlipsterClient,
    state: &SharedState,
    symbol: &str,
    side: &str,
    amount_usd: f64,
    signal_price: f64,
    leverage: u32,
    wait_s: f64,
    margin: &str,
) -> anyhow::Result<(Option<Value>, Option<String>)> {
    // Flipster validates LIMIT prices tightly against current market
    // ("InvalidPrice" if too far off). Pass the signal price as-is —
    // pipeline lag is small, and crossing happens via the order's TIF.
    let limit_price = signal_price;
    let resp = fc
        .place_order_oneway_with_margin(
            symbol,
            side,
            amount_usd,
            limit_price,
            leverage,
            false,
            "ORDER_TYPE_LIMIT",
            false, // postOnly: NO — we want IOC-like behavior (cross if possible).
            margin,
        )
        .await?;

    let (avg, sz, _slot) = extract_fill(&resp);
    if avg > 0.0 && sz > 0.0 {
        return Ok((Some(resp), None));
    }

    let order_id = resp
        .get("order")
        .and_then(|o| o.get("orderId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let Some(order_id) = order_id else {
        return Ok((None, None));
    };

    tokio::time::sleep(std::time::Duration::from_secs_f64(wait_s)).await;

    // Cancel + verify (3 attempts), checking WS to confirm the order is
    // really gone. Without this Python had silent-cancel orphans because
    // the DELETE without body was rejected silently.
    let mut cancel_done = false;
    for attempt in 0..3 {
        if let Err(e) = fc.cancel_order(symbol, &order_id).await {
            tracing::warn!(
                attempt = attempt + 1,
                error = %e,
                "[FLIPSTER-LIMIT] cancel attempt err"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let still_open = state.read().await.order_open(&order_id);
        if !still_open {
            cancel_done = true;
            break;
        }
        tracing::warn!(
            order_id = %order_id,
            attempt = attempt + 1,
            "[FLIPSTER-LIMIT] still open after cancel; retrying"
        );
    }
    if !cancel_done {
        tracing::error!(
            order_id = %order_id,
            "[FLIPSTER-LIMIT] !!! order still OPEN after 3 cancel attempts — manual intervention may be needed !!!"
        );
    }

    // Position verification with margin-aware fallback. WS sometimes
    // broadcasts ``initMarginReserved`` before the ``position`` size
    // field, AND sometimes lags behind server fill by 100-500ms+ on
    // bursty days. Poll has_position OR position_size up to 2s
    // unconditionally — a missed detection here orphans the position.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let s = state.read().await;
        if s.position_size(symbol, 0).abs() > 0.0 || s.has_position(symbol, 0) {
            drop(s);
            // Once any signal arrives, poll size for up to 1s more
            let inner_deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(1);
            loop {
                if state.read().await.position_size(symbol, 0).abs() > 0.0 {
                    break;
                }
                if std::time::Instant::now() >= inner_deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            break;
        }
        drop(s);
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let sz = state.read().await.position_size(symbol, 0).abs();
    if sz > 0.0 {
        let synthetic = serde_json::json!({
            "position": {
                "symbol": symbol,
                "size": sz,
                "avgPrice": signal_price,
                "slot": 0,
            },
            "order": resp.get("order").cloned().unwrap_or(Value::Null),
        });
        return Ok((Some(synthetic), Some(order_id)));
    }

    // Margin-only fallback: WS shows margin reserved but never delivered
    // the size field even after polling. Treat as filled, approximate
    // size from notional. Better to enter Gate hedge than leave Flipster
    // naked.
    if state.read().await.has_position(symbol, 0) {
        let approx = if signal_price > 0.0 {
            amount_usd / signal_price
        } else {
            0.0
        };
        let synthetic = serde_json::json!({
            "position": {
                "symbol": symbol,
                "size": approx,
                "avgPrice": signal_price,
                "slot": 0,
            },
            "order": resp.get("order").cloned().unwrap_or(Value::Null),
        });
        tracing::warn!(
            "[FLIPSTER-LIMIT] margin-only fallback (size unknown) — treating as filled"
        );
        return Ok((Some(synthetic), Some(order_id)));
    }

    Ok((None, Some(order_id)))
}

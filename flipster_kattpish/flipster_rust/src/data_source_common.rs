// Shared message processing for IPC and ZMQ data sources (bookticker/trade -> cache)

use crate::data_cache::DataCache;
use crate::types::{BookTickerC, TradeC};
use crate::elog;
use anyhow::Result;
use std::sync::atomic::{AtomicU64, Ordering};

/// Parse topic bytes "exchange_data_type_symbol" into (&str, &str, &str).
#[inline(always)]
pub fn parse_topic_parts(topic: &[u8]) -> Option<(&str, &str, &str)> {
    let i1 = topic.iter().position(|&b| b == b'_')?;
    let rest = &topic[i1 + 1..];
    let i2_rel = rest.iter().position(|&b| b == b'_')?;
    let i2 = i1 + 1 + i2_rel;
    let exchange = std::str::from_utf8(&topic[..i1]).ok()?;
    let data_type = std::str::from_utf8(&topic[i1 + 1..i2]).ok()?;
    let symbol = std::str::from_utf8(&topic[i2 + 1..]).ok()?;
    Some((exchange, data_type, symbol))
}

static MESSAGE_COUNT: AtomicU64 = AtomicU64::new(0);
/// Last time we logged the "Received N messages" line (ms). Throttle to at most once per second.
static LAST_DATA_CLIENT_LOG_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Process one (topic, payload) message and update cache. Used by both IPC and ZMQ clients.
pub fn process_data_message(
    topic_bytes: &[u8],
    payload_bytes: &[u8],
    received_at_ms: i64,
    cache: &DataCache,
) -> Result<()> {
    let count = MESSAGE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;

    let (exchange, data_type, symbol) = match parse_topic_parts(topic_bytes) {
        Some(parts) => parts,
        None => return Ok(()),
    };

    if count % 10000 == 0 {
        let last = LAST_DATA_CLIENT_LOG_MS.load(Ordering::Relaxed);
        if received_at_ms.saturating_sub(last) >= 1000 {
            LAST_DATA_CLIENT_LOG_MS.store(received_at_ms, Ordering::Relaxed);
            elog!(
                "[DataClient] Received {} messages, latest topic: {}_{}_{}",
                count,
                exchange,
                data_type,
                symbol
            );
        }
    }

    match data_type {
        "bookticker" => {
            if let Some(ticker) = BookTickerC::from_bytes(payload_bytes) {
                cache.update_bookticker(exchange, symbol, ticker, received_at_ms);
            } else {
                elog!("[DataClient] Failed to parse BookTickerC for {}", symbol);
            }
        }
        "webbookticker" => {
            if let Some(ticker) = BookTickerC::from_bytes(payload_bytes) {
                cache.update_webbookticker(symbol, ticker, received_at_ms);
            } else {
                elog!(
                    "[DataClient] Failed to parse BookTickerC (web) for {}",
                    symbol
                );
            }
        }
        "trade" => {
            if let Some(trade) = TradeC::from_bytes(payload_bytes) {
                cache.update_trade(symbol, trade, received_at_ms);
            } else {
                elog!("[DataClient] Failed to parse TradeC for {}", symbol);
            }
        }
        _ => {}
    }

    Ok(())
}

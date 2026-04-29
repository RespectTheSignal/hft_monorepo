//! ZMQ direct subscriber for upstream data publishers.
//!
//! Bypasses the QuestDB polling layer entirely by connecting directly to the
//! data_publisher's ZMQ PUB socket. This eliminates the ~100ms average poll
//! lag that the `questdb_reader` introduces, bringing live tick latency in
//! line with what the backtest replay produces.
//!
//! Two wire formats are supported:
//!
//! - **Standard 120B** (Binance, Gate, Bybit, Bitget, OKX): multipart frame
//!   `[topic][payload]` where payload is `<16s32s4d5q>`:
//!     exchange(16) | symbol(32) | bid|ask|bid_size|ask_size (f64 LE) |
//!     event_time | server_time | publisher_sent | subscriber_received |
//!     subscriber_dump (i64 LE)
//!
//! - **Flipster 104B**: multipart frame `[topic][payload]` where payload is
//!   `<32s5d4q>`:
//!     symbol(32) | bid|ask|last|mark|index (f64 LE) |
//!     server_ts_ns | publisher_recv | publisher_sent | subscriber_recv (i64 LE)
//!
//! The blocking ZMQ recv loop runs on a dedicated `tokio::task::spawn_blocking`
//! thread so it doesn't starve the runtime; parsed `BookTick`s are pushed
//! into the same broadcast channel the strategies already subscribe to.

use anyhow::{anyhow, Result};
use chrono::{TimeZone, Utc};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::model::{BookTick, ExchangeName};

const STANDARD_BT_SIZE: usize = 120;
const FLIPSTER_BT_SIZE: usize = 104;

/// Spawn a dedicated blocking thread that subscribes to a ZMQ PUB socket and
/// re-emits parsed bookticker frames into the broadcast channel.
///
/// Topic filter is empty (subscribe to everything). Frames whose payload
/// length doesn't match either supported format are silently dropped.
pub fn spawn(
    address: String,
    exchange: ExchangeName,
    tx: broadcast::Sender<BookTick>,
) {
    info!(
        address = %address,
        exchange = exchange.as_str(),
        "zmq_reader: connecting"
    );
    std::thread::Builder::new()
        .name(format!("zmq-{}", exchange.as_str()))
        .spawn(move || {
            if let Err(e) = run(&address, exchange, &tx) {
                warn!(
                    address = %address,
                    exchange = exchange.as_str(),
                    error = %e,
                    "zmq_reader: terminated"
                );
            }
        })
        .expect("spawn zmq reader thread");
}

fn run(
    address: &str,
    exchange: ExchangeName,
    tx: &broadcast::Sender<BookTick>,
) -> Result<()> {
    let ctx = zmq::Context::new();
    let socket = ctx.socket(zmq::SUB)?;
    // Match strategy_flipster_python's tuning: HWM 100k, RCVBUF 4MiB.
    socket.set_rcvhwm(100_000)?;
    socket.set_subscribe(b"")?;
    socket.connect(address)?;
    info!(address = %address, exchange = exchange.as_str(), "zmq_reader: connected");

    let mut count: u64 = 0;
    let mut last_log = std::time::Instant::now();
    loop {
        let parts = socket.recv_multipart(0)?;
        if parts.len() != 2 {
            continue;
        }
        let payload = &parts[1];
        let parsed = match payload.len() {
            STANDARD_BT_SIZE => parse_standard(exchange, payload),
            FLIPSTER_BT_SIZE if matches!(exchange, ExchangeName::Flipster) => {
                parse_flipster(payload)
            }
            _ => continue,
        };
        if let Some(tick) = parsed {
            let _ = tx.send(tick);
            count += 1;
            if last_log.elapsed().as_secs() >= 30 {
                info!(
                    exchange = exchange.as_str(),
                    count,
                    "zmq_reader: cumulative recv"
                );
                last_log = std::time::Instant::now();
            }
        }
    }
}

fn read_f64_le(buf: &[u8], offset: usize) -> f64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[offset..offset + 8]);
    f64::from_le_bytes(bytes)
}

fn read_i64_le(buf: &[u8], offset: usize) -> i64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[offset..offset + 8]);
    i64::from_le_bytes(bytes)
}

fn null_trim(buf: &[u8]) -> &[u8] {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    &buf[..end]
}

/// Parse the 120B standard layout used by binance / gate / bybit / bitget / okx.
/// `exchange` is taken from the function argument rather than the embedded
/// 16-byte field — both match in practice and the arg avoids an extra
/// allocation per tick.
fn parse_standard(exchange: ExchangeName, p: &[u8]) -> Option<BookTick> {
    if p.len() != STANDARD_BT_SIZE {
        return None;
    }
    let raw_symbol = std::str::from_utf8(null_trim(&p[16..48])).ok()?;
    if raw_symbol.is_empty() {
        return None;
    }
    // Gate publishes `BTC_USDT`; the rest of the codebase expects `BTCUSDT`
    // (matches what exchanges/gate.rs and questdb_reader emit).
    let symbol = if matches!(exchange, ExchangeName::Gate) {
        raw_symbol.replace('_', "")
    } else {
        raw_symbol.to_string()
    };
    let bid = read_f64_le(p, 48);
    let ask = read_f64_le(p, 56);
    let bid_size = read_f64_le(p, 64);
    let ask_size = read_f64_le(p, 72);
    let event_time_ms = read_i64_le(p, 80);
    let ts = if event_time_ms > 0 {
        Utc.timestamp_millis_opt(event_time_ms).single()?
    } else {
        Utc::now()
    };
    Some(BookTick {
        exchange,
        symbol,
        bid_price: bid,
        ask_price: ask,
        bid_size,
        ask_size,
        last_price: None,
        mark_price: None,
        index_price: None,
        timestamp: ts,
    })
}

/// Parse the 104B Flipster-specific layout. Symbols arrive as `BTCUSDT.PERP`.
fn parse_flipster(p: &[u8]) -> Option<BookTick> {
    if p.len() != FLIPSTER_BT_SIZE {
        return None;
    }
    let symbol = std::str::from_utf8(null_trim(&p[0..32])).ok()?.to_string();
    if symbol.is_empty() {
        return None;
    }
    let bid = read_f64_le(p, 32);
    let ask = read_f64_le(p, 40);
    let last = read_f64_le(p, 48);
    let mark = read_f64_le(p, 56);
    let index = read_f64_le(p, 64);
    let server_ts_ns = read_i64_le(p, 72);
    let ts = if server_ts_ns > 0 {
        let secs = server_ts_ns / 1_000_000_000;
        let nsec = (server_ts_ns % 1_000_000_000) as u32;
        Utc.timestamp_opt(secs, nsec).single().unwrap_or_else(Utc::now)
    } else {
        Utc::now()
    };
    Some(BookTick {
        exchange: ExchangeName::Flipster,
        symbol,
        bid_price: bid,
        ask_price: ask,
        bid_size: 0.0,
        ask_size: 0.0,
        last_price: Some(last),
        mark_price: Some(mark),
        index_price: Some(index),
        timestamp: ts,
    })
}

/// Helper: convert a TCP address into a friendly log label (host:port).
#[allow(dead_code)]
pub fn label(address: &str) -> &str {
    address.trim_start_matches("tcp://")
}

#[allow(dead_code)]
fn anyhow_check<T>(opt: Option<T>, msg: &str) -> Result<T> {
    opt.ok_or_else(|| anyhow!(msg.to_string()))
}

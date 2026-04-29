//! Local Flipster feed consumer.
//!
//! Connects to `feed/data_subscriber`'s Unix domain socket
//! (default `/tmp/flipster_data_subscriber.sock`, override
//! `FLIPSTER_IPC_PATH`) and re-emits parsed bookticker frames into the
//! same broadcast channel the strategies subscribe to.
//!
//! Wire protocol (see `feed/data_subscriber/README.md`):
//!
//! 1. Register on connect: `REGISTER:<process_id>:<symbols_csv>\n`
//!    Empty symbol list = all symbols.
//! 2. Frames: `[4B topic_len LE][topic][4B payload_len LE][payload]`
//!    Topic example: `flipster_bookticker_BTC-USDT-PERP` (note dashes).
//!    Payload: 104B FlipsterBookTicker C-struct, little-endian
//!    (symbol[32] | bid | ask | last | mark | index | server_ts_ns |
//!     publisher_recv_ns | publisher_sent_ns | subscriber_recv_ns).
//!
//! Symbol normalization: feed emits `BTC-USDT-PERP`; rest of the
//! collector expects `BTCUSDT.PERP`. The reader normalizes on the way
//! in so downstream code (`pairs_core::base_of`, position keys, ILP
//! writes) stays untouched.
//!
//! Replaces the previous remote flipster zmq_reader connection
//! (`tcp://211.181.122.104:7000`) with a local IPC path — same instance
//! → IPC, per the deployment convention.
//!
//! Reconnects every 2 seconds on any I/O error (feed restart, slow
//! handshake, etc.). Non-fatal: collector continues, strategies idle on
//! Flipster ticks until the feed is back.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::model::{BookTick, ExchangeName};

const SOCK_DEFAULT: &str = "/tmp/flipster_data_subscriber.sock";
const REG_PROCESS_ID_DEFAULT: &str = "flipster_kattpish_collector";
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);
const PAYLOAD_SIZE: usize = 104;

pub fn spawn(tx: broadcast::Sender<BookTick>) {
    let path = std::env::var("FLIPSTER_IPC_PATH").unwrap_or_else(|_| SOCK_DEFAULT.to_string());
    let process_id = std::env::var("FLIPSTER_IPC_PROCESS_ID")
        .unwrap_or_else(|_| REG_PROCESS_ID_DEFAULT.to_string());
    info!(path = %path, process_id = %process_id, "flipster_ipc_reader: starting");
    std::thread::Builder::new()
        .name("flipster-ipc".to_string())
        .spawn(move || loop {
            match run_once(&path, &process_id, &tx) {
                Ok(()) => {
                    warn!("flipster_ipc_reader: stream closed cleanly; reconnecting");
                }
                Err(e) => {
                    warn!(path = %path, error = %e, "flipster_ipc_reader: stream error");
                }
            }
            std::thread::sleep(RECONNECT_BACKOFF);
        })
        .expect("spawn flipster_ipc_reader thread");
}

fn run_once(path: &str, process_id: &str, tx: &broadcast::Sender<BookTick>) -> Result<()> {
    let mut stream = UnixStream::connect(path)
        .with_context(|| format!("connect {}", path))?;
    let reg = format!("REGISTER:{}:\n", process_id);
    stream
        .write_all(reg.as_bytes())
        .context("write REGISTER")?;
    info!(path = %path, "flipster_ipc_reader: registered");

    let mut count: u64 = 0;
    let mut last_log = std::time::Instant::now();
    loop {
        let topic = read_lp_frame(&mut stream).context("read topic")?;
        let payload = read_lp_frame(&mut stream).context("read payload")?;
        if payload.len() != PAYLOAD_SIZE {
            // Length prefix protected; mismatched payload means a
            // protocol drift, not a transient. Bail to reconnect.
            return Err(anyhow::anyhow!(
                "unexpected payload size {} (expected {})",
                payload.len(),
                PAYLOAD_SIZE
            ));
        }
        if let Some(tick) = parse(&topic, &payload) {
            // tx.send returns Err when there are no subscribers; not
            // useful to propagate (paper bot may not be wired yet).
            let _ = tx.send(tick);
            count += 1;
            if last_log.elapsed().as_secs() >= 30 {
                info!(count, "flipster_ipc_reader: cumulative recv");
                last_log = std::time::Instant::now();
            }
        }
    }
}

/// Read a `[4B len LE][bytes]` length-prefixed frame.
fn read_lp_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

fn parse(topic: &[u8], payload: &[u8]) -> Option<BookTick> {
    // Topic shape: `flipster_bookticker_<SYMBOL>`. We don't strictly need
    // to read the symbol here — the payload also carries it — but using
    // the topic lets us bail cheaply on non-bookticker frames if more
    // data types are added later.
    let topic_str = std::str::from_utf8(topic).ok()?;
    if !topic_str.starts_with("flipster_bookticker_") {
        return None;
    }

    let raw_symbol = read_symbol(&payload[0..32])?;
    let symbol = normalize_symbol(&raw_symbol);
    let bid = read_f64_le(payload, 32);
    let ask = read_f64_le(payload, 40);
    let last = read_f64_le(payload, 48);
    let mark = read_f64_le(payload, 56);
    let index = read_f64_le(payload, 64);
    let server_ts_ns = read_i64_le(payload, 72);

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
        // Feed's Flipster path doesn't carry book-top sizes. Leaving 0.0
        // matches what the previous parse_flipster did.
        bid_size: 0.0,
        ask_size: 0.0,
        last_price: Some(last),
        mark_price: Some(mark),
        index_price: Some(index),
        timestamp: ts,
    })
}

fn read_symbol(buf: &[u8]) -> Option<String> {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let s = std::str::from_utf8(&buf[..end]).ok()?;
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// `BTC-USDT-PERP` → `BTCUSDT.PERP`. Pass-through for already-normalized
/// inputs. Keeps the rest of the collector (`pairs_core::base_of`,
/// position keys, ILP writes) untouched.
fn normalize_symbol(raw: &str) -> String {
    if raw.contains('-') {
        if let Some(rest) = raw.strip_suffix("-PERP") {
            return format!("{}.PERP", rest.replace('-', ""));
        }
        return raw.replace('-', "");
    }
    raw.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_dash_perp() {
        assert_eq!(normalize_symbol("BTC-USDT-PERP"), "BTCUSDT.PERP");
        assert_eq!(normalize_symbol("ETH-USDT-PERP"), "ETHUSDT.PERP");
    }

    #[test]
    fn normalize_already_dotted() {
        assert_eq!(normalize_symbol("BTCUSDT.PERP"), "BTCUSDT.PERP");
    }

    #[test]
    fn normalize_dash_no_perp() {
        // Unusual but defensive: drop dashes if any sneak through.
        assert_eq!(normalize_symbol("BTC-USDT"), "BTCUSDT");
    }

    #[test]
    fn parse_bookticker_payload() {
        let mut payload = [0u8; PAYLOAD_SIZE];
        let sym = b"BTC-USDT-PERP";
        payload[..sym.len()].copy_from_slice(sym);
        payload[32..40].copy_from_slice(&100.0_f64.to_le_bytes());
        payload[40..48].copy_from_slice(&100.5_f64.to_le_bytes());
        payload[48..56].copy_from_slice(&100.25_f64.to_le_bytes());
        payload[56..64].copy_from_slice(&100.0_f64.to_le_bytes());
        payload[64..72].copy_from_slice(&100.0_f64.to_le_bytes());
        payload[72..80].copy_from_slice(&1_700_000_000_000_000_000_i64.to_le_bytes());

        let tick = parse(b"flipster_bookticker_BTC-USDT-PERP", &payload)
            .expect("parse should succeed");
        assert_eq!(tick.exchange, ExchangeName::Flipster);
        assert_eq!(tick.symbol, "BTCUSDT.PERP");
        assert!((tick.bid_price - 100.0).abs() < 1e-9);
        assert!((tick.ask_price - 100.5).abs() < 1e-9);
    }

    #[test]
    fn parse_rejects_non_bookticker_topic() {
        let payload = [0u8; PAYLOAD_SIZE];
        assert!(parse(b"flipster_trade_BTC-USDT-PERP", &payload).is_none());
    }
}

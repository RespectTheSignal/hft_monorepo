mod protocol;

use protocol::FlipsterBookTicker;
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};

const TABLE_NAME: &str = "flipster_bookticker";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    dotenvy::dotenv().ok();

    let ipc_socket_path = std::env::var("IPC_SOCKET_PATH")
        .unwrap_or_else(|_| "/tmp/flipster_data_subscriber.sock".to_string());
    let qdb_conf = std::env::var("QDB_CLIENT_CONF")
        .unwrap_or_else(|_| "tcp::addr=211.181.122.102:9009;".to_string());
    let table_name = std::env::var("QDB_TABLE_NAME")
        .unwrap_or_else(|_| TABLE_NAME.to_string());
    let flush_interval_ms: u64 = std::env::var("FLUSH_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    // ---- Ctrl-C handler ---------------------------------------------------
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::Relaxed);
    })
    .expect("set ctrl-c handler");

    // ---- QuestDB sender ---------------------------------------------------
    info!("connecting to QuestDB...");
    let mut sender = match Sender::from_conf(&qdb_conf) {
        Ok(s) => s,
        Err(e) => {
            error!("QuestDB connection failed: {e}");
            std::process::exit(1);
        }
    };
    let mut buffer = Buffer::new();
    info!("QuestDB connected (table={table_name})");

    // ---- IPC client -------------------------------------------------------
    info!("connecting to IPC at {ipc_socket_path}...");
    let mut stream = loop {
        match UnixStream::connect(&ipc_socket_path) {
            Ok(s) => break s,
            Err(e) => {
                if !running.load(Ordering::Relaxed) {
                    info!("shutting down before IPC connect");
                    return;
                }
                warn!("IPC connect failed: {e}, retrying in 2s...");
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
    };

    // Register for all symbols
    {
        use std::io::Write;
        let reg = format!("REGISTER:questdb_uploader_{}:\n", std::process::id());
        stream.write_all(reg.as_bytes()).expect("IPC register");
        info!("IPC registered (all symbols)");
    }

    // ---- Main loop --------------------------------------------------------
    info!("===== questdb_uploader running (Ctrl-C to stop) =====");

    let mut rows_buffered: usize = 0;
    let mut rows_flushed: u64 = 0;
    let mut flush_errors: u64 = 0;
    let mut recv_count: u64 = 0;
    let mut last_flush = std::time::Instant::now();
    let mut last_log = std::time::Instant::now();
    let flush_dur = std::time::Duration::from_millis(flush_interval_ms);

    while running.load(Ordering::Relaxed) {
        // Read framed message: [4B topic_len][topic][4B payload_len][payload]
        let topic = match read_frame(&mut stream) {
            Ok(data) => data,
            Err(e) => {
                error!("IPC read topic error: {e}");
                break;
            }
        };

        let payload = match read_frame(&mut stream) {
            Ok(data) => data,
            Err(e) => {
                error!("IPC read payload error: {e}");
                break;
            }
        };

        recv_count += 1;

        // Parse payload
        let tick = match FlipsterBookTicker::from_bytes(&payload) {
            Some(t) => t,
            None => {
                warn!("failed to parse payload ({} bytes)", payload.len());
                continue;
            }
        };

        // Buffer to QuestDB
        if let Err(e) = buffer_tick(&mut buffer, &table_name, &tick) {
            error!("QuestDB buffer error: {e}");
            continue;
        }
        rows_buffered += 1;

        // Periodic flush
        if last_flush.elapsed() >= flush_dur {
            if rows_buffered > 0 {
                match sender.flush(&mut buffer) {
                    Ok(()) => {
                        rows_flushed += rows_buffered as u64;
                        rows_buffered = 0;
                    }
                    Err(e) => {
                        flush_errors += 1;
                        error!("QuestDB flush failed ({rows_buffered} rows): {e}");
                        buffer.clear();
                        rows_buffered = 0;
                    }
                }
            }
            last_flush = std::time::Instant::now();
        }

        // Periodic log
        if last_log.elapsed().as_secs() >= 5 {
            info!(
                "received={recv_count} flushed={rows_flushed} buffered={rows_buffered} errors={flush_errors}"
            );
            last_log = std::time::Instant::now();
        }
    }

    // Final flush
    if rows_buffered > 0 {
        match sender.flush(&mut buffer) {
            Ok(()) => {
                rows_flushed += rows_buffered as u64;
            }
            Err(e) => {
                error!("QuestDB final flush failed: {e}");
                flush_errors += 1;
            }
        }
    }

    info!(
        "shutting down (received={recv_count}, flushed={rows_flushed}, errors={flush_errors})"
    );
}

fn buffer_tick(
    buf: &mut Buffer,
    table: &str,
    tick: &FlipsterBookTicker,
) -> Result<(), questdb::Error> {
    buf.table(table)?;
    buf.symbol("symbol", &tick.symbol)?;
    buf.column_f64("bid_price", tick.bid_price)?;
    buf.column_f64("ask_price", tick.ask_price)?;
    buf.column_f64("last_price", tick.last_price)?;
    buf.column_f64("mark_price", tick.mark_price)?;
    buf.column_f64("index_price", tick.index_price)?;
    buf.at(TimestampNanos::new(tick.server_ts_ns))?;
    Ok(())
}

/// Read a length-prefixed frame: [4B len LE][data]
fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut data = vec![0u8; len];
    stream.read_exact(&mut data)?;
    Ok(data)
}

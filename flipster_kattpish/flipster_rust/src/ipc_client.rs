// IPC Client for receiving market data from Data Subscriber
// Uses Unix Domain Socket for low-latency local communication
// Event-based: messages trigger cache updates immediately

use crate::data_cache::DataCache;
use crate::data_source_common::process_data_message;
use crate::data_client::DataClient;
use crate::{elog, plog};
use anyhow::{Context, Result};
use parking_lot::RwLock;
use std::string::String;
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct IpcClient {
    socket_path: String,
    symbols: Arc<RwLock<Vec<String>>>,
    cache: Arc<DataCache>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl IpcClient {
    pub fn new(socket_path: String, symbols: Vec<String>, cache: Arc<DataCache>) -> Self {
        Self {
            socket_path,
            symbols: Arc::new(RwLock::new(symbols)),
            cache,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub fn update_symbols(&self, symbols: Vec<String>) {
        let mut guard = self.symbols.write();
        *guard = symbols;
    }

    pub fn start(&self) -> Result<()> {
        if self.running.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(()); // Already running
        }

        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let socket_path = self.socket_path.clone();
        let symbols = self.symbols.read().clone();
        let cache = self.cache.clone();
        let running = self.running.clone();

        plog!(
            "\x1b[32m✅ IPC client started (socket: {})\x1b[0m",
            socket_path
        );

        std::thread::spawn(move || {
            if let Err(e) = Self::receive_loop(socket_path, symbols, cache, running) {
                elog!("[IPC Client] Error: {}", e);
            }
        });

        Ok(())
    }

    fn receive_loop(
        socket_path: String,
        symbols: Vec<String>,
        cache: Arc<DataCache>,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        // Check if socket file exists
        if !Path::new(&socket_path).exists() {
            return Err(anyhow::anyhow!(
                "Socket file does not exist: {}",
                socket_path
            ));
        }

        // Connect to IPC server (blocking)
        let mut stream = UnixStream::connect(&socket_path)
            .with_context(|| format!("Failed to connect to IPC server: {}", socket_path))?;

        // Send registration: "REGISTER:<process_id>:<symbol1>,<symbol2>,...\n"
        let process_id = format!("flipster_rust_{}", std::process::id());
        let symbols_str = symbols.join(",");
        let registration = format!("REGISTER:{}:{}\n", process_id, symbols_str);
        stream
            .write_all(registration.as_bytes())
            .context("Failed to send registration")?;
        stream.flush()?;

        // Silent - match Python behavior (no connection message)

        // Use Vec as buffer (similar to Python's bytearray)
        let mut buffer = Vec::new();
        let mut read_buf = vec![0u8; 4096]; // Temporary buffer for recv

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            // Blocking read - OS will wake us when data arrives (like Python)
            match stream.read(&mut read_buf) {
                Ok(0) => {
                    // Connection closed
                    plog!("[IPC Client] Connection closed by server");
                    break;
                }
                Ok(n) => {
                    // Append received data to buffer
                    buffer.extend_from_slice(&read_buf[..n]);

                    // Process complete messages from buffer
                    loop {
                        // Need at least 4 bytes for topic_len
                        if buffer.len() < 4 {
                            break;
                        }

                        // Read topic length (4 bytes, little-endian)
                        let topic_len =
                            u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]])
                                as usize;

                        // Check if we have complete topic
                        if buffer.len() < 4 + topic_len {
                            break;
                        }

                        // Check if we have payload length (4 bytes)
                        if buffer.len() < 8 + topic_len {
                            break;
                        }

                        // Read payload length (4 bytes, little-endian)
                        let payload_len = u32::from_le_bytes([
                            buffer[4 + topic_len],
                            buffer[5 + topic_len],
                            buffer[6 + topic_len],
                            buffer[7 + topic_len],
                        ]) as usize;

                        // Check if we have complete payload
                        if buffer.len() < 8 + topic_len + payload_len {
                            break;
                        }

                        // Record receive time immediately after we have full message (before any processing)
                        let received_at_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as i64;

                        // Pass slices only (no to_vec); process_message borrows from buffer
                        let topic_slice = &buffer[4..4 + topic_len];
                        let payload_slice = &buffer[8 + topic_len..8 + topic_len + payload_len];

                        if let Err(e) = process_data_message(
                            topic_slice,
                            payload_slice,
                            received_at_ms,
                            &cache,
                        ) {
                            elog!("[IPC Client] Error processing message: {}", e);
                        }

                        // Remove processed message from buffer after we're done with slices
                        buffer.drain(..8 + topic_len + payload_len);
                    }
                }
                Err(e) => {
                    if !running.load(std::sync::atomic::Ordering::SeqCst) {
                        break; // Stopped intentionally
                    }
                    return Err(e).context("Failed to read from socket");
                }
            }
        }

        running.store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    pub fn stop(&self) {
        self.running
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl crate::data_client::DataClient for IpcClient {
    fn start(&self) -> Result<()> {
        IpcClient::start(self)
    }
    fn stop(&self) {
        IpcClient::stop(self);
    }
    fn update_symbols(&self, symbols: Vec<String>) {
        IpcClient::update_symbols(self, symbols);
    }
    fn is_running(&self) -> bool {
        IpcClient::is_running(self)
    }
}

// ZMQ SUB client: receive market data directly from data_publisher (no data_subscriber).
// Connects to gate PUB (GATE_DATA_URL), optional GATE_DATA_URL_BACKUP, and optionally base PUB (BINANCE_DATA_URL etc. by base_exchange).

use crate::data_cache::DataCache;
use crate::data_client::DataClient;
use crate::data_source_common::process_data_message;
use crate::plog;
use anyhow::{Context, Result};
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct ZmqSubClient {
    gate_data_url: String,
    gate_data_url_backup: Option<String>,
    base_data_url: Option<String>,
    base_exchange: String,
    symbols: Arc<RwLock<Vec<String>>>,
    cache: Arc<DataCache>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl ZmqSubClient {
    /// gate_data_url: e.g. tcp://host:5559 (Gate).
    /// gate_data_url_backup: optional second gate PUB for redundancy.
    /// base_data_url: optional, e.g. tcp://host:6000 (Binance); when set, subscribes to base_exchange bookticker.
    pub fn new(
        gate_data_url: String,
        gate_data_url_backup: Option<String>,
        base_data_url: Option<String>,
        base_exchange: String,
        symbols: Vec<String>,
        cache: Arc<DataCache>,
    ) -> Self {
        Self {
            gate_data_url,
            gate_data_url_backup,
            base_data_url,
            base_exchange,
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
            return Ok(());
        }

        self.running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let gate_data_url = self.gate_data_url.clone();
        let gate_data_url_backup = self.gate_data_url_backup.clone();
        let base_data_url = self.base_data_url.clone();
        let base_exchange = self.base_exchange.clone();
        let cache = self.cache.clone();
        let running = self.running.clone();

        plog!(
            "\x1b[32m✅ ZMQ data client started (gate: {}, backup: {:?}, base: {:?})\x1b[0m",
            gate_data_url,
            gate_data_url_backup,
            base_data_url
        );

        std::thread::spawn(move || {
            if let Err(e) = Self::receive_loop(
                gate_data_url,
                gate_data_url_backup,
                base_data_url,
                base_exchange,
                cache,
                running,
            ) {
                crate::elog!("[ZMQ Sub Client] Error: {}", e);
            }
        });

        Ok(())
    }

    fn receive_loop(
        gate_data_url: String,
        gate_data_url_backup: Option<String>,
        base_data_url: Option<String>,
        base_exchange: String,
        cache: Arc<DataCache>,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        let ctx = zmq::Context::new();
        let sub = ctx
            .socket(zmq::SUB)
            .context("Failed to create ZMQ SUB socket")?;
        sub.set_rcvhwm(100_000)?;
        sub.set_rcvbuf(4 * 1024 * 1024).ok(); // 4MB

        sub.connect(&gate_data_url)
            .with_context(|| format!("Failed to connect to gate {}", gate_data_url))?;
        if let Some(ref url) = gate_data_url_backup {
            sub.connect(url)
                .with_context(|| format!("Failed to connect to gate backup {}", url))?;
        }
        if let Some(ref url) = base_data_url {
            sub.connect(url)
                .with_context(|| format!("Failed to connect to base {}", url))?;
        }

        // Gate (same as data_subscriber)
        sub.set_subscribe(b"gate_bookticker_")?;
        sub.set_subscribe(b"gate_webbookticker_")?;
        sub.set_subscribe(b"gate_trade_")?;
        // Base exchange bookticker (e.g. binance_bookticker_)
        let base_prefix = format!("{}_bookticker_", base_exchange.to_lowercase());
        sub.set_subscribe(base_prefix.as_bytes())?;

        // Balance latency vs CPU: poll when no data. Env ZMQ_POLL_MS (default 1); larger = less CPU, slightly more latency when idle.
        let poll_ms: i64 = std::env::var("ZMQ_POLL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1)
            .clamp(0, 100);

        while running.load(std::sync::atomic::Ordering::SeqCst) {
            match sub.recv_multipart(zmq::DONTWAIT) {
                Ok(frames) => {
                    if frames.len() < 2 {
                        continue;
                    }
                    let topic = &frames[0];
                    let payload = &frames[1];
                    let received_at_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64;

                    if let Err(e) = process_data_message(topic, payload, received_at_ms, &cache) {
                        crate::elog!("[ZMQ Sub Client] process_message error: {}", e);
                    }
                }
                Err(zmq::Error::EAGAIN) => {
                    let mut items = [sub.as_poll_item(zmq::POLLIN)];
                    let _ = zmq::poll(&mut items, poll_ms);
                }
                Err(e) => {
                    if running.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(e).context("ZMQ recv failed");
                    }
                    break;
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

impl DataClient for ZmqSubClient {
    fn start(&self) -> Result<()> {
        ZmqSubClient::start(self)
    }
    fn stop(&self) {
        ZmqSubClient::stop(self);
    }
    fn update_symbols(&self, symbols: Vec<String>) {
        ZmqSubClient::update_symbols(self, symbols);
    }
    fn is_running(&self) -> bool {
        ZmqSubClient::is_running(self)
    }
}

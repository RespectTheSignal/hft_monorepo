use crate::error::{Error, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// Single bookticker row sent from WS tasks to the writer thread.
#[derive(Default)]
pub struct BookTickerRow {
    pub symbol: String,
    pub bid: f64,
    pub ask: f64,
    pub bid_size: Option<f64>,
    pub ask_size: Option<f64>,
    pub server_ts_ns: i64,
    /// Optional local-clock receive time (ns since epoch). When set, written as a
    /// `recv_ts_ns` LONG column. Used to measure feed delivery lag where
    /// `server_ts_ns` is identical across endpoints (e.g., HL api vs api-ui).
    pub recv_ts_ns: Option<i64>,
}

/// Async-safe handle. Cloning is cheap; underlying sender thread is shared.
#[derive(Clone)]
pub struct BookTickerWriter {
    tx: mpsc::SyncSender<BookTickerRow>,
}

impl BookTickerWriter {
    /// `host:port` is the QuestDB ILP TCP endpoint (default 9009).
    /// `table` is the target table name, e.g. `okx_bookticker`.
    /// Buffer flushes every `flush_every` rows or `flush_interval`.
    pub fn spawn(host: String, port: u16, table: String) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<BookTickerRow>(65_536);
        let conf = format!("tcp::addr={host}:{port};");
        let sender = Sender::from_conf(&conf)
            .map_err(|e| Error::Other(format!("ilp connect {host}:{port}: {e}")))?;
        info!("ILP writer connected: {host}:{port} → {table}");

        thread::Builder::new()
            .name(format!("ilp-{table}"))
            .spawn(move || writer_thread(sender, table, rx))
            .map_err(|e| Error::Other(format!("spawn ilp thread: {e}")))?;

        Ok(Self { tx })
    }

    /// Non-blocking submit. Drops the row if the queue is full (back-pressure
    /// rather than block the WS task).
    pub fn submit(&self, row: BookTickerRow) {
        if self.tx.try_send(row).is_err() {
            // queue full or thread died — drop silently to avoid log spam.
        }
    }
}

const FLUSH_EVERY_ROWS: usize = 200;
const FLUSH_INTERVAL: Duration = Duration::from_millis(200);

fn writer_thread(mut sender: Sender, table: String, rx: mpsc::Receiver<BookTickerRow>) {
    let mut buffer = Buffer::new();
    let mut pending: usize = 0;
    let mut total: u64 = 0;
    let mut last_flush = Instant::now();
    let mut last_log = Instant::now();

    loop {
        let timeout = FLUSH_INTERVAL
            .checked_sub(last_flush.elapsed())
            .unwrap_or(Duration::ZERO);

        match rx.recv_timeout(timeout) {
            Ok(row) => {
                if let Err(e) = encode_row(&mut buffer, &table, &row) {
                    warn!("encode row: {e}");
                    continue;
                }
                pending += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if pending > 0 {
                    let _ = sender.flush(&mut buffer);
                }
                info!("ILP writer for {table} exiting ({total} rows total)");
                return;
            }
        }

        let due = pending >= FLUSH_EVERY_ROWS || last_flush.elapsed() >= FLUSH_INTERVAL;
        if pending > 0 && due {
            match sender.flush(&mut buffer) {
                Ok(_) => {
                    total += pending as u64;
                    pending = 0;
                    last_flush = Instant::now();
                }
                Err(e) => {
                    error!("ILP flush failed for {table}: {e} — dropping {pending} rows");
                    buffer.clear();
                    pending = 0;
                    last_flush = Instant::now();
                }
            }
        }

        if last_log.elapsed() >= Duration::from_secs(15) {
            info!("ILP {table}: {total} rows written total");
            last_log = Instant::now();
        }
    }
}

// ---------------------------------------------------------------------------
// Trade rows (Binance aggTrade etc.)
// ---------------------------------------------------------------------------

pub struct TradeRow {
    pub symbol: String,
    pub price: f64,
    pub quantity: f64,
    pub usd_value: f64,
    /// true = aggressive sell hit the bid; false = aggressive buy lifted the ask.
    pub is_buyer_maker: bool,
    pub trade_id: i64,
    pub server_ts_ns: i64,
}

#[derive(Clone)]
pub struct TradeWriter {
    tx: mpsc::SyncSender<TradeRow>,
}

impl TradeWriter {
    pub fn spawn(host: String, port: u16, table: String) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<TradeRow>(131_072);
        let conf = format!("tcp::addr={host}:{port};");
        let sender = Sender::from_conf(&conf)
            .map_err(|e| Error::Other(format!("ilp connect {host}:{port}: {e}")))?;
        info!("ILP trade writer connected: {host}:{port} → {table}");
        thread::Builder::new()
            .name(format!("ilp-{table}"))
            .spawn(move || trade_writer_thread(sender, table, rx))
            .map_err(|e| Error::Other(format!("spawn ilp thread: {e}")))?;
        Ok(Self { tx })
    }
    pub fn submit(&self, row: TradeRow) {
        let _ = self.tx.try_send(row);
    }
}

fn trade_writer_thread(mut sender: Sender, table: String, rx: mpsc::Receiver<TradeRow>) {
    let mut buffer = Buffer::new();
    let mut pending: usize = 0;
    let mut total: u64 = 0;
    let mut last_flush = Instant::now();
    let mut last_log = Instant::now();
    loop {
        let timeout = FLUSH_INTERVAL.checked_sub(last_flush.elapsed()).unwrap_or(Duration::ZERO);
        match rx.recv_timeout(timeout) {
            Ok(row) => {
                if let Err(e) = encode_trade(&mut buffer, &table, &row) {
                    warn!("encode trade: {e}");
                    continue;
                }
                pending += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if pending > 0 { let _ = sender.flush(&mut buffer); }
                info!("ILP trade writer for {table} exiting ({total} rows)");
                return;
            }
        }
        let due = pending >= FLUSH_EVERY_ROWS || last_flush.elapsed() >= FLUSH_INTERVAL;
        if pending > 0 && due {
            match sender.flush(&mut buffer) {
                Ok(_) => { total += pending as u64; pending = 0; last_flush = Instant::now(); }
                Err(e) => { error!("ILP flush failed for {table}: {e}"); buffer.clear(); pending = 0; last_flush = Instant::now(); }
            }
        }
        if last_log.elapsed() >= Duration::from_secs(15) {
            info!("ILP {table}: {total} rows total");
            last_log = Instant::now();
        }
    }
}

fn encode_trade(buffer: &mut Buffer, table: &str, row: &TradeRow) -> Result<()> {
    buffer
        .table(table).map_err(|e| Error::Other(format!("table: {e}")))?
        .symbol("symbol", &row.symbol).map_err(|e| Error::Other(format!("sym: {e}")))?
        .symbol("side", if row.is_buyer_maker { "sell" } else { "buy" }).map_err(|e| Error::Other(format!("side: {e}")))?
        .column_f64("price", row.price).map_err(|e| Error::Other(format!("price: {e}")))?
        .column_f64("quantity", row.quantity).map_err(|e| Error::Other(format!("qty: {e}")))?
        .column_f64("usd_value", row.usd_value).map_err(|e| Error::Other(format!("usd: {e}")))?
        .column_i64("trade_id", row.trade_id).map_err(|e| Error::Other(format!("tid: {e}")))?
        .at(TimestampNanos::new(row.server_ts_ns)).map_err(|e| Error::Other(format!("at: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Depth5 rows (Binance @depth5@100ms etc.)
// ---------------------------------------------------------------------------

pub struct Depth5Row {
    pub symbol: String,
    pub bids: [(f64, f64); 5], // (price, qty) — index 0 = best bid
    pub asks: [(f64, f64); 5],
    pub server_ts_ns: i64,
}

#[derive(Clone)]
pub struct Depth5Writer {
    tx: mpsc::SyncSender<Depth5Row>,
}

impl Depth5Writer {
    pub fn spawn(host: String, port: u16, table: String) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Depth5Row>(65_536);
        let conf = format!("tcp::addr={host}:{port};");
        let sender = Sender::from_conf(&conf)
            .map_err(|e| Error::Other(format!("ilp connect {host}:{port}: {e}")))?;
        info!("ILP depth5 writer connected: {host}:{port} → {table}");
        thread::Builder::new()
            .name(format!("ilp-{table}"))
            .spawn(move || depth5_writer_thread(sender, table, rx))
            .map_err(|e| Error::Other(format!("spawn ilp thread: {e}")))?;
        Ok(Self { tx })
    }
    pub fn submit(&self, row: Depth5Row) {
        let _ = self.tx.try_send(row);
    }
}

fn depth5_writer_thread(mut sender: Sender, table: String, rx: mpsc::Receiver<Depth5Row>) {
    let mut buffer = Buffer::new();
    let mut pending: usize = 0;
    let mut total: u64 = 0;
    let mut last_flush = Instant::now();
    let mut last_log = Instant::now();
    loop {
        let timeout = FLUSH_INTERVAL.checked_sub(last_flush.elapsed()).unwrap_or(Duration::ZERO);
        match rx.recv_timeout(timeout) {
            Ok(row) => {
                if let Err(e) = encode_depth5(&mut buffer, &table, &row) {
                    warn!("encode depth: {e}");
                    continue;
                }
                pending += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if pending > 0 { let _ = sender.flush(&mut buffer); }
                info!("ILP depth5 writer for {table} exiting ({total} rows)");
                return;
            }
        }
        let due = pending >= FLUSH_EVERY_ROWS || last_flush.elapsed() >= FLUSH_INTERVAL;
        if pending > 0 && due {
            match sender.flush(&mut buffer) {
                Ok(_) => { total += pending as u64; pending = 0; last_flush = Instant::now(); }
                Err(e) => { error!("ILP flush failed for {table}: {e}"); buffer.clear(); pending = 0; last_flush = Instant::now(); }
            }
        }
        if last_log.elapsed() >= Duration::from_secs(15) {
            info!("ILP {table}: {total} rows total");
            last_log = Instant::now();
        }
    }
}

fn encode_depth5(buffer: &mut Buffer, table: &str, row: &Depth5Row) -> Result<()> {
    let mut b = buffer.table(table).map_err(|e| Error::Other(format!("table: {e}")))?
        .symbol("symbol", &row.symbol).map_err(|e| Error::Other(format!("sym: {e}")))?;
    let cols: [(&str, f64); 20] = [
        ("bid1_px", row.bids[0].0), ("bid1_qty", row.bids[0].1),
        ("bid2_px", row.bids[1].0), ("bid2_qty", row.bids[1].1),
        ("bid3_px", row.bids[2].0), ("bid3_qty", row.bids[2].1),
        ("bid4_px", row.bids[3].0), ("bid4_qty", row.bids[3].1),
        ("bid5_px", row.bids[4].0), ("bid5_qty", row.bids[4].1),
        ("ask1_px", row.asks[0].0), ("ask1_qty", row.asks[0].1),
        ("ask2_px", row.asks[1].0), ("ask2_qty", row.asks[1].1),
        ("ask3_px", row.asks[2].0), ("ask3_qty", row.asks[2].1),
        ("ask4_px", row.asks[3].0), ("ask4_qty", row.asks[3].1),
        ("ask5_px", row.asks[4].0), ("ask5_qty", row.asks[4].1),
    ];
    for (name, val) in cols {
        b = b.column_f64(name, val).map_err(|e| Error::Other(format!("{name}: {e}")))?;
    }
    b.at(TimestampNanos::new(row.server_ts_ns)).map_err(|e| Error::Other(format!("at: {e}")))?;
    Ok(())
}

fn encode_row(buffer: &mut Buffer, table: &str, row: &BookTickerRow) -> Result<()> {
    buffer
        .table(table)
        .map_err(|e| Error::Other(format!("table: {e}")))?
        .symbol("symbol", &row.symbol)
        .map_err(|e| Error::Other(format!("symbol col: {e}")))?
        .column_f64("bid_price", row.bid)
        .map_err(|e| Error::Other(format!("bid: {e}")))?
        .column_f64("ask_price", row.ask)
        .map_err(|e| Error::Other(format!("ask: {e}")))?;
    if let Some(v) = row.bid_size {
        buffer
            .column_f64("bid_size", v)
            .map_err(|e| Error::Other(format!("bid_size: {e}")))?;
    }
    if let Some(v) = row.ask_size {
        buffer
            .column_f64("ask_size", v)
            .map_err(|e| Error::Other(format!("ask_size: {e}")))?;
    }
    if let Some(v) = row.recv_ts_ns {
        buffer
            .column_i64("recv_ts_ns", v)
            .map_err(|e| Error::Other(format!("recv_ts_ns: {e}")))?;
    }
    buffer
        .at(TimestampNanos::new(row.server_ts_ns))
        .map_err(|e| Error::Other(format!("at: {e}")))?;
    Ok(())
}

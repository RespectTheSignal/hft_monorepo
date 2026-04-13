use crate::error::{Error, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use tracing::{error, info, warn};

pub struct QdbWriter {
    sender: Sender,
    buffer: Buffer,
    table: String,
    rows_buffered: usize,
    rows_flushed: u64,
    flush_errors: u64,
}

impl QdbWriter {
    /// Create a new QdbWriter. The TCP connection is established here,
    /// so this call blocks until connected (or fails).
    /// Use `try_new_with_timeout` for a non-hanging alternative.
    pub fn new(conf: &str, table: &str) -> Result<Self> {
        let sender = Sender::from_conf(conf).map_err(|e| {
            Error::Other(format!("QuestDB sender config error: {e}"))
        })?;
        info!("QuestDB sender connected (table={table})");
        Ok(Self {
            sender,
            buffer: Buffer::new(),
            table: table.to_string(),
            rows_buffered: 0,
            rows_flushed: 0,
            flush_errors: 0,
        })
    }

    /// Try to create a QdbWriter with a timeout (in seconds).
    /// Returns Ok(None) if the connection times out.
    pub fn try_new_with_timeout(
        conf: &str,
        table: &str,
        timeout_secs: u64,
    ) -> Result<Option<Self>> {
        let conf = conf.to_string();
        let table_str = table.to_string();

        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let res = Sender::from_conf(&conf);
            let _ = result_tx.send(res);
        });

        match result_rx.recv_timeout(std::time::Duration::from_secs(timeout_secs)) {
            Ok(Ok(sender)) => {
                info!("QuestDB sender connected (table={table})");
                Ok(Some(Self {
                    sender,
                    buffer: Buffer::new(),
                    table: table_str,
                    rows_buffered: 0,
                    rows_flushed: 0,
                    flush_errors: 0,
                }))
            }
            Ok(Err(e)) => {
                Err(Error::Other(format!("QuestDB connection failed: {e}")))
            }
            Err(_) => {
                warn!("QuestDB connection timed out after {timeout_secs}s");
                Ok(None)
            }
        }
    }

    /// Buffer a single ticker row. Call `flush()` periodically to send.
    pub fn buffer_tick(
        &mut self,
        symbol: &str,
        bid_price: Option<f64>,
        ask_price: Option<f64>,
        last_price: Option<f64>,
        mark_price: Option<f64>,
        index_price: Option<f64>,
        server_ts_ns: i64,
    ) -> Result<()> {
        let buf = &mut self.buffer;
        buf.table(self.table.as_str()).map_err(qdb_err)?;
        buf.symbol("symbol", symbol).map_err(qdb_err)?;

        if let Some(v) = bid_price {
            buf.column_f64("bid_price", v).map_err(qdb_err)?;
        }
        if let Some(v) = ask_price {
            buf.column_f64("ask_price", v).map_err(qdb_err)?;
        }
        if let Some(v) = last_price {
            buf.column_f64("last_price", v).map_err(qdb_err)?;
        }
        if let Some(v) = mark_price {
            buf.column_f64("mark_price", v).map_err(qdb_err)?;
        }
        if let Some(v) = index_price {
            buf.column_f64("index_price", v).map_err(qdb_err)?;
        }

        buf.at(TimestampNanos::new(server_ts_ns)).map_err(qdb_err)?;
        self.rows_buffered += 1;
        Ok(())
    }

    /// Flush buffered rows to QuestDB. Returns the number of rows flushed.
    pub fn flush(&mut self) -> Result<usize> {
        if self.rows_buffered == 0 {
            return Ok(0);
        }
        let n = self.rows_buffered;
        match self.sender.flush(&mut self.buffer) {
            Ok(()) => {
                self.rows_flushed += n as u64;
                self.rows_buffered = 0;
                Ok(n)
            }
            Err(e) => {
                self.flush_errors += 1;
                error!("QuestDB flush failed ({n} rows): {e}");
                self.buffer.clear();
                self.rows_buffered = 0;
                Err(Error::Other(format!("QuestDB flush: {e}")))
            }
        }
    }

    pub fn rows_flushed(&self) -> u64 {
        self.rows_flushed
    }

    pub fn flush_errors(&self) -> u64 {
        self.flush_errors
    }
}

fn qdb_err(e: questdb::Error) -> Error {
    Error::Other(format!("QuestDB: {e}"))
}

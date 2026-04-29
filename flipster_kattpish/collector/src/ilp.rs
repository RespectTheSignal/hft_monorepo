use crate::model::BookTick;
use anyhow::Result;
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Thin async wrapper around questdb-rs ILP sender.
/// The underlying `Sender` is blocking, so we guard it with a Mutex and
/// spawn `flush` calls on `spawn_blocking` when needed.
#[derive(Clone)]
pub struct IlpWriter {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    sender: Sender,
    buffer: Buffer,
    pending: usize,
    flush_every: usize,
}

impl Inner {
    fn do_flush(&mut self) -> Result<()> {
        self.sender.flush(&mut self.buffer)?;
        self.pending = 0;
        Ok(())
    }
    fn maybe_flush(&mut self) -> Result<()> {
        if self.pending >= self.flush_every {
            self.do_flush()?;
        }
        Ok(())
    }
}

impl IlpWriter {
    pub fn connect(host: &str, port: u16, flush_every: usize) -> Result<Self> {
        let conf = format!("tcp::addr={host}:{port};");
        let sender = Sender::from_conf(conf)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(Inner {
                sender,
                buffer: Buffer::new(),
                pending: 0,
                flush_every,
            })),
        })
    }

    /// Write a single per-base baseline row for the spread_revert
    /// strategy. The row contains the moving averages it needs to enter
    /// without an in-memory warm-up:
    ///   - `bf_avg_gap_bp`: 30 min binance↔flipster mid gap, sampled
    ///     at 5 s.
    ///   - `flipster_avg_spread_bp` / `binance_avg_spread_bp`: 10 min
    ///     per-venue avg spread.
    /// `ts` is the moment the values were computed (used to detect
    /// staleness on the consumer side).
    #[allow(clippy::too_many_arguments)]
    pub async fn write_baseline(
        &self,
        base: &str,
        ts: chrono::DateTime<chrono::Utc>,
        bf_avg_gap_bp: f64,
        flipster_avg_spread_bp: f64,
        binance_avg_spread_bp: f64,
        n_gap_samples: i64,
        n_spread_samples: i64,
    ) -> Result<()> {
        let mut g = self.inner.lock().await;
        g.buffer
            .table("bf_baseline")?
            .symbol("base", base)?
            .column_f64("bf_avg_gap_bp", bf_avg_gap_bp)?
            .column_f64("flipster_avg_spread_bp", flipster_avg_spread_bp)?
            .column_f64("binance_avg_spread_bp", binance_avg_spread_bp)?
            .column_i64("n_gap_samples", n_gap_samples)?
            .column_i64("n_spread_samples", n_spread_samples)?
            .at(TimestampNanos::new(
                ts.timestamp_nanos_opt().unwrap_or(0),
            ))?;
        g.pending += 1;
        g.maybe_flush()?;
        Ok(())
    }

    pub async fn write_tick(&self, tick: &BookTick) -> Result<()> {
        // On shared QuestDB deployments where an upstream feed already writes
        // bookticker rows (e.g. gate1's central feed), set DISABLE_TICK_WRITES=1
        // in the environment to short-circuit this write and avoid duplicate
        // rows. Position_log / funding_rate / latency_log writes are unaffected.
        if std::env::var("DISABLE_TICK_WRITES")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            return Ok(());
        }
        let mut g = self.inner.lock().await;
        g.buffer
            .table(tick.exchange.table())?
            .symbol("symbol", &tick.symbol)?
            .column_f64("bid_price", tick.bid_price)?
            .column_f64("ask_price", tick.ask_price)?
            .column_f64("bid_size", tick.bid_size)?
            .column_f64("ask_size", tick.ask_size)?;
        if let Some(v) = tick.last_price {
            g.buffer.column_f64("last_price", v)?;
        }
        if let Some(v) = tick.mark_price {
            g.buffer.column_f64("mark_price", v)?;
        }
        if let Some(v) = tick.index_price {
            g.buffer.column_f64("index_price", v)?;
        }
        let ts_ns = tick.timestamp.timestamp_nanos_opt().unwrap_or(0);
        g.buffer.at(TimestampNanos::new(ts_ns))?;
        g.pending += 1;
        g.maybe_flush()?;
        Ok(())
    }

    pub async fn write_health(&self, exchange: &str, event: &str, detail: &str) -> Result<()> {
        let mut g = self.inner.lock().await;
        let ts_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        g.buffer
            .table("collector_health")?
            .symbol("exchange", exchange)?
            .symbol("event", event)?
            .column_str("detail", detail)?
            .at(TimestampNanos::new(ts_ns))?;
        g.do_flush()?;
        Ok(())
    }

    pub async fn write_funding(
        &self,
        exchange: &str,
        symbol: &str,
        rate: f64,
        next_funding: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let mut g = self.inner.lock().await;
        let ts_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        g.buffer
            .table("funding_rate")?
            .symbol("exchange", exchange)?
            .symbol("symbol", symbol)?
            .column_f64("rate", rate)?
            .column_ts(
                "next_funding_time",
                TimestampNanos::new(next_funding.timestamp_nanos_opt().unwrap_or(0)),
            )?
            .at(TimestampNanos::new(ts_ns))?;
        g.do_flush()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn write_latency(
        &self,
        symbol: &str,
        binance_ts: chrono::DateTime<chrono::Utc>,
        flipster_ts: chrono::DateTime<chrono::Utc>,
        latency_ms: f64,
        binance_mid: f64,
        flipster_mid: f64,
        price_diff_bp: f64,
        direction: i32,
    ) -> Result<()> {
        let mut g = self.inner.lock().await;
        g.buffer
            .table("latency_log")?
            .symbol("symbol", symbol)?
            .column_ts(
                "flipster_ts",
                TimestampNanos::new(flipster_ts.timestamp_nanos_opt().unwrap_or(0)),
            )?
            .column_f64("latency_ms", latency_ms)?
            .column_f64("binance_mid", binance_mid)?
            .column_f64("flipster_mid", flipster_mid)?
            .column_f64("price_diff_bp", price_diff_bp)?
            .column_i64("direction", direction as i64)?
            .at(TimestampNanos::new(
                binance_ts.timestamp_nanos_opt().unwrap_or(0),
            ))?;
        g.pending += 1;
        g.maybe_flush()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn write_position(
        &self,
        account_id: &str,
        symbol: &str,
        side: &str,
        entry_price: f64,
        exit_price: f64,
        size: f64,
        pnl_bp: f64,
        entry_time: chrono::DateTime<chrono::Utc>,
        exit_time: chrono::DateTime<chrono::Utc>,
        strategy: &str,
        exit_reason: &str,
        mode: &str,
    ) -> Result<()> {
        let mut g = self.inner.lock().await;
        g.buffer
            .table("position_log")?
            .symbol("account_id", account_id)?
            .symbol("symbol", symbol)?
            .symbol("side", side)?
            .symbol("strategy", strategy)?
            .symbol("exit_reason", exit_reason)?
            .symbol("mode", mode)?
            .column_f64("entry_price", entry_price)?
            .column_f64("exit_price", exit_price)?
            .column_f64("size", size)?
            .column_f64("pnl_bp", pnl_bp)?
            .column_ts(
                "exit_time",
                TimestampNanos::new(exit_time.timestamp_nanos_opt().unwrap_or(0)),
            )?
            .at(TimestampNanos::new(
                entry_time.timestamp_nanos_opt().unwrap_or(0),
            ))?;
        g.do_flush()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn write_trade_signal(
        &self,
        account_id: &str,
        base: &str,
        action: &str,          // "entry" | "exit"
        flipster_side: &str,   // "long" | "short"
        size_usd: f64,
        flipster_price: f64,
        gate_price: f64,
        position_id: u64,
        ts: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let mut g = self.inner.lock().await;
        g.buffer
            .table("trade_signal")?
            .symbol("account_id", account_id)?
            .symbol("base", base)?
            .symbol("action", action)?
            .symbol("flipster_side", flipster_side)?
            .column_f64("size_usd", size_usd)?
            .column_f64("flipster_price", flipster_price)?
            .column_f64("gate_price", gate_price)?
            .column_i64("position_id", position_id as i64)?
            .at(TimestampNanos::new(ts.timestamp_nanos_opt().unwrap_or(0)))?;
        g.do_flush()?;
        Ok(())
    }

    pub async fn flush(&self) -> Result<()> {
        let mut g = self.inner.lock().await;
        if g.pending > 0 {
            g.do_flush()?;
        }
        Ok(())
    }
}

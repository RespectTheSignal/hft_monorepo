use crate::ilp::IlpWriter;
use crate::model::{BookTick, ExchangeName};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::warn;

#[derive(Debug, Clone)]
pub struct LatencySignal {
    pub symbol: String,
    pub binance_mid: f64,
    pub flipster_mid: f64,
    pub price_diff_bp: f64,
    pub direction: i32,
    pub binance_ts: DateTime<Utc>,
    pub flipster_ts: DateTime<Utc>,
}

fn normalize(sym: &str) -> String {
    sym.trim_end_matches(".PERP").to_string()
}

#[derive(Default)]
struct SymbolState {
    last_binance_mid: Option<f64>,
    last_flipster: Option<(f64, DateTime<Utc>)>,
    recent: VecDeque<LatencySignal>,
}

pub struct LatencyEngine {
    writer: IlpWriter,
    state: Arc<Mutex<HashMap<String, SymbolState>>>,
    entry_threshold_bp: f64,
    max_recent: usize,
    signal_tx: mpsc::Sender<LatencySignal>,
}

impl LatencyEngine {
    pub fn new(
        writer: IlpWriter,
        entry_threshold_bp: f64,
        signal_tx: mpsc::Sender<LatencySignal>,
    ) -> Arc<Self> {
        Arc::new(Self {
            writer,
            state: Arc::new(Mutex::new(HashMap::new())),
            entry_threshold_bp,
            max_recent: 1000,
            signal_tx,
        })
    }

    pub fn spawn(self: Arc<Self>, mut rx: broadcast::Receiver<BookTick>) {
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(t) => {
                        if let Err(e) = self.on_tick(t).await {
                            warn!(error = %e, "latency on_tick failed");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(missed = n, "latency receiver lagged");
                    }
                    Err(_) => break,
                }
            }
        });
    }

    async fn on_tick(&self, tick: BookTick) -> Result<()> {
        let key = normalize(&tick.symbol);
        let mid = tick.mid();
        if !mid.is_finite() || mid <= 0.0 {
            return Ok(());
        }
        let mut states = self.state.lock().await;
        let st = states.entry(key.clone()).or_default();

        match tick.exchange {
            ExchangeName::Flipster => {
                st.last_flipster = Some((mid, tick.timestamp));
                return Ok(());
            }
            ExchangeName::Binance => {
                let prev = st.last_binance_mid.replace(mid);
                let Some(prev_mid) = prev else { return Ok(()) };
                if prev_mid <= 0.0 {
                    return Ok(());
                }
                let move_bp = ((mid - prev_mid).abs() / prev_mid) * 10_000.0;
                if move_bp < self.entry_threshold_bp {
                    return Ok(());
                }
                let Some((f_mid, f_ts)) = st.last_flipster else {
                    return Ok(());
                };
                if f_mid <= 0.0 {
                    return Ok(());
                }
                let price_diff_bp = ((mid - f_mid) / f_mid) * 10_000.0;
                let direction = if mid > prev_mid { 1 } else { -1 };
                let signal = LatencySignal {
                    symbol: key.clone(),
                    binance_mid: mid,
                    flipster_mid: f_mid,
                    price_diff_bp,
                    direction,
                    binance_ts: tick.timestamp,
                    flipster_ts: f_ts,
                };
                st.recent.push_back(signal.clone());
                if st.recent.len() > self.max_recent {
                    st.recent.pop_front();
                }
                drop(states);
                let _ = self.signal_tx.try_send(signal.clone());
                let latency_ms =
                    (signal.binance_ts - signal.flipster_ts).num_milliseconds() as f64;
                self.writer
                    .write_latency(
                        &signal.symbol,
                        signal.binance_ts,
                        signal.flipster_ts,
                        latency_ms,
                        signal.binance_mid,
                        signal.flipster_mid,
                        signal.price_diff_bp,
                        signal.direction,
                    )
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }
}

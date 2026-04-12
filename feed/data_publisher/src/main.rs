use data_publisher::config::Config;
use data_publisher::error::Result;
use data_publisher::flipster::models::{Ticker, WsMessage};
use data_publisher::flipster::rest::FlipsterRestClient;
use data_publisher::flipster::ws::FlipsterWsClient;
use data_publisher::time_sync;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

const DEFAULT_TOPICS_PER_CONN: usize = 10;

struct CliArgs {
    topics_per_conn: usize,
    max_symbols: Option<usize>,
}

fn parse_args() -> CliArgs {
    let mut topics_per_conn = DEFAULT_TOPICS_PER_CONN;
    let mut max_symbols: Option<usize> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--topics-per-conn" | "-t" => {
                topics_per_conn = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|&n: &usize| n > 0)
                    .unwrap_or_else(|| { eprintln!("-t requires a positive integer"); std::process::exit(1); });
            }
            "--max-symbols" | "-n" => {
                max_symbols = Some(
                    args.next()
                        .and_then(|v| v.parse().ok())
                        .filter(|&n: &usize| n > 0)
                        .unwrap_or_else(|| { eprintln!("-n requires a positive integer"); std::process::exit(1); }),
                );
            }
            "--help" | "-h" => {
                eprintln!("data_publisher — Flipster bookticker latency checker\n");
                eprintln!("Options:");
                eprintln!("  -t, --topics-per-conn <N>  Max topics per WebSocket (default: {DEFAULT_TOPICS_PER_CONN})");
                eprintln!("  -n, --max-symbols <N>      Limit number of symbols to subscribe");
                std::process::exit(0);
            }
            _ => {}
        }
    }
    CliArgs { topics_per_conn, max_symbols }
}

// ---------------------------------------------------------------------------
// Tick event sent from WS tasks → collector
// ---------------------------------------------------------------------------

struct TickEvent {
    symbol: String,
    bid: Option<String>,
    ask: Option<String>,
    latency_us: f64,
    recv_ns: i64,          // local timestamp when message was read
    server_ts_ns: i64,     // server-side timestamp from message
    read_gap_us: f64,      // time since last message on this connection
    conn_id: usize,        // which connection this came from
}

// ---------------------------------------------------------------------------
// Per-symbol latency statistics
// ---------------------------------------------------------------------------

struct LatencyStats {
    count: u64,
    sum_us: f64,
    min_us: f64,
    max_us: f64,
    samples: Vec<f64>,
}

impl LatencyStats {
    fn new() -> Self {
        Self {
            count: 0,
            sum_us: 0.0,
            min_us: f64::MAX,
            max_us: f64::MIN,
            samples: Vec::new(),
        }
    }

    fn record(&mut self, us: f64) {
        self.count += 1;
        self.sum_us += us;
        if us < self.min_us {
            self.min_us = us;
        }
        if us > self.max_us {
            self.max_us = us;
        }
        self.samples.push(us);
    }

    fn avg(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum_us / self.count as f64
        }
    }
}

fn percentile(vals: &[f64], p: f64) -> f64 {
    if vals.is_empty() {
        return 0.0;
    }
    let mut sorted = vals.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = parse_args();

    let config = Config::from_env()?;
    info!(
        "configuration loaded (topics_per_conn={}, max_symbols={:?})",
        cli.topics_per_conn, cli.max_symbols
    );

    let rest = FlipsterRestClient::new(config.clone());

    // ---- 1. Time synchronisation ------------------------------------------
    info!("===== Time Synchronisation =====");
    let clock_offset_ns = time_sync::estimate_clock_offset(&rest, 10).await?;

    // ---- 2. Discover contracts --------------------------------------------
    info!("===== Fetching Contracts =====");
    let contracts = rest.get_contracts(None).await?;
    let mut symbols: Vec<String> = contracts.iter().map(|c| c.symbol.clone()).collect();
    info!("found {} contracts total", symbols.len());

    if let Some(n) = cli.max_symbols {
        symbols.truncate(n);
        info!("limited to {} symbols", symbols.len());
    }

    // ---- 3. Split into chunks and spawn WS tasks --------------------------
    let topic_chunks: Vec<Vec<String>> = symbols
        .chunks(cli.topics_per_conn)
        .map(|chunk| chunk.iter().map(|s| format!("ticker.{s}")).collect())
        .collect();

    let num_conns = topic_chunks.len();
    info!(
        "splitting into {} WebSocket connections ({} topics each, last {})",
        num_conns,
        cli.topics_per_conn,
        topic_chunks.last().map(|c| c.len()).unwrap_or(0),
    );

    let (tx, mut rx) = mpsc::channel::<TickEvent>(4096);

    for (idx, topics) in topic_chunks.into_iter().enumerate() {
        let cfg = config.clone();
        let tx = tx.clone();
        let offset = clock_offset_ns;
        tokio::spawn(async move {
            ws_task(idx, cfg, topics, offset, tx).await;
        });
    }
    drop(tx); // main holds no sender — channel closes when all tasks exit

    // ---- 4. Collect events ------------------------------------------------
    info!("===== Receiving Tickers (Ctrl-C to stop) =====");
    info!(
        "{:<24} {:>14} {:>14} {:>12} {:>12}",
        "symbol", "bid", "ask", "spread", "latency"
    );
    info!("{}", "-".repeat(78));

    let mut stats: HashMap<String, LatencyStats> = HashMap::new();
    let spike_threshold_us: f64 = 60_000.0; // 60ms
    let mut spike_count: u64 = 0;
    let mut spike_with_large_gap: u64 = 0;  // read_gap also large → network/kernel delay
    let mut spike_with_small_gap: u64 = 0;  // read_gap normal → server-side staleness
    let mut spike_details: Vec<(String, f64, f64, usize, i64, i64)> = Vec::new(); // for summary

    loop {
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Some(tick) => {
                        let bid = tick.bid.as_deref().unwrap_or("-");
                        let ask = tick.ask.as_deref().unwrap_or("-");
                        let spread = match (&tick.bid, &tick.ask) {
                            (Some(b), Some(a)) => {
                                let bf: f64 = b.parse().unwrap_or(0.0);
                                let af: f64 = a.parse().unwrap_or(0.0);
                                format!("{:.4}", af - bf)
                            }
                            _ => "-".into(),
                        };

                        if tick.latency_us > spike_threshold_us {
                            spike_count += 1;
                            let gap_threshold_us = 80_000.0; // 80ms — normal gap is ~few ms
                            if tick.read_gap_us > gap_threshold_us {
                                spike_with_large_gap += 1;
                            } else {
                                spike_with_small_gap += 1;
                            }
                            warn!(
                                "SPIKE [ws-{}] {:<24} latency={:.1}ms  read_gap={:.1}ms  server_ts={} recv_ns={}",
                                tick.conn_id,
                                tick.symbol,
                                tick.latency_us / 1000.0,
                                tick.read_gap_us / 1000.0,
                                tick.server_ts_ns,
                                tick.recv_ns,
                            );
                            spike_details.push((
                                tick.symbol.clone(),
                                tick.latency_us,
                                tick.read_gap_us,
                                tick.conn_id,
                                tick.server_ts_ns,
                                tick.recv_ns,
                            ));
                        } else {
                            info!(
                                "{:<24} {:>14} {:>14} {:>12} {:>9.1}\u{00b5}s",
                                tick.symbol, bid, ask, spread, tick.latency_us,
                            );
                        }

                        stats
                            .entry(tick.symbol)
                            .or_insert_with(LatencyStats::new)
                            .record(tick.latency_us);
                    }
                    None => {
                        info!("all websocket tasks exited");
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down...");
                break;
            }
        }
    }

    // ---- Spike diagnosis summary -----------------------------------------
    if spike_count > 0 {
        info!("");
        info!("===== Spike Diagnosis (latency > {:.0}ms) =====", spike_threshold_us / 1000.0);
        info!("total spikes:            {}", spike_count);
        info!("  large read_gap (>80ms): {} → network/kernel delivery delay", spike_with_large_gap);
        info!("  small read_gap (<80ms): {} → server-side stale timestamp", spike_with_small_gap);
        info!("");
        info!(
            "{:<24} {:>10} {:>10} {:>6} {:>20} {:>20}",
            "symbol", "latency", "read_gap", "conn", "server_ts", "recv_ns"
        );
        info!("{}", "-".repeat(94));
        for (sym, lat, gap, conn, sts, rns) in &spike_details {
            info!(
                "{:<24} {:>8.1}ms {:>8.1}ms {:>6} {:>20} {:>20}",
                sym,
                lat / 1000.0,
                gap / 1000.0,
                conn,
                sts,
                rns,
            );
        }
    }

    // ---- 5. Print summary -------------------------------------------------
    print_summary(&stats);

    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket task — one per connection, auto-reconnects
// ---------------------------------------------------------------------------

async fn ws_task(
    id: usize,
    config: Config,
    topics: Vec<String>,
    offset_ns: i64,
    tx: mpsc::Sender<TickEvent>,
) {
    loop {
        info!("[ws-{}] connecting ({} topics)...", id, topics.len());

        let mut ws = match FlipsterWsClient::connect(&config).await {
            Ok(ws) => ws,
            Err(e) => {
                error!("[ws-{}] connect failed: {e}, retrying in 3s", id);
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                continue;
            }
        };

        if let Err(e) = ws.subscribe(&topics).await {
            error!("[ws-{}] subscribe failed: {e}, retrying in 3s", id);
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            continue;
        }

        info!("[ws-{}] streaming", id);

        // Read loop
        let mut last_recv_ns: i64 = 0;
        loop {
            match ws.next_message().await {
                Some(Ok(Message::Text(text))) => {
                    let recv_ns = time_sync::local_now_ns();
                    let read_gap_us = if last_recv_ns == 0 {
                        0.0
                    } else {
                        (recv_ns - last_recv_ns) as f64 / 1_000.0
                    };
                    last_recv_ns = recv_ns;

                    if let Some(events) = parse_tickers(&text, recv_ns, offset_ns, read_gap_us, id) {
                        for ev in events {
                            if tx.send(ev).await.is_err() {
                                return; // receiver dropped → shutdown
                            }
                        }
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    warn!("[ws-{}] closed: {:?}", id, frame);
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    warn!("[ws-{}] error: {e}", id);
                    break;
                }
                None => {
                    warn!("[ws-{}] stream ended", id);
                    break;
                }
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

// ---------------------------------------------------------------------------
// Parse ticker messages into TickEvents
// ---------------------------------------------------------------------------

fn parse_tickers(
    text: &str,
    recv_ns: i64,
    offset_ns: i64,
    read_gap_us: f64,
    conn_id: usize,
) -> Option<Vec<TickEvent>> {
    let msg: WsMessage = serde_json::from_str(text).ok()?;

    let server_ts: i64 = msg.ts.parse().ok()?;
    let latency_us = (recv_ns - server_ts + offset_ns) as f64 / 1_000.0;

    let mut events = Vec::new();
    for data in &msg.data {
        for row in &data.rows {
            if let Ok(ticker) = serde_json::from_value::<Ticker>(row.clone()) {
                events.push(TickEvent {
                    symbol: ticker.symbol,
                    bid: ticker.bid_price,
                    ask: ticker.ask_price,
                    latency_us,
                    recv_ns,
                    server_ts_ns: server_ts,
                    read_gap_us,
                    conn_id,
                });
            }
        }
    }

    if events.is_empty() {
        None
    } else {
        Some(events)
    }
}

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

fn print_summary(stats: &HashMap<String, LatencyStats>) {
    if stats.is_empty() {
        return;
    }

    info!("");
    info!("===== Latency Summary =====");
    info!(
        "{:<24} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "symbol", "count", "min(us)", "avg(us)", "p50(us)", "p99(us)", "max(us)"
    );
    info!("{}", "-".repeat(84));

    let mut entries: Vec<_> = stats.iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    for (sym, s) in &entries {
        let p50 = percentile(&s.samples, 50.0);
        let p99 = percentile(&s.samples, 99.0);
        info!(
            "{:<24} {:>8} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1}",
            sym, s.count, s.min_us, s.avg(), p50, p99, s.max_us,
        );
    }

    // aggregate
    let all: Vec<f64> = stats
        .values()
        .flat_map(|s| s.samples.iter().copied())
        .collect();
    if !all.is_empty() {
        let total: f64 = all.iter().sum();
        let min = all.iter().cloned().fold(f64::MAX, f64::min);
        let max = all.iter().cloned().fold(f64::MIN, f64::max);
        info!("{}", "-".repeat(84));
        info!(
            "{:<24} {:>8} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1}",
            "ALL",
            all.len(),
            min,
            total / all.len() as f64,
            percentile(&all, 50.0),
            percentile(&all, 99.0),
            max,
        );
    }
}

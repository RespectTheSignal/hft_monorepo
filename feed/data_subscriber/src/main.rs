mod ipc_server;
mod protocol;

use ipc_server::IpcServer;
use protocol::{parse_topic, stamp_recv_ns, BOOKTICKER_SIZE};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};

fn now_ns() -> i64 {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    ts.as_nanos() as i64
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    dotenvy::dotenv().ok();

    let publisher_ip =
        std::env::var("DATA_SERVER_IP").unwrap_or_else(|_| "127.0.0.1".to_string());
    let flipster_port =
        std::env::var("FLIPSTER_DATA_PORT").unwrap_or_else(|_| "7000".to_string());
    let ipc_socket_path = std::env::var("IPC_SOCKET_PATH")
        .unwrap_or_else(|_| "/tmp/flipster_data_subscriber.sock".to_string());

    let zmq_addr = format!("tcp://{publisher_ip}:{flipster_port}");

    // ---- ZMQ SUB socket ---------------------------------------------------
    let ctx = zmq::Context::new();
    let sub = ctx.socket(zmq::SUB).expect("ZMQ SUB socket");
    sub.set_rcvhwm(100_000).expect("set rcvhwm");
    sub.set_rcvbuf(4 * 1024 * 1024).expect("set rcvbuf");
    sub.connect(&zmq_addr)
        .unwrap_or_else(|e| panic!("ZMQ connect to {zmq_addr}: {e}"));
    sub.set_subscribe(b"flipster_bookticker_")
        .expect("set subscribe");

    info!("ZMQ SUB connected to {zmq_addr}");
    info!("subscribed to: flipster_bookticker_*");

    // ---- IPC server -------------------------------------------------------
    let ipc = IpcServer::new(&ipc_socket_path);
    info!("IPC server ready at {}", ipc.socket_path());

    // ---- Ctrl-C handler ---------------------------------------------------
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::Relaxed);
    })
    .expect("set ctrl-c handler");

    // ---- Main loop --------------------------------------------------------
    info!("===== data_subscriber running (Ctrl-C to stop) =====");

    let mut total_received: u64 = 0;
    let mut total_forwarded: u64 = 0;
    let mut last_log = std::time::Instant::now();
    let mut src_pub_samples: Vec<f64> = Vec::new();
    let mut pub_sub_samples: Vec<f64> = Vec::new();

    while running.load(Ordering::Relaxed) {
        // Poll with 100ms timeout so we can check the running flag
        let poll_result = sub.poll(zmq::POLLIN, 100);
        if poll_result.unwrap_or(0) == 0 {
            continue;
        }

        // Drain all available messages (non-blocking)
        loop {
            // Receive multipart: [topic][payload]
            let topic = match sub.recv_bytes(zmq::DONTWAIT) {
                Ok(t) => t,
                Err(zmq::Error::EAGAIN) => break,
                Err(e) => {
                    error!("ZMQ recv error: {e}");
                    break;
                }
            };

            let has_more = sub.get_rcvmore().unwrap_or(false);
            if !has_more {
                warn!("received topic without payload, skipping");
                continue;
            }

            let mut payload = match sub.recv_bytes(0) {
                Ok(p) => p,
                Err(e) => {
                    error!("ZMQ recv payload error: {e}");
                    continue;
                }
            };

            total_received += 1;

            if payload.len() < BOOKTICKER_SIZE {
                warn!("payload too small: {} < {BOOKTICKER_SIZE}", payload.len());
                continue;
            }

            // Stamp subscriber receive timestamp
            let recv_ns = now_ns();
            stamp_recv_ns(&mut payload, recv_ns);

            // Broadcast to IPC clients
            let n = ipc.broadcast(&topic, &payload);
            if n > 0 {
                total_forwarded += 1;
            }

            // Latency sampling (every 100th message)
            if total_received % 100 == 0 && payload.len() >= BOOKTICKER_SIZE {
                // server_ts_ns=72, publisher_recv_ns=80, publisher_sent_ns=88
                let server_ts_ns =
                    i64::from_le_bytes(payload[72..80].try_into().unwrap());
                let pub_recv_ns =
                    i64::from_le_bytes(payload[80..88].try_into().unwrap());
                let pub_sent_ns =
                    i64::from_le_bytes(payload[88..96].try_into().unwrap());

                let src_pub_us = (pub_recv_ns - server_ts_ns) as f64 / 1_000.0;
                let pub_sub_us = (recv_ns - pub_sent_ns) as f64 / 1_000.0;
                src_pub_samples.push(src_pub_us);
                pub_sub_samples.push(pub_sub_us);
            }
        }

        // Periodic status log
        if last_log.elapsed().as_secs() >= 5 {
            let src_pub_str = if src_pub_samples.is_empty() {
                "-".to_string()
            } else {
                src_pub_samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let p50 = percentile(&src_pub_samples, 50.0);
                let p99 = percentile(&src_pub_samples, 99.0);
                src_pub_samples.clear();
                format!("p50={p50:.0}us p99={p99:.0}us")
            };

            let pub_sub_str = if pub_sub_samples.is_empty() {
                "-".to_string()
            } else {
                pub_sub_samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let p50 = percentile(&pub_sub_samples, 50.0);
                let p99 = percentile(&pub_sub_samples, 99.0);
                pub_sub_samples.clear();
                format!("p50={p50:.0}us p99={p99:.0}us")
            };

            info!(
                "received={total_received} forwarded={total_forwarded} clients={} | src->pub: {src_pub_str} | pub->sub: {pub_sub_str}",
                ipc.client_count()
            );
            last_log = std::time::Instant::now();
        }
    }

    info!("shutting down... (received={total_received}, forwarded={total_forwarded})");
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

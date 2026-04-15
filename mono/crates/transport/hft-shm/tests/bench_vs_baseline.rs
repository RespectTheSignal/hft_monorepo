//! SHM 지연 측정 — `cargo test --release -- --nocapture` 로 실행.
//!
//! criterion 의존성을 피하기 위해 순수 `std::time` 기반. 비교 대상으로 Linux
//! **unix datagram socket (SOCK_DGRAM, AF_UNIX)** 을 쓴다. UDS 가 문제상 가장
//! 가까운 "비SHM IPC" 기준선이다. ZMQ 는 별 프로세스/빌드가 필요해 생략.
//!
//! 측정 지표:
//! - **publish→consume 1-hop latency**, 같은 프로세스 두 스레드 간.
//! - 반복 수 100k, warm-up 5k.
//! - 중앙값 / p99 / p99.9.
//!
//! 기대 결과 (대략, 3.5GHz x86, busy-poll 격리 코어):
//! - SHM trade ring : p50 ~50-200ns, p99 ~500ns-1μs
//! - UDS SOCK_DGRAM : p50 ~5-12μs, p99 ~30μs+

use std::hint;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use hft_shm::*;
use tempfile::tempdir;

const ITERS: usize = 100_000;
const WARMUP: usize = 5_000;

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn summary(name: &str, mut ns: Vec<u64>) {
    ns.sort_unstable();
    let sum: u128 = ns.iter().map(|&x| x as u128).sum();
    let mean = (sum / ns.len() as u128) as u64;
    println!(
        "[{:<20}] n={:>7} mean={:>7}ns p50={:>7}ns p90={:>7}ns p99={:>7}ns p99.9={:>7}ns max={:>7}ns",
        name,
        ns.len(),
        mean,
        percentile(&ns, 0.50),
        percentile(&ns, 0.90),
        percentile(&ns, 0.99),
        percentile(&ns, 0.999),
        ns.last().copied().unwrap_or(0),
    );
}

#[test]
#[ignore] // `cargo test --release -- --ignored --nocapture` 로만 실행.
fn bench_trade_ring_1hop() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bench_trades");
    let w = Arc::new(TradeRingWriter::create(&path, 1 << 16).unwrap());
    let mut r = TradeRingReader::open(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    let w_c = w.clone();

    // 현재 시각 (ns) 을 event_ns 에 담아 보내고 reader 가 Instant::now() 대비.
    let writer = thread::spawn(move || {
        let mut i = 0u64;
        while !stop_w.load(Ordering::Relaxed) {
            let now = monotonic_ns();
            let f = TradeFrame {
                seq: AtomicU64::new(0),
                exchange_id: 2,
                _pad1: [0; 3],
                symbol_idx: 0,
                price: i as i64,
                size: 1,
                trade_id: i as i64,
                event_ns: 0,
                recv_ns: 0,
                pub_ns: now,
                flags: 0,
                _pad2: [0; 28],
            };
            w_c.publish(&f);
            i += 1;
            // 느슨한 간격 — 소비자가 busy-poll 이라 이 없어도 OK.
            for _ in 0..32 {
                hint::spin_loop();
            }
        }
    });

    let mut samples = Vec::with_capacity(ITERS);
    let mut seen = 0usize;
    // warmup
    while seen < WARMUP {
        if r.try_consume().is_some() {
            seen += 1;
        } else {
            hint::spin_loop();
        }
    }
    // measure
    while samples.len() < ITERS {
        if let Some(f) = r.try_consume() {
            let now = monotonic_ns();
            samples.push(now.saturating_sub(f.pub_ns));
        } else {
            hint::spin_loop();
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    summary("SHM trade 1-hop", samples);
}

#[test]
#[ignore]
fn bench_quote_slot_read() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("bench_quotes");
    let w = QuoteSlotWriter::create(&path, 256).unwrap();
    let r = QuoteSlotReader::open(&path).unwrap();

    // writer 스레드 — 동일 slot 을 계속 갱신.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    let path_w = path.clone();
    let writer = thread::spawn(move || {
        let w = QuoteSlotWriter::create(&path_w, 256).unwrap();
        let mut i = 0i64;
        while !stop_w.load(Ordering::Relaxed) {
            w.publish(
                7,
                &QuoteUpdate {
                    exchange_id: 1,
                    bid_price: 1000 + i,
                    bid_size: 1,
                    ask_price: 1001 + i,
                    ask_size: 1,
                    event_ns: 0,
                    recv_ns: 0,
                    pub_ns: monotonic_ns(),
                },
            )
            .unwrap();
            i += 1;
            for _ in 0..16 {
                hint::spin_loop();
            }
        }
    });
    drop(w);

    // warmup
    for _ in 0..WARMUP {
        let _ = r.read(7);
    }

    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        if let Some(s) = r.read(7) {
            let now = monotonic_ns();
            samples.push(now.saturating_sub(s.pub_ns));
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    summary("SHM quote read", samples);
}

#[test]
#[ignore]
fn bench_uds_dgram_1hop() {
    use std::os::unix::net::UnixDatagram;
    let dir = tempdir().unwrap();
    let path_a = dir.path().join("uds_a");
    let path_b = dir.path().join("uds_b");
    let tx = UnixDatagram::bind(&path_a).unwrap();
    let rx = UnixDatagram::bind(&path_b).unwrap();
    tx.connect(&path_b).unwrap();
    rx.set_nonblocking(false).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_w = stop.clone();
    let writer = thread::spawn(move || {
        let mut i = 0u64;
        while !stop_w.load(Ordering::Relaxed) {
            let mut buf = [0u8; 128];
            let now = monotonic_ns().to_le_bytes();
            buf[..8].copy_from_slice(&now);
            buf[8..16].copy_from_slice(&i.to_le_bytes());
            let _ = tx.send(&buf);
            i += 1;
            for _ in 0..16 {
                hint::spin_loop();
            }
        }
    });

    let mut buf = [0u8; 128];
    // warmup
    for _ in 0..WARMUP {
        let _ = rx.recv(&mut buf);
    }

    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        let n = rx.recv(&mut buf).unwrap();
        if n >= 8 {
            let sent_ns = u64::from_le_bytes(buf[..8].try_into().unwrap());
            let now = monotonic_ns();
            samples.push(now.saturating_sub(sent_ns));
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = writer.join();
    summary("UDS dgram 1-hop", samples);
}

#[inline(always)]
fn monotonic_ns() -> u64 {
    // Instant 를 u64 ns 로 근사: std 에는 직접 접근이 없으므로 delta 계산용.
    // 여기선 절댓값 시각이 필요하므로 CLOCK_MONOTONIC_RAW 를 libc 로 읽는다.
    #[cfg(target_os = "linux")]
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }
    #[cfg(not(target_os = "linux"))]
    {
        // non-linux 는 fallback — duration_since(시작점) 이 필요하지만 여기선
        // 측정 용도라 epoch-based 로도 무방.
        use std::time::UNIX_EPOCH;
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    }
}

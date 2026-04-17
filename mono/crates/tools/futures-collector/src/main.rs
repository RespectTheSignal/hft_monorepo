//! futures-collector — SUB 스트림을 소비해 시간단위 JSONL 파일로 아카이브.
//!
//! # 배경
//! 레거시 `futures_collector` 는 WebSocket 을 직접 열어 parquet 로 저장하던 분리된
//! 프로세스였다. Phase 1 에선 publisher ↔ subscriber ↔ sink 파이프라인이 통일됐으니
//! 이 도구는 그 파이프라인 위에 **offline 아카이빙** 역할만 남긴다.
//!
//! # 산출 포맷
//! - `out_dir/{YYYY}-{MM}-{DD}/{HH}.jsonl` 에 1 line 1 event JSON 으로 append.
//! - 파일 핸들은 "현재 시각의 hour" 기준으로 rotate. hour 바뀌면 close + 새 파일 open.
//! - JSON 스키마는 MarketEvent 바리안트마다:
//!     - bookticker / webbookticker:
//!       `{"type":"bookticker","exchange":"gate","symbol":"BTC_USDT",
//!         "bid_price":100.5,"ask_price":100.6,"bid_size":1.0,"ask_size":2.0,
//!         "event_time_ms":...,"server_time_ms":...,"ingest_ms":...}`
//!     - trade:
//!       `{"type":"trade",...,"trade_id":..,"is_internal":false}`
//! - 타임존은 UTC 로 고정 (Hive 파티션 호환).
//!
//! # 설정
//! - `HFT_COLLECTOR_OUT_DIR` 환경변수로 출력 디렉토리 지정. 기본값: `./collected`.
//! - 디렉토리는 실행 시점에 mkdir -p 된다.
//!
//! # 종료
//! SIGINT/SIGTERM → subscriber cancel → 채널 disconnect → writer 가 남은 이벤트
//! drain 후 현재 파일 flush + close.

#![deny(rust_2018_idioms)]

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, Timelike, Utc};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use hft_config::AppConfig;
use hft_exchange_api::CancellationToken;
use hft_time::LatencyStamps;
use hft_types::{BookTicker, MarketEvent, Trade};
use subscriber::{start as subscriber_start, InprocQueue, INPROC_QUEUE_CAPACITY};
use tracing::{error, info, warn};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// 환경변수: 출력 디렉토리. 없으면 `./collected`.
const ENV_OUT_DIR: &str = "HFT_COLLECTOR_OUT_DIR";

/// Writer recv_timeout — 이 간격마다 hour-rotate 체크 + flush.
const RECV_TIMEOUT_MS: u64 = 100;

fn init_telemetry(cfg: &AppConfig) -> Result<hft_telemetry::TelemetryHandle> {
    let tcfg = hft_telemetry::TelemetryConfig {
        otlp_endpoint: cfg.telemetry.otlp_endpoint.clone(),
        default_level: cfg.telemetry.default_level.clone(),
        json_logs: cfg.telemetry.stdout_json,
    };
    hft_telemetry::init(&tcfg, &cfg.service_name).context("telemetry init failed")
}

fn install_signal_handler(cancel: CancellationToken) {
    let fired = Arc::new(AtomicBool::new(false));
    let install_result = ctrlc::set_handler(move || {
        if fired.swap(true, Ordering::SeqCst) {
            eprintln!("[futures-collector] second signal — aborting");
            std::process::exit(130);
        }
        eprintln!("[futures-collector] shutdown signal — draining");
        cancel.cancel();
    });
    if let Err(e) = install_result {
        warn!(error = %e, "failed to install ctrlc handler");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Rolling file writer
// ─────────────────────────────────────────────────────────────────────────────

/// 현재 시각의 hour 가 바뀌면 파일을 새로 여는 writer.
struct RollingWriter {
    out_root: PathBuf,
    /// 현재 연-월-일-시 (UTC). None 이면 아직 파일이 열리지 않음.
    current_hour: Option<(i32, u32, u32, u32)>,
    file: Option<BufWriter<File>>,
}

impl RollingWriter {
    fn new(out_root: PathBuf) -> Self {
        Self {
            out_root,
            current_hour: None,
            file: None,
        }
    }

    /// 현재 시각에 맞춰 rotate. 필요 시 디렉토리 mkdir -p.
    fn ensure_file(&mut self, now: DateTime<Utc>) -> Result<()> {
        let hour = (now.year(), now.month(), now.day(), now.hour());
        if self.current_hour == Some(hour) {
            return Ok(());
        }

        // 이전 파일 close — BufWriter drop 시 flush 가 따로 호출되므로 명시적으로.
        if let Some(f) = self.file.as_mut() {
            if let Err(e) = f.flush() {
                warn!(error = %e, "flush before rotate failed");
            }
        }
        self.file = None;

        let dir = self
            .out_root
            .join(format!("{:04}-{:02}-{:02}", hour.0, hour.1, hour.2));
        std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
        let path = dir.join(format!("{:02}.jsonl", hour.3));

        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        self.file = Some(BufWriter::with_capacity(64 * 1024, f));
        self.current_hour = Some(hour);
        info!(path = %path.display(), "rolled over to new file");
        Ok(())
    }

    fn write_line(&mut self, line: &str) -> Result<()> {
        let f = self
            .file
            .as_mut()
            .ok_or_else(|| anyhow!("no file open (forgot ensure_file?)"))?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }

    fn flush(&mut self) {
        if let Some(f) = self.file.as_mut() {
            if let Err(e) = f.flush() {
                warn!(error = %e, "final flush failed");
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JSON encoding — 수동, 빠르고 의존성 최소 (serde 파생 없이 serde_json::json! 사용)
// ─────────────────────────────────────────────────────────────────────────────

/// MarketEvent → JSON 라인 문자열.
fn encode_event(ev: &MarketEvent, stamps: &LatencyStamps) -> String {
    let ingest_ms = stamps.ws_received.wall_ms;
    match ev {
        MarketEvent::BookTicker(b) => bookticker_json(b, ingest_ms, "bookticker"),
        MarketEvent::WebBookTicker(b) => bookticker_json(b, ingest_ms, "webbookticker"),
        MarketEvent::Trade(t) => trade_json(t, ingest_ms),
    }
}

fn bookticker_json(b: &BookTicker, ingest_ms: i64, ty: &str) -> String {
    serde_json::json!({
        "type": ty,
        "exchange": b.exchange.as_str(),
        "symbol": b.symbol.as_str(),
        "bid_price": b.bid_price.0,
        "ask_price": b.ask_price.0,
        "bid_size": b.bid_size.0,
        "ask_size": b.ask_size.0,
        "event_time_ms": b.event_time_ms,
        "server_time_ms": b.server_time_ms,
        "ingest_ms": ingest_ms,
    })
    .to_string()
}

fn trade_json(t: &Trade, ingest_ms: i64) -> String {
    serde_json::json!({
        "type": "trade",
        "exchange": t.exchange.as_str(),
        "symbol": t.symbol.as_str(),
        "price": t.price.0,
        "size": t.size.0,
        "trade_id": t.trade_id,
        "create_time_s": t.create_time_s,
        "create_time_ms": t.create_time_ms,
        "server_time_ms": t.server_time_ms,
        "is_internal": t.is_internal,
        "ingest_ms": ingest_ms,
    })
    .to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// Writer loop (blocking — 파일 I/O 이므로 spawn_blocking 에서 실행)
// ─────────────────────────────────────────────────────────────────────────────

fn writer_loop(
    rx: Receiver<(MarketEvent, LatencyStamps)>,
    cancel: CancellationToken,
    out_dir: PathBuf,
) {
    info!(target: "futures_collector", out = %out_dir.display(), "writer started");
    let mut writer = RollingWriter::new(out_dir);

    loop {
        if cancel.is_cancelled() && rx.is_empty() {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(RECV_TIMEOUT_MS)) {
            Ok((ev, stamps)) => {
                if let Err(e) = writer.ensure_file(Utc::now()) {
                    warn!(error = %e, "rollover failed — dropping event");
                    continue;
                }
                let line = encode_event(&ev, &stamps);
                if let Err(e) = writer.write_line(&line) {
                    warn!(error = %e, "write_line failed");
                }
                // 배치 drain — 들어와 있는 이벤트는 모두 소비해 같은 파일에 기록.
                while let Ok((ev, stamps)) = rx.try_recv() {
                    let line = encode_event(&ev, &stamps);
                    if let Err(e) = writer.write_line(&line) {
                        warn!(error = %e, "write_line(drain) failed");
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // hour-rotate 체크용 tick — 이벤트 없어도 파일 flush.
                writer.flush();
            }
            Err(RecvTimeoutError::Disconnected) => {
                info!(target: "futures_collector", "channel disconnected — exit");
                break;
            }
        }
    }
    writer.flush();
    info!(target: "futures_collector", "writer stopped");
}

fn out_dir_from_env() -> PathBuf {
    std::env::var(ENV_OUT_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(".").join("collected"))
}

async fn run() -> Result<()> {
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;
    let _tele = init_telemetry(&cfg)?;

    let out_dir = out_dir_from_env();
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("create out dir {}", out_dir.display()))?;

    info!(
        service = %cfg.service_name,
        out_dir = %out_dir.display(),
        sub = %cfg.zmq.sub_endpoint,
        exchanges = cfg.exchanges.len(),
        "futures-collector starting"
    );

    let (queue, rx) = InprocQueue::bounded(INPROC_QUEUE_CAPACITY);
    let downstream = Arc::new(queue);
    let sub_handle = subscriber_start(cfg.clone(), downstream)
        .await
        .context("subscriber::start failed")?;

    install_signal_handler(sub_handle.cancel.clone());

    let writer_cancel = sub_handle.cancel.child_token();
    let writer_task = tokio::task::spawn_blocking(move || writer_loop(rx, writer_cancel, out_dir));

    sub_handle.join().await;
    if let Err(e) = writer_task.await {
        warn!(error = ?e, "writer task join error");
    }

    info!("futures-collector exited cleanly");
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[futures-collector] fatal: {e:#}");
            error!(error = %format!("{e:#}"), "futures-collector fatal");
            ExitCode::from(1)
        }
    }
}

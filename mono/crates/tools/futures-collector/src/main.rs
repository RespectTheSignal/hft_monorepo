//! futures-collector — SUB 스트림을 소비해 시간단위 Parquet Hive 파티션으로 아카이브.
//!
//! # 배경
//! 레거시 `futures_collector` 는 분석 파이프라인이 바로 읽을 수 있도록 Parquet 를
//! 썼다. Rust 버전 초기 스캐폴드는 JSONL 로 최소 경로만 연결했지만, 운영/분석
//! 측면에선 Hive 파티션된 columnar 포맷이 더 적합하므로 Phase 3 에서 Parquet 로
//! 올린다.
//!
//! # 산출 포맷
//! - `type=bookticker/year=YYYY/month=MM/day=DD/hour=HH/data.parquet`
//! - `type=trade/year=YYYY/month=MM/day=DD/hour=HH/data.parquet`
//! - UTC 기준 hour rotate.
//! - 파티션 키(type/year/month/day/hour)는 디렉토리에만 두고 파일 컬럼에는 중복 저장하지 않는다.
//!
//! # 종료
//! SIGINT/SIGTERM → subscriber cancel → writer thread 가 남은 버퍼를 flush 후 종료.

#![deny(rust_2018_idioms)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use arrow::array::{ArrayRef, BooleanArray, Float64Array, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use chrono::{DateTime, Datelike, Timelike, Utc};
use clap::Parser;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use hft_config::AppConfig;
use hft_exchange_api::CancellationToken;
use hft_time::LatencyStamps;
use hft_types::MarketEvent;
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::{WriterProperties, WriterVersion};
use subscriber::{start as subscriber_start, InprocQueue, INPROC_QUEUE_CAPACITY};
use tracing::{error, info, warn};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// writer recv timeout — 이 간격마다 rotate 재시도/idle tick 을 수행.
const RECV_TIMEOUT_MS: u64 = 100;

type HourKey = (i32, u32, u32, u32);

/// CLI — latency-probe 스타일 clap derive 패턴을 그대로 따른다.
#[derive(Debug, Parser)]
#[command(
    name = "futures-collector",
    about = "SUB 스트림을 Parquet Hive 파티션으로 아카이브"
)]
struct Cli {
    /// 출력 디렉토리.
    #[arg(long, env = "HFT_COLLECTOR_OUT_DIR", default_value = "./collected")]
    out_dir: PathBuf,

    /// Parquet 압축 방식: zstd | snappy | none.
    #[arg(long, default_value = "zstd")]
    compression: String,

    /// ZSTD 압축 레벨 (zstd 선택 시만 의미 있음).
    #[arg(long, default_value_t = 3)]
    zstd_level: i32,
}

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

/// 컬럼형 저장 전용 flat row. Hive 파티션 키는 디렉토리에만 둔다.
#[derive(Debug, Clone, PartialEq)]
struct BookTickerRow {
    exchange: String,
    symbol: String,
    bid_price: f64,
    ask_price: f64,
    bid_size: f64,
    ask_size: f64,
    event_time_ms: i64,
    server_time_ms: i64,
    ingest_ms: i64,
}

/// Trade Parquet row.
#[derive(Debug, Clone, PartialEq)]
struct TradeRow {
    exchange: String,
    symbol: String,
    price: f64,
    size: f64,
    trade_id: i64,
    create_time_s: i64,
    create_time_ms: i64,
    server_time_ms: i64,
    is_internal: bool,
    ingest_ms: i64,
}

fn bookticker_schema() -> Schema {
    Schema::new(vec![
        Field::new("exchange", DataType::Utf8, false),
        Field::new("symbol", DataType::Utf8, false),
        Field::new("bid_price", DataType::Float64, false),
        Field::new("ask_price", DataType::Float64, false),
        Field::new("bid_size", DataType::Float64, false),
        Field::new("ask_size", DataType::Float64, false),
        Field::new("event_time_ms", DataType::Int64, false),
        Field::new("server_time_ms", DataType::Int64, false),
        Field::new("ingest_ms", DataType::Int64, false),
    ])
}

fn trade_schema() -> Schema {
    Schema::new(vec![
        Field::new("exchange", DataType::Utf8, false),
        Field::new("symbol", DataType::Utf8, false),
        Field::new("price", DataType::Float64, false),
        Field::new("size", DataType::Float64, false),
        Field::new("trade_id", DataType::Int64, false),
        Field::new("create_time_s", DataType::Int64, false),
        Field::new("create_time_ms", DataType::Int64, false),
        Field::new("server_time_ms", DataType::Int64, false),
        Field::new("is_internal", DataType::Boolean, false),
        Field::new("ingest_ms", DataType::Int64, false),
    ])
}

#[inline]
fn hour_key(now: DateTime<Utc>) -> HourKey {
    (now.year(), now.month(), now.day(), now.hour())
}

fn partition_dir(root: &Path, ty: &str, hour: HourKey) -> PathBuf {
    root.join(format!(
        "type={ty}/year={}/month={:02}/day={:02}/hour={:02}",
        hour.0, hour.1, hour.2, hour.3
    ))
}

fn parquet_path(root: &Path, ty: &str, hour: HourKey) -> PathBuf {
    partition_dir(root, ty, hour).join("data.parquet")
}

fn writer_properties(compression: &str, zstd_level: i32) -> Result<WriterProperties> {
    let compression = match compression.to_ascii_lowercase().as_str() {
        "zstd" => Compression::ZSTD(
            ZstdLevel::try_new(zstd_level).map_err(|e| anyhow!("invalid zstd level: {e}"))?,
        ),
        "snappy" => Compression::SNAPPY,
        "none" => Compression::UNCOMPRESSED,
        other => {
            return Err(anyhow!(
                "unsupported compression `{other}` (expected zstd|snappy|none)"
            ))
        }
    };
    Ok(WriterProperties::builder()
        .set_compression(compression)
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_max_row_group_size(100_000)
        .set_data_page_size_limit(1024 * 1024)
        .build())
}

/// 시간단위 Hive 파티션 Parquet writer.
struct ParquetRollingWriter {
    out_root: PathBuf,
    current_hour: Option<HourKey>,
    bt_buffer: Vec<BookTickerRow>,
    trade_buffer: Vec<TradeRow>,
    write_props: WriterProperties,
}

impl ParquetRollingWriter {
    fn new(out_root: PathBuf, write_props: WriterProperties) -> Self {
        Self {
            out_root,
            current_hour: None,
            bt_buffer: Vec::with_capacity(50_000),
            trade_buffer: Vec::with_capacity(50_000),
            write_props,
        }
    }

    /// hour 변경 감지 → 이전 hour flush. flush 실패 시 current_hour 를 바꾸지 않아 다음 tick 에 재시도한다.
    fn maybe_rotate(&mut self, now: DateTime<Utc>) -> Result<()> {
        let next = hour_key(now);
        match self.current_hour {
            None => {
                self.current_hour = Some(next);
                Ok(())
            }
            Some(cur) if cur == next => Ok(()),
            Some(_) => {
                self.flush_buffers()?;
                self.current_hour = Some(next);
                Ok(())
            }
        }
    }

    fn push_event(&mut self, ev: &MarketEvent, stamps: &LatencyStamps) {
        let ingest_ms = stamps.ws_received.wall_ms;
        match ev {
            MarketEvent::BookTicker(b) | MarketEvent::WebBookTicker(b) => {
                self.bt_buffer.push(BookTickerRow {
                    exchange: b.exchange.as_str().to_owned(),
                    symbol: b.symbol.as_str().to_owned(),
                    bid_price: b.bid_price.0,
                    ask_price: b.ask_price.0,
                    bid_size: b.bid_size.0,
                    ask_size: b.ask_size.0,
                    event_time_ms: b.event_time_ms,
                    server_time_ms: b.server_time_ms,
                    ingest_ms,
                });
            }
            MarketEvent::Trade(t) => {
                self.trade_buffer.push(TradeRow {
                    exchange: t.exchange.as_str().to_owned(),
                    symbol: t.symbol.as_str().to_owned(),
                    price: t.price.0,
                    size: t.size.0,
                    trade_id: t.trade_id,
                    create_time_s: t.create_time_s,
                    create_time_ms: t.create_time_ms,
                    server_time_ms: t.server_time_ms,
                    is_internal: t.is_internal,
                    ingest_ms,
                });
            }
        }
    }

    /// buffer 를 Parquet 로 flush. 실패한 버퍼는 clear 하지 않고 유지해 다음 rotate/종료 flush 에 재시도한다.
    fn flush_buffers(&mut self) -> Result<()> {
        let hour = self
            .current_hour
            .ok_or_else(|| anyhow!("flush_buffers without current_hour"))?;
        let mut first_error: Option<anyhow::Error> = None;

        if !self.bt_buffer.is_empty() {
            let path = parquet_path(&self.out_root, "bookticker", hour);
            match self.write_bookticker_parquet(&path) {
                Ok(()) => {
                    info!(path = %path.display(), rows = self.bt_buffer.len(), "wrote bookticker parquet");
                    self.bt_buffer.clear();
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "bookticker parquet write failed — buffer retained");
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }

        if !self.trade_buffer.is_empty() {
            let path = parquet_path(&self.out_root, "trade", hour);
            match self.write_trade_parquet(&path) {
                Ok(()) => {
                    info!(path = %path.display(), rows = self.trade_buffer.len(), "wrote trade parquet");
                    self.trade_buffer.clear();
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "trade parquet write failed — buffer retained");
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }

        if let Some(e) = first_error {
            Err(e)
        } else {
            Ok(())
        }
    }

    fn flush_all(&mut self) {
        if let Err(e) = self.flush_buffers() {
            warn!(error = %e, "final parquet flush failed");
        }
    }

    fn write_bookticker_parquet(&self, path: &Path) -> Result<()> {
        let schema = Arc::new(bookticker_schema());
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(
                    self.bt_buffer
                        .iter()
                        .map(|r| r.exchange.clone())
                        .collect::<Vec<_>>(),
                )) as ArrayRef,
                Arc::new(StringArray::from(
                    self.bt_buffer
                        .iter()
                        .map(|r| r.symbol.clone())
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Float64Array::from(
                    self.bt_buffer
                        .iter()
                        .map(|r| r.bid_price)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Float64Array::from(
                    self.bt_buffer
                        .iter()
                        .map(|r| r.ask_price)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Float64Array::from(
                    self.bt_buffer
                        .iter()
                        .map(|r| r.bid_size)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Float64Array::from(
                    self.bt_buffer
                        .iter()
                        .map(|r| r.ask_size)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    self.bt_buffer
                        .iter()
                        .map(|r| r.event_time_ms)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    self.bt_buffer
                        .iter()
                        .map(|r| r.server_time_ms)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    self.bt_buffer
                        .iter()
                        .map(|r| r.ingest_ms)
                        .collect::<Vec<_>>(),
                )),
            ],
        )
        .context("build bookticker record batch")?;
        self.write_batch(path, batch)
    }

    fn write_trade_parquet(&self, path: &Path) -> Result<()> {
        let schema = Arc::new(trade_schema());
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(
                    self.trade_buffer
                        .iter()
                        .map(|r| r.exchange.clone())
                        .collect::<Vec<_>>(),
                )) as ArrayRef,
                Arc::new(StringArray::from(
                    self.trade_buffer
                        .iter()
                        .map(|r| r.symbol.clone())
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Float64Array::from(
                    self.trade_buffer
                        .iter()
                        .map(|r| r.price)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Float64Array::from(
                    self.trade_buffer.iter().map(|r| r.size).collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    self.trade_buffer
                        .iter()
                        .map(|r| r.trade_id)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    self.trade_buffer
                        .iter()
                        .map(|r| r.create_time_s)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    self.trade_buffer
                        .iter()
                        .map(|r| r.create_time_ms)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    self.trade_buffer
                        .iter()
                        .map(|r| r.server_time_ms)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(BooleanArray::from(
                    self.trade_buffer
                        .iter()
                        .map(|r| r.is_internal)
                        .collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    self.trade_buffer
                        .iter()
                        .map(|r| r.ingest_ms)
                        .collect::<Vec<_>>(),
                )),
            ],
        )
        .context("build trade record batch")?;
        self.write_batch(path, batch)
    }

    fn write_batch(&self, path: &Path, batch: RecordBatch) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("parquet path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;

        let tmp = path.with_extension("parquet.tmp");
        let write_res = (|| -> Result<()> {
            let file = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            let mut writer =
                ArrowWriter::try_new(file, batch.schema(), Some(self.write_props.clone()))
                    .context("ArrowWriter::try_new")?;
            writer.write(&batch).context("ArrowWriter::write")?;
            writer.close().context("ArrowWriter::close")?;
            if path.exists() {
                std::fs::remove_file(path)
                    .with_context(|| format!("remove old {}", path.display()))?;
            }
            std::fs::rename(&tmp, path)
                .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
            Ok(())
        })();

        if write_res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        write_res
    }
}

fn writer_loop(
    rx: Receiver<(MarketEvent, LatencyStamps)>,
    cancel: CancellationToken,
    out_dir: PathBuf,
    write_props: WriterProperties,
) {
    info!(
        target: "futures_collector",
        out = %out_dir.display(),
        "writer started (parquet mode)"
    );
    let mut writer = ParquetRollingWriter::new(out_dir, write_props);

    loop {
        if cancel.is_cancelled() && rx.is_empty() {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(RECV_TIMEOUT_MS)) {
            Ok((ev, stamps)) => {
                if let Err(e) = writer.maybe_rotate(Utc::now()) {
                    warn!(error = %e, "rotate failed — dropping event");
                    continue;
                }
                writer.push_event(&ev, &stamps);
                while let Ok((ev, stamps)) = rx.try_recv() {
                    if let Err(e) = writer.maybe_rotate(Utc::now()) {
                        warn!(error = %e, "drain rotate failed — remaining events deferred");
                        break;
                    }
                    writer.push_event(&ev, &stamps);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Err(e) = writer.maybe_rotate(Utc::now()) {
                    warn!(error = %e, "idle rotate failed");
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                info!(target: "futures_collector", "channel disconnected — exit");
                break;
            }
        }
    }
    writer.flush_all();
    info!(target: "futures_collector", "writer stopped");
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;
    let _tele = init_telemetry(&cfg)?;
    std::fs::create_dir_all(&cli.out_dir)
        .with_context(|| format!("create out dir {}", cli.out_dir.display()))?;

    let write_props = writer_properties(&cli.compression, cli.zstd_level)?;

    info!(
        service = %cfg.service_name,
        out_dir = %cli.out_dir.display(),
        compression = %cli.compression,
        zstd_level = cli.zstd_level,
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
    let out_dir = cli.out_dir.clone();
    let writer_task =
        tokio::task::spawn_blocking(move || writer_loop(rx, writer_cancel, out_dir, write_props));

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

#[cfg(test)]
mod tests {
    use super::*;

    use arrow::array::{BooleanArray, Float64Array, StringArray};
    use chrono::TimeZone;
    use hft_types::{BookTicker, ExchangeId, Price, Size, Symbol, Trade};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::file::reader::{FileReader, SerializedFileReader};

    fn temp_root(name: &str) -> PathBuf {
        let unique = format!(
            "hft-futures-collector-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("mkdir temp root");
        path
    }

    fn cleanup_temp(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    fn sample_stamps(ingest_ms: i64) -> LatencyStamps {
        let mut stamps = LatencyStamps::new();
        stamps.ws_received.wall_ms = ingest_ms;
        stamps
    }

    fn sample_bookticker() -> MarketEvent {
        MarketEvent::BookTicker(BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(100.5),
            ask_price: Price(100.6),
            bid_size: Size(1.0),
            ask_size: Size(2.0),
            event_time_ms: 1_700_000_000_001,
            server_time_ms: 1_700_000_000_002,
        })
    }

    fn sample_trade() -> MarketEvent {
        MarketEvent::Trade(Trade {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("ETH_USDT"),
            price: Price(3450.25),
            size: Size(-5.0),
            trade_id: 42,
            create_time_s: 1_700_000_000,
            create_time_ms: 1_700_000_000_123,
            server_time_ms: 1_700_000_000_124,
            is_internal: false,
        })
    }

    fn sample_writer(root: PathBuf) -> ParquetRollingWriter {
        ParquetRollingWriter::new(root, writer_properties("zstd", 3).expect("writer props"))
    }

    fn read_batches(path: &Path) -> Vec<RecordBatch> {
        let file = File::open(path).expect("open parquet");
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .expect("builder")
            .build()
            .expect("build reader");
        reader.map(|b| b.expect("batch")).collect()
    }

    fn parquet_rows(path: &Path) -> usize {
        read_batches(path).iter().map(RecordBatch::num_rows).sum()
    }

    #[test]
    fn test_bookticker_schema_fields() {
        let schema = bookticker_schema();
        assert_eq!(schema.fields().len(), 9);
        assert_eq!(schema.field(0).name(), "exchange");
        assert_eq!(schema.field(0).data_type(), &DataType::Utf8);
        assert_eq!(schema.field(8).name(), "ingest_ms");
        assert_eq!(schema.field(8).data_type(), &DataType::Int64);
    }

    #[test]
    fn test_trade_schema_fields() {
        let schema = trade_schema();
        assert_eq!(schema.fields().len(), 10);
        assert_eq!(schema.field(0).name(), "exchange");
        assert_eq!(schema.field(3).data_type(), &DataType::Float64);
        assert_eq!(schema.field(8).data_type(), &DataType::Boolean);
    }

    #[test]
    fn test_push_bookticker_populates_buffer() {
        let root = temp_root("push-bt");
        let mut writer = sample_writer(root.clone());
        writer.current_hour = Some((2026, 4, 18, 15));
        writer.push_event(&sample_bookticker(), &sample_stamps(1234));
        assert_eq!(writer.bt_buffer.len(), 1);
        assert!(writer.trade_buffer.is_empty());
        cleanup_temp(&root);
    }

    #[test]
    fn test_push_trade_populates_buffer() {
        let root = temp_root("push-trade");
        let mut writer = sample_writer(root.clone());
        writer.current_hour = Some((2026, 4, 18, 15));
        writer.push_event(&sample_trade(), &sample_stamps(1234));
        assert_eq!(writer.trade_buffer.len(), 1);
        assert!(writer.bt_buffer.is_empty());
        cleanup_temp(&root);
    }

    #[test]
    fn test_hive_partition_path() {
        let root = Path::new("/tmp/collector");
        let path = parquet_path(root, "bookticker", (2026, 4, 18, 15));
        assert_eq!(
            path,
            Path::new(
                "/tmp/collector/type=bookticker/year=2026/month=04/day=18/hour=15/data.parquet"
            )
        );
    }

    #[test]
    fn test_flush_creates_parquet_file() {
        let root = temp_root("flush");
        let mut writer = sample_writer(root.clone());
        writer.current_hour = Some((2026, 4, 18, 15));
        for _ in 0..10 {
            writer.push_event(&sample_bookticker(), &sample_stamps(1111));
        }
        writer.flush_buffers().expect("flush buffers");
        let path = parquet_path(&root, "bookticker", (2026, 4, 18, 15));
        assert!(path.exists());
        assert_eq!(parquet_rows(&path), 10);
        cleanup_temp(&root);
    }

    #[test]
    fn test_rotate_flushes_previous_hour() {
        let root = temp_root("rotate");
        let mut writer = sample_writer(root.clone());
        let first = Utc
            .with_ymd_and_hms(2026, 4, 18, 15, 59, 58)
            .single()
            .expect("first hour");
        writer.maybe_rotate(first).expect("set first hour");
        writer.push_event(&sample_trade(), &sample_stamps(1234));

        let second = Utc
            .with_ymd_and_hms(2026, 4, 18, 16, 0, 1)
            .single()
            .expect("second hour");
        writer.maybe_rotate(second).expect("rotate second hour");

        let first_path = parquet_path(&root, "trade", (2026, 4, 18, 15));
        assert!(first_path.exists());
        assert!(writer.trade_buffer.is_empty());
        assert_eq!(writer.current_hour, Some((2026, 4, 18, 16)));
        cleanup_temp(&root);
    }

    #[test]
    fn test_parquet_roundtrip_bookticker() {
        let root = temp_root("roundtrip-bt");
        let mut writer = sample_writer(root.clone());
        writer.current_hour = Some((2026, 4, 18, 15));
        writer.push_event(&sample_bookticker(), &sample_stamps(7777));
        writer.flush_buffers().expect("flush");

        let path = parquet_path(&root, "bookticker", (2026, 4, 18, 15));
        let batches = read_batches(&path);
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        let exchange = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("exchange col");
        let bid_price = batch
            .column(2)
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("bid col");
        assert_eq!(exchange.value(0), "gate");
        assert_eq!(bid_price.value(0), 100.5);
        cleanup_temp(&root);
    }

    #[test]
    fn test_parquet_roundtrip_trade() {
        let root = temp_root("roundtrip-trade");
        let mut writer = sample_writer(root.clone());
        writer.current_hour = Some((2026, 4, 18, 15));
        writer.push_event(&sample_trade(), &sample_stamps(8888));
        writer.flush_buffers().expect("flush");

        let path = parquet_path(&root, "trade", (2026, 4, 18, 15));
        let batches = read_batches(&path);
        assert_eq!(batches.len(), 1);
        let batch = &batches[0];
        let symbol = batch
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("symbol col");
        let internal = batch
            .column(8)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .expect("bool col");
        assert_eq!(symbol.value(0), "ETH_USDT");
        assert!(!internal.value(0));
        cleanup_temp(&root);
    }

    #[test]
    fn test_zstd_compression() {
        let root = temp_root("compression");
        let mut writer = sample_writer(root.clone());
        writer.current_hour = Some((2026, 4, 18, 15));
        writer.push_event(&sample_bookticker(), &sample_stamps(9999));
        writer.flush_buffers().expect("flush");

        let path = parquet_path(&root, "bookticker", (2026, 4, 18, 15));
        let file = File::open(&path).expect("open parquet");
        let reader = SerializedFileReader::new(file).expect("serialized reader");
        let codec = reader.metadata().row_group(0).column(0).compression();
        assert!(matches!(codec, Compression::ZSTD(_)));
        cleanup_temp(&root);
    }
}

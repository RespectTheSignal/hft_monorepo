//! hft-storage — QuestDB ILP writer (raw TCP, Line Protocol).
//!
//! ## 설계
//! - **Hot path 금지**: 이 crate 는 SUB 서비스(또는 별도 storage-svc 프로세스)에서만
//!   호출. publisher/strategy 는 건드리지 않는다.
//! - **raw ILP over TCP**: questdb-rs API drift 위험을 피해 InfluxDB Line Protocol
//!   문자열을 직접 TCP socket 9009 로 보낸다. escape 규칙은 ILP v1 공식 문서 기준.
//! - **Batch + Spool**: batch_rows / batch_ms 하한에 맞춰 flush. 연결 장애 시에는
//!   ILP 텍스트를 `spool_dir/*.ilp` 파일로 append 하고 복구 시 replay.
//! - **Reconnect backoff**: 100ms → 30s cap, 지수적. hot path 가 아니므로 안전하게 재시도.
//!
//! ## 테이블 스키마 (논리)
//! - `bookticker(symbol SYMBOL, exchange SYMBOL, bid_price DOUBLE, ask_price DOUBLE,
//!               bid_size DOUBLE, ask_size DOUBLE, event_time_ms LONG,
//!               server_time_ms LONG, ts TIMESTAMP)` → partition BY DAY
//! - `trade(symbol SYMBOL, exchange SYMBOL, price DOUBLE, size DOUBLE,
//!          trade_id LONG, create_time_ms LONG, server_time_ms LONG,
//!          is_internal BOOLEAN, ts TIMESTAMP)` → partition BY DAY
//! - `latency_stages(stage SYMBOL, nanos LONG, ts TIMESTAMP)` → partition BY HOUR
//!
//! CREATE TABLE IF NOT EXISTS 는 PG wire 로 별도 실행 (Phase 2).
//!
//! ## 계약
//! - `push_*` 는 Buffer 에 append 만. 1-line 당 수 μs. alloc 은 Vec grow 시 1회.
//! - `maybe_flush` 는 bg 의 100ms tick 또는 이벤트 배치 경계에서 호출.
//! - 연결 장애 시 `push_*` 는 실패하지 않는다 (spool 에 기록).
//! - `force_flush` 는 shutdown 에서만. 필요 시 블로킹.
//!
//! ## Anti-jitter
//! - 파일 I/O 는 별도 thread 로 옮길 수 있도록 `SpoolWriter` 를 trait 뒤로 숨김.
//! - `Instant::now()` 호출은 push 당 0회 (caller 가 넘기는 stamp 에 의존), flush 경로에서만.
//! - TCP connect 재시도는 별 thread 에서 수행 (현 impl 은 inline, Phase 2 분리).

#![deny(rust_2018_idioms)]

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tracing::{debug, info, warn};

use hft_config::QuestDbConfig;
use hft_time::{LatencyStamps, Stage};
use hft_types::{BookTicker, Trade};

// ─────────────────────────────────────────────────────────────────────────────
// Error
// ─────────────────────────────────────────────────────────────────────────────

/// 스토리지 레이어 에러.
#[derive(Debug, Error)]
pub enum StorageError {
    /// I/O 에러 (TCP, 파일).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// ILP 에러 (길이 초과 등).
    #[error("ilp format error: {0}")]
    Format(String),
    /// 아직 연결되지 않아 flush 를 수행할 수 없음 — spool 로만 기록.
    #[error("not connected (spooled)")]
    NotConnected,
}

/// 결과 alias.
pub type StorageResult<T> = Result<T, StorageError>;

// ─────────────────────────────────────────────────────────────────────────────
// 테이블명 상수
// ─────────────────────────────────────────────────────────────────────────────

/// `bookticker` 테이블명.
pub const TABLE_BOOKTICKER: &str = "bookticker";
/// `trade` 테이블명.
pub const TABLE_TRADE: &str = "trade";
/// `latency_stages` 테이블명.
pub const TABLE_LATENCY: &str = "latency_stages";

// ─────────────────────────────────────────────────────────────────────────────
// ILP encoder — 순수 함수, buffer append
// ─────────────────────────────────────────────────────────────────────────────

/// ILP v1 포맷 인코더. 모든 메서드는 주어진 `Vec<u8>` 에 line 을 append.
///
/// 포맷: `measurement,tag1=val1 field1=v1,field2=v2 timestamp_ns\n`
pub struct IlpEncoder;

impl IlpEncoder {
    /// BookTicker 한 줄 append.
    ///
    /// stamps.exchange_server.wall_ms (ms) 를 timestamp ns 로 변환해 사용.
    /// wall_ms == 0 이면 ingestion 시각 (SystemTime::now) 으로 fallback.
    pub fn append_bookticker(
        buf: &mut Vec<u8>,
        bt: &BookTicker,
        stamps: &LatencyStamps,
    ) -> StorageResult<()> {
        let ts_ns = stamp_to_ns_or_now(stamps.exchange_server.wall_ms);
        buf.extend_from_slice(TABLE_BOOKTICKER.as_bytes());
        write_tag(buf, "symbol", bt.symbol.as_str())?;
        write_tag(buf, "exchange", bt.exchange.as_str())?;
        buf.push(b' ');
        write_field_f64(buf, "bid_price", bt.bid_price.get(), true);
        write_field_f64(buf, "ask_price", bt.ask_price.get(), false);
        write_field_f64(buf, "bid_size", bt.bid_size.get(), false);
        write_field_f64(buf, "ask_size", bt.ask_size.get(), false);
        write_field_i64(buf, "event_time_ms", bt.event_time_ms, false);
        write_field_i64(buf, "server_time_ms", bt.server_time_ms, false);
        buf.push(b' ');
        write_u64(buf, ts_ns);
        buf.push(b'\n');
        Ok(())
    }

    /// Trade 한 줄 append.
    pub fn append_trade(
        buf: &mut Vec<u8>,
        tr: &Trade,
        stamps: &LatencyStamps,
    ) -> StorageResult<()> {
        let ts_ns = stamp_to_ns_or_now(stamps.exchange_server.wall_ms);
        buf.extend_from_slice(TABLE_TRADE.as_bytes());
        write_tag(buf, "symbol", tr.symbol.as_str())?;
        write_tag(buf, "exchange", tr.exchange.as_str())?;
        buf.push(b' ');
        write_field_f64(buf, "price", tr.price.get(), true);
        write_field_f64(buf, "size", tr.size.get(), false);
        write_field_i64(buf, "trade_id", tr.trade_id, false);
        write_field_i64(buf, "create_time_ms", tr.create_time_ms, false);
        write_field_i64(buf, "server_time_ms", tr.server_time_ms, false);
        write_field_bool(buf, "is_internal", tr.is_internal, false);
        buf.push(b' ');
        write_u64(buf, ts_ns);
        buf.push(b'\n');
        Ok(())
    }

    /// Latency sample 한 줄 append.
    pub fn append_latency(
        buf: &mut Vec<u8>,
        stage: Stage,
        nanos: u64,
    ) -> StorageResult<()> {
        let ts_ns = stamp_to_ns_or_now(0);
        buf.extend_from_slice(TABLE_LATENCY.as_bytes());
        write_tag(buf, "stage", stage_name(stage))?;
        buf.push(b' ');
        write_field_i64(buf, "nanos", nanos as i64, true);
        buf.push(b' ');
        write_u64(buf, ts_ns);
        buf.push(b'\n');
        Ok(())
    }
}

/// 0 이면 지금 SystemTime::now() (ns), 그 외에는 ms→ns 변환.
fn stamp_to_ns_or_now(wall_ms: i64) -> u64 {
    if wall_ms > 0 {
        (wall_ms as u64).saturating_mul(1_000_000)
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}

/// Stage → ILP tag value.
fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::ExchangeServer => "exchange_server",
        Stage::WsReceived => "ws_received",
        Stage::Serialized => "serialized",
        Stage::Pushed => "pushed",
        Stage::Published => "published",
        Stage::Subscribed => "subscribed",
        Stage::Consumed => "consumed",
    }
}

/// 태그 append (앞에 `,` 자동). key 와 value 모두 escape.
fn write_tag(buf: &mut Vec<u8>, key: &str, value: &str) -> StorageResult<()> {
    if value.is_empty() {
        return Err(StorageError::Format(format!("empty tag value for {}", key)));
    }
    buf.push(b',');
    escape_key_or_tag(buf, key);
    buf.push(b'=');
    escape_key_or_tag(buf, value);
    Ok(())
}

/// DOUBLE field. `first=true` 면 leading `,` 생략.
fn write_field_f64(buf: &mut Vec<u8>, key: &str, value: f64, first: bool) {
    if !first {
        buf.push(b',');
    }
    escape_field_key(buf, key);
    buf.push(b'=');
    let v = if value.is_finite() { value } else { 0.0 };
    buf.extend_from_slice(format!("{}", v).as_bytes());
}

/// LONG field. ILP 는 정수는 `i` suffix.
fn write_field_i64(buf: &mut Vec<u8>, key: &str, value: i64, first: bool) {
    if !first {
        buf.push(b',');
    }
    escape_field_key(buf, key);
    buf.push(b'=');
    buf.extend_from_slice(format!("{}", value).as_bytes());
    buf.push(b'i');
}

/// BOOLEAN field. ILP 는 `t`/`f`.
fn write_field_bool(buf: &mut Vec<u8>, key: &str, value: bool, first: bool) {
    if !first {
        buf.push(b',');
    }
    escape_field_key(buf, key);
    buf.push(b'=');
    buf.push(if value { b't' } else { b'f' });
}

/// unsigned 숫자 append (timestamp 용).
fn write_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(format!("{}", v).as_bytes());
}

/// measurement/tag key/tag value escape: `,`, `=`, ` `, `\` 를 backslash 로.
fn escape_key_or_tag(buf: &mut Vec<u8>, s: &str) {
    for &b in s.as_bytes() {
        match b {
            b',' | b'=' | b' ' | b'\\' => {
                buf.push(b'\\');
                buf.push(b);
            }
            b'\n' | b'\r' => {
                // control char 제거
            }
            _ => buf.push(b),
        }
    }
}

/// field key escape: tag 와 동일 규칙.
fn escape_field_key(buf: &mut Vec<u8>, s: &str) {
    escape_key_or_tag(buf, s);
}

// ─────────────────────────────────────────────────────────────────────────────
// TCP connection wrapper with reconnect backoff
// ─────────────────────────────────────────────────────────────────────────────

/// TCP 연결 상태 + exponential backoff.
struct IlpConnection {
    addr: String,
    stream: Option<TcpStream>,
    next_backoff_ms: u64,
    last_attempt: Option<Instant>,
}

impl IlpConnection {
    /// 초기 상태 (미연결).
    fn new(addr: String) -> Self {
        Self {
            addr,
            stream: None,
            next_backoff_ms: 100,
            last_attempt: None,
        }
    }

    /// 현재 연결 상태.
    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// 필요 시 재연결. backoff 동안은 skip.
    fn maybe_reconnect(&mut self) {
        if self.is_connected() || self.addr.is_empty() {
            return;
        }
        if let Some(last) = self.last_attempt {
            if last.elapsed() < Duration::from_millis(self.next_backoff_ms) {
                return;
            }
        }
        self.last_attempt = Some(Instant::now());

        let sock_addr: std::net::SocketAddr = match self.addr.parse() {
            Ok(a) => a,
            Err(e) => {
                warn!(
                    target: "hft_storage",
                    addr = %self.addr,
                    error = ?e,
                    "bad ILP addr"
                );
                self.next_backoff_ms = (self.next_backoff_ms * 2).min(30_000);
                return;
            }
        };

        match TcpStream::connect_timeout(&sock_addr, Duration::from_secs(3)) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                self.stream = Some(stream);
                self.next_backoff_ms = 100;
                info!(target: "hft_storage", addr = %self.addr, "ILP connected");
            }
            Err(e) => {
                debug!(
                    target: "hft_storage",
                    addr = %self.addr,
                    error = ?e,
                    backoff_ms = self.next_backoff_ms,
                    "ILP connect failed — backoff"
                );
                self.next_backoff_ms = (self.next_backoff_ms * 2).min(30_000);
            }
        }
    }

    /// buffer 를 TCP 로 flush. 실패 시 연결 drop.
    fn write_all(&mut self, data: &[u8]) -> StorageResult<()> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(StorageError::NotConnected);
        };
        match stream.write_all(data) {
            Ok(()) => Ok(()),
            Err(e) => {
                warn!(
                    target: "hft_storage",
                    error = ?e,
                    "ILP write failed — dropping connection"
                );
                self.stream = None;
                Err(StorageError::Io(e))
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Spool writer — 연결 장애 시 파일로 ILP 라인 보관
// ─────────────────────────────────────────────────────────────────────────────

/// 디스크 spool 관리. 연결 복구 후 replay.
pub struct SpoolWriter {
    dir: PathBuf,
    current_path: Option<PathBuf>,
    current: Option<BufWriter<File>>,
}

impl SpoolWriter {
    /// 디렉터리 보장 + 초기화 (기존 파일은 남겨 두어 startup 시 replay 대상).
    pub fn new(dir: &Path) -> StorageResult<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            current_path: None,
            current: None,
        })
    }

    /// 빈 path: 비활성화. 파일 I/O 를 전혀 수행하지 않음.
    pub fn disabled() -> Self {
        Self {
            dir: PathBuf::new(),
            current_path: None,
            current: None,
        }
    }

    fn ensure_open(&mut self) -> StorageResult<()> {
        if self.current.is_some() || self.dir.as_os_str().is_empty() {
            return Ok(());
        }
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = self.dir.join(format!("ilp-{}.ilp", now_ns));
        let f = OpenOptions::new().create(true).append(true).open(&path)?;
        self.current_path = Some(path.clone());
        self.current = Some(BufWriter::new(f));
        debug!(target: "hft_storage", path = %path.display(), "opened spool file");
        Ok(())
    }

    /// ILP 바이트를 spool 에 append. disabled 면 silent no-op.
    pub fn append(&mut self, data: &[u8]) -> StorageResult<()> {
        if self.dir.as_os_str().is_empty() {
            return Ok(());
        }
        self.ensure_open()?;
        if let Some(w) = self.current.as_mut() {
            w.write_all(data)?;
        }
        Ok(())
    }

    /// 현재 spool 파일 flush (버퍼만). fsync 하지 않음 (jitter 회피).
    pub fn flush(&mut self) -> StorageResult<()> {
        if let Some(w) = self.current.as_mut() {
            w.flush()?;
        }
        Ok(())
    }

    /// 누적된 spool 파일을 replay. 성공한 파일은 삭제.
    ///
    /// 반환: replay 된 파일 수.
    pub fn replay(&mut self, conn: &mut IlpConnection) -> StorageResult<usize> {
        if self.dir.as_os_str().is_empty() || !conn.is_connected() {
            return Ok(0);
        }
        if let Some(mut w) = self.current.take() {
            let _ = w.flush();
        }
        self.current_path = None;

        let mut replayed = 0;
        let entries = std::fs::read_dir(&self.dir)?;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("ilp") {
                continue;
            }
            let mut f = match File::open(&p) {
                Ok(f) => f,
                Err(e) => {
                    warn!(
                        target: "hft_storage",
                        path = %p.display(),
                        error = ?e,
                        "spool open failed"
                    );
                    continue;
                }
            };
            let mut bytes = Vec::new();
            if f.read_to_end(&mut bytes).is_err() {
                continue;
            }
            if bytes.is_empty() {
                let _ = std::fs::remove_file(&p);
                continue;
            }
            match conn.write_all(&bytes) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&p);
                    replayed += 1;
                }
                Err(e) => {
                    warn!(
                        target: "hft_storage",
                        path = %p.display(),
                        error = ?e,
                        "replay failed — stopping, will retry later"
                    );
                    break;
                }
            }
        }
        if replayed > 0 {
            info!(target: "hft_storage", replayed, "spool replay complete");
        }
        Ok(replayed)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// QuestDbSink — 공개 API
// ─────────────────────────────────────────────────────────────────────────────

/// QuestDB ILP 싱크. batch 후 flush.
pub struct QuestDbSink {
    conn: IlpConnection,
    buffer: Vec<u8>,
    rows_in_buffer: usize,
    batch_rows: usize,
    batch_interval: Duration,
    last_flush: Instant,
    spool: SpoolWriter,
}

impl QuestDbSink {
    /// cfg 기준 Sink 생성. TCP 연결은 lazy (처음 flush 시 시도).
    ///
    /// `cfg.ilp_addr` 가 비어있으면 "disabled" 모드 — flush 는 항상 NotConnected.
    pub fn new(cfg: &QuestDbConfig) -> StorageResult<Self> {
        let spool = if cfg.spool_dir.as_os_str().is_empty() {
            SpoolWriter::disabled()
        } else {
            SpoolWriter::new(&cfg.spool_dir)?
        };
        Ok(Self {
            conn: IlpConnection::new(cfg.ilp_addr.clone()),
            buffer: Vec::with_capacity(64 * 1024),
            rows_in_buffer: 0,
            batch_rows: cfg.batch_rows.max(1),
            batch_interval: Duration::from_millis(cfg.batch_ms.max(1)),
            last_flush: Instant::now(),
            spool,
        })
    }

    /// BookTicker 를 buffer 에 append.
    pub fn push_bookticker(
        &mut self,
        bt: &BookTicker,
        stamps: &LatencyStamps,
    ) -> StorageResult<()> {
        IlpEncoder::append_bookticker(&mut self.buffer, bt, stamps)?;
        self.rows_in_buffer += 1;
        Ok(())
    }

    /// Trade 를 buffer 에 append.
    pub fn push_trade(
        &mut self,
        tr: &Trade,
        stamps: &LatencyStamps,
    ) -> StorageResult<()> {
        IlpEncoder::append_trade(&mut self.buffer, tr, stamps)?;
        self.rows_in_buffer += 1;
        Ok(())
    }

    /// Latency sample append.
    pub fn push_latency_sample(
        &mut self,
        stage: Stage,
        nanos: u64,
    ) -> StorageResult<()> {
        IlpEncoder::append_latency(&mut self.buffer, stage, nanos)?;
        self.rows_in_buffer += 1;
        Ok(())
    }

    /// batch_rows 또는 batch_ms 초과 시 flush. 그 외엔 no-op.
    pub fn maybe_flush(&mut self) -> StorageResult<()> {
        if self.rows_in_buffer == 0 {
            return Ok(());
        }
        let overflow_rows = self.rows_in_buffer >= self.batch_rows;
        let overflow_time = self.last_flush.elapsed() >= self.batch_interval;
        if !overflow_rows && !overflow_time {
            return Ok(());
        }
        self.force_flush()
    }

    /// 즉시 flush. 실패하면 spool 로 이동.
    pub fn force_flush(&mut self) -> StorageResult<()> {
        if self.rows_in_buffer == 0 {
            return Ok(());
        }
        self.conn.maybe_reconnect();
        if !self.conn.is_connected() {
            self.spool.append(&self.buffer)?;
            self.reset_after_flush();
            return Err(StorageError::NotConnected);
        }

        // 기회되면 지난 spool 내용 먼저 replay.
        let _ = self.spool.replay(&mut self.conn);

        match self.conn.write_all(&self.buffer) {
            Ok(()) => {
                debug!(
                    target: "hft_storage",
                    rows = self.rows_in_buffer,
                    bytes = self.buffer.len(),
                    "flushed"
                );
                self.reset_after_flush();
                Ok(())
            }
            Err(e) => {
                if let Err(se) = self.spool.append(&self.buffer) {
                    warn!(
                        target: "hft_storage",
                        error = ?se,
                        "spool append failed after tcp failure — data loss risk"
                    );
                }
                self.reset_after_flush();
                Err(e)
            }
        }
    }

    fn reset_after_flush(&mut self) {
        self.buffer.clear();
        self.rows_in_buffer = 0;
        self.last_flush = Instant::now();
    }

    /// shutdown 시 flush + spool 도 flush.
    pub fn shutdown(mut self) -> StorageResult<()> {
        let _ = self.force_flush();
        self.spool.flush()?;
        Ok(())
    }

    /// 현재 pending row 수 (테스트/metric 용).
    pub fn pending_rows(&self) -> usize {
        self.rows_in_buffer
    }

    /// 현재 연결 상태.
    pub fn is_connected(&self) -> bool {
        self.conn.is_connected()
    }

    /// 내부 buffer 접근 (테스트 전용).
    #[cfg(test)]
    pub(crate) fn buffer(&self) -> &[u8] {
        &self.buffer
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 테스트
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_types::{ExchangeId, Price, Size, Symbol};

    fn sample_bt() -> BookTicker {
        BookTicker {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            bid_price: Price(100.5),
            ask_price: Price(100.6),
            bid_size: Size(1.0),
            ask_size: Size(2.5),
            event_time_ms: 1_700_000_000_123,
            server_time_ms: 1_700_000_000_200,
        }
    }

    fn sample_trade() -> Trade {
        Trade {
            exchange: ExchangeId::Binance,
            symbol: Symbol::new("ETH_USDT"),
            price: Price(3000.0),
            size: Size(-0.5),
            trade_id: 42,
            create_time_s: 1_700_000_000,
            create_time_ms: 1_700_000_000_500,
            server_time_ms: 1_700_000_000_600,
            is_internal: false,
        }
    }

    fn stamps_with_server(ms: i64) -> LatencyStamps {
        let mut s = LatencyStamps::new();
        s.set_exchange_server_ms(ms);
        s
    }

    fn rand_suffix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    #[test]
    fn encode_bookticker_expected_shape() {
        let mut buf = Vec::new();
        IlpEncoder::append_bookticker(
            &mut buf,
            &sample_bt(),
            &stamps_with_server(1_700_000_000_123),
        )
        .unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.starts_with("bookticker,"), "got: {}", s);
        assert!(s.contains(",symbol=BTC_USDT"));
        assert!(s.contains(",exchange=gate"));
        assert!(s.contains(" bid_price=100.5,"));
        assert!(s.contains(",ask_price=100.6,"));
        assert!(s.contains(",bid_size=1,"));
        assert!(s.contains(",ask_size=2.5,"));
        assert!(s.contains(",event_time_ms=1700000000123i,"));
        assert!(s.contains(",server_time_ms=1700000000200i "));
        assert!(s.ends_with("1700000000123000000\n"), "got: {}", s);
    }

    #[test]
    fn encode_trade_expected_shape() {
        let mut buf = Vec::new();
        IlpEncoder::append_trade(
            &mut buf,
            &sample_trade(),
            &stamps_with_server(1_700_000_000_600),
        )
        .unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.starts_with("trade,"), "got: {}", s);
        assert!(s.contains(",symbol=ETH_USDT"));
        assert!(s.contains(",exchange=binance"));
        assert!(s.contains(" price=3000,"));
        assert!(s.contains(",size=-0.5,"));
        assert!(s.contains(",trade_id=42i,"));
        assert!(s.contains(",is_internal=f "));
        assert!(s.ends_with("1700000000600000000\n"));
    }

    #[test]
    fn encode_latency_stage() {
        let mut buf = Vec::new();
        IlpEncoder::append_latency(&mut buf, Stage::Serialized, 1234).unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.starts_with("latency_stages,stage=serialized "));
        assert!(s.contains("nanos=1234i "));
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn escape_tag_with_special_chars() {
        let mut buf = Vec::new();
        escape_key_or_tag(&mut buf, "a b,c=d\\e");
        let s = std::str::from_utf8(&buf).unwrap();
        assert_eq!(s, "a\\ b\\,c\\=d\\\\e");
    }

    #[test]
    fn nan_field_becomes_zero() {
        let mut buf = Vec::new();
        write_field_f64(&mut buf, "x", f64::NAN, true);
        let s = std::str::from_utf8(&buf).unwrap();
        assert_eq!(s, "x=0");
    }

    #[test]
    fn sink_buffers_then_flush_required() {
        let cfg = QuestDbConfig {
            ilp_addr: String::new(),
            batch_rows: 2,
            batch_ms: 50,
            spool_dir: PathBuf::new(),
        };
        let mut sink = QuestDbSink::new(&cfg).unwrap();
        sink.push_bookticker(&sample_bt(), &stamps_with_server(1_700_000_000_100))
            .unwrap();
        assert_eq!(sink.pending_rows(), 1);
        sink.maybe_flush().unwrap_or_default();
        assert_eq!(sink.pending_rows(), 1, "rows_in_buffer below batch_rows");

        sink.push_bookticker(&sample_bt(), &stamps_with_server(1_700_000_000_200))
            .unwrap();
        assert_eq!(sink.pending_rows(), 2);
        let err = sink.maybe_flush();
        assert!(matches!(err, Err(StorageError::NotConnected)));
        assert_eq!(sink.pending_rows(), 0, "reset after flush attempt");
    }

    #[test]
    fn spool_append_and_replay_roundtrip() {
        let tmp = std::env::temp_dir().join(format!(
            "hft-storage-spool-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut spool = SpoolWriter::new(&tmp).unwrap();

        spool.append(b"line1\n").unwrap();
        spool.append(b"line2\n").unwrap();
        spool.flush().unwrap();

        let entries: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("ilp"))
            .collect();
        assert_eq!(entries.len(), 1);

        let mut conn = IlpConnection::new(String::new());
        assert_eq!(spool.replay(&mut conn).unwrap(), 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn disabled_spool_is_noop() {
        let mut s = SpoolWriter::disabled();
        s.append(b"nothing\n").unwrap();
        s.flush().unwrap();
        let mut conn = IlpConnection::new(String::new());
        assert_eq!(s.replay(&mut conn).unwrap(), 0);
    }

    #[test]
    fn maybe_flush_empty_is_noop() {
        let cfg = QuestDbConfig {
            ilp_addr: String::new(),
            batch_rows: 10,
            batch_ms: 50,
            spool_dir: PathBuf::new(),
        };
        let mut sink = QuestDbSink::new(&cfg).unwrap();
        sink.maybe_flush().unwrap();
        assert_eq!(sink.pending_rows(), 0);
    }

    #[test]
    fn force_flush_without_rows_is_ok() {
        let cfg = QuestDbConfig {
            ilp_addr: String::new(),
            batch_rows: 10,
            batch_ms: 50,
            spool_dir: PathBuf::new(),
        };
        let mut sink = QuestDbSink::new(&cfg).unwrap();
        sink.force_flush().unwrap();
    }

    #[test]
    fn buffer_accessor_shows_accumulated_ilp() {
        let cfg = QuestDbConfig {
            ilp_addr: String::new(),
            batch_rows: 100,
            batch_ms: 1000,
            spool_dir: PathBuf::new(),
        };
        let mut sink = QuestDbSink::new(&cfg).unwrap();
        sink.push_bookticker(&sample_bt(), &stamps_with_server(1_700_000_000_100))
            .unwrap();
        sink.push_trade(&sample_trade(), &stamps_with_server(1_700_000_000_200))
            .unwrap();
        sink.push_latency_sample(Stage::Pushed, 5000).unwrap();
        assert_eq!(sink.pending_rows(), 3);
        let s = std::str::from_utf8(sink.buffer()).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("bookticker"));
        assert!(lines[1].starts_with("trade"));
        assert!(lines[2].starts_with("latency_stages"));
    }
}

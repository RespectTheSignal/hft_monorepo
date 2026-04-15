//! hft-exchange-bybit — Bybit Linear Futures (v5) feed.
//!
//! ## 설계 (Phase 2 refactor: zero-alloc hot-path)
//!
//! - **URL**: `wss://stream.bybit.com/v5/public/linear`.
//! - **Subscribe**: `{"op":"subscribe","args":["orderbook.1.BTCUSDT","publicTrade.BTCUSDT", …]}`.
//!   Bybit 서버 제약 상 `args` 는 [`MAX_SUBSCRIBE_ARGS`] = 10개씩 chunk.
//! - **Level-1 orderbook** 은 ~10ms push rate. snapshot + delta 둘 다
//!   `orderbook.1` 채널이면 `b[0]` / `a[0]` = top-of-book 이라 delta 여도
//!   1-level 은 완결된 상태. (delta 인데 `b`/`a` 가 비어있으면 top 미변경 → skip.)
//! - **publicTrade** 채널: 체결 1건 당 [`MarketEvent::Trade`] emit. `S` 필드가
//!   `"Buy"` / `"Sell"` 로 오므로 buyer aggressor (+), seller aggressor (-) 부호 적용.
//!   `i` (trade id) 는 UUID 문자열이므로 `i64` parse 시도 후 실패하면
//!   `ahash` 로 stable 64bit hash 생성 (truncate to i63 to avoid negative).
//! - **Client ping**: [`BybitConfig::ping_interval_secs`] (기본 20s) 간격으로
//!   `Message::Ping(empty)` 을 보낸다. Bybit 서버가 **JSON ping** 을 보내는
//!   경우도 있으므로 `{"op":"ping","req_id":"..."}` 수신 시 `{"op":"pong",
//!   "req_id":"..."}` 으로 즉시 답한다.
//! - **압축**: 일부 배포본은 gzip / zlib 압축 바이너리 프레임을 보낸다
//!   ([`decode_into`] 에서 magic byte 기반으로 분기).
//! - **Frame**:
//!   ```json
//!   {
//!     "topic":"orderbook.1.BTCUSDT",
//!     "type":"snapshot|delta",
//!     "data":{"s":"BTCUSDT","b":[["p","s"],...],"a":[["p","s"],...]},
//!     "ts":1_700_000_000_123
//!   }
//!   ```
//!   `data` 가 드물게 `[{...}]` array 형태로 오는 배포본도 있어 둘 다 지원.
//!
//! ## 심볼 매핑
//!
//! - canonical `BTC_USDT` ↔ wire `BTCUSDT` (uppercase, underscore 제거).
//! - 복원은 quote suffix match: `USDT` > `USDC` > `USD`.
//! - [`BybitFeed::stream`] 시점에 **raw→canonical 매핑을 미리 빌드** 하여
//!   이벤트당 string alloc 을 제거한다. cache miss 경로는 `from_bybit_symbol`
//!   + [`SymbolCache::intern`] fallback.
//!
//! ## 계약
//!
//! - [`BybitFeed::stream`] 은 내부 재연결. 상위에는 `Ok(())` (cancel) /
//!   초기화 실패 시에만 `Err` 를 전파.
//! - 프레임 1건 당 [`MarketEvent::BookTicker`] 를 **정확히 1회** emit.
//! - [`LatencyStamps`] 는 `ws_received` + `exchange_server_ms` (=frame ts)
//!   두 stage 를 채운다.

#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]

pub mod executor;
pub use executor::{BybitExecutor, BybitExecutorConfig};

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use flate2::read::{GzDecoder, ZlibDecoder};
use futures_util::{SinkExt, StreamExt};
use serde::{de, Deserialize, Deserializer};
use serde_json::value::RawValue;
use serde_json::Value;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, trace, warn};
use url::Url;

use hft_exchange_api::{Backoff, CancellationToken, Emitter, ExchangeFeed};
use hft_exchange_common::{
    deserialize_f64_lenient, deserialize_i64_lenient_opt, stamps_with_ws, SymbolCache,
};
use hft_time::{Clock, Stamp, SystemClock};
use hft_types::{BookTicker, DataRole, ExchangeId, MarketEvent, Price, Size, Symbol, Trade};

/// Bybit subscribe `args` chunk size (서버 제약).
const MAX_SUBSCRIBE_ARGS: usize = 10;

/// WS text scratch buffer 기본 capacity (bytes).
const DEFAULT_SCRATCH_CAPACITY: usize = 4096;

// ────────────────────────────────────────────────────────────────────────────
// BybitConfig
// ────────────────────────────────────────────────────────────────────────────

/// Bybit feed 구성.
#[derive(Debug, Clone)]
pub struct BybitConfig {
    /// Public WebSocket URL (v5 linear).
    pub ws_url: String,
    /// REST API base.
    pub rest_base_url: String,
    /// WS read timeout (초).
    pub read_timeout_secs: u64,
    /// Client ping interval (초).
    pub ping_interval_secs: u64,
    /// 백오프 최소 (ms).
    pub backoff_base_ms: u64,
    /// 백오프 최대 (ms).
    pub backoff_cap_ms: u64,
    /// REST 타임아웃 (ms).
    pub rest_timeout_ms: u64,
    /// Text frame scratch buffer 초기 capacity.
    pub scratch_capacity: usize,
}

impl Default for BybitConfig {
    fn default() -> Self {
        Self {
            ws_url: "wss://stream.bybit.com/v5/public/linear".into(),
            rest_base_url: "https://api.bybit.com".into(),
            read_timeout_secs: 60,
            ping_interval_secs: 20,
            backoff_base_ms: 200,
            backoff_cap_ms: 5_000,
            rest_timeout_ms: 10_000,
            scratch_capacity: DEFAULT_SCRATCH_CAPACITY,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// BybitFeed
// ────────────────────────────────────────────────────────────────────────────

/// Bybit linear futures `orderbook.1` feed.
///
/// `SymbolCache` 는 여러 feed 인스턴스 간 공유 가능하다. 같은 거래소의 여러
/// 역할 (primary / shadow) 또는 여러 거래소 feed 들이 동일한 프로세스 내에서
/// 동일한 intern pool 을 쓰도록 상위 오케스트레이션에서 Arc clone 하여 주입.
pub struct BybitFeed {
    cfg: Arc<BybitConfig>,
    clock: Arc<dyn Clock>,
    cache: Arc<SymbolCache>,
}

impl BybitFeed {
    /// 기본 `SystemClock` + 새 `SymbolCache` 로 생성.
    pub fn new(cfg: BybitConfig) -> Self {
        Self {
            cfg: Arc::new(cfg),
            clock: Arc::new(SystemClock::new()),
            cache: Arc::new(SymbolCache::new()),
        }
    }

    /// 외부 clock 주입 (테스트용 `MockClock` 등).
    pub fn with_clock(cfg: BybitConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            cfg: Arc::new(cfg),
            clock,
            cache: Arc::new(SymbolCache::new()),
        }
    }

    /// 공유 `SymbolCache` 주입.
    pub fn with_cache(cfg: BybitConfig, cache: Arc<SymbolCache>) -> Self {
        Self {
            cfg: Arc::new(cfg),
            clock: Arc::new(SystemClock::new()),
            cache,
        }
    }

    /// clock + cache 둘 다 외부 주입.
    pub fn with_clock_and_cache(
        cfg: BybitConfig,
        clock: Arc<dyn Clock>,
        cache: Arc<SymbolCache>,
    ) -> Self {
        Self {
            cfg: Arc::new(cfg),
            clock,
            cache,
        }
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub fn config(&self) -> &BybitConfig {
        &self.cfg
    }

    /// 내부 `SymbolCache` 에 대한 shared reference — 상위 컴포넌트가 같은
    /// intern pool 을 재사용할 때 사용.
    pub fn symbol_cache(&self) -> &Arc<SymbolCache> {
        &self.cache
    }
}

#[async_trait]
impl ExchangeFeed for BybitFeed {
    fn id(&self) -> ExchangeId {
        ExchangeId::Bybit
    }

    fn role(&self) -> DataRole {
        DataRole::Primary
    }

    async fn available_symbols(&self) -> anyhow::Result<Vec<Symbol>> {
        let url = format!(
            "{}/v5/market/instruments-info?category=linear",
            self.cfg.rest_base_url
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(self.cfg.rest_timeout_ms))
            .build()?;
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("bybit instruments-info status = {}", resp.status());
        }
        let json: Value = resp.json().await?;
        let ret = json.get("retCode").and_then(|v| v.as_i64()).unwrap_or(0);
        if ret != 0 {
            anyhow::bail!("bybit retCode = {}", ret);
        }
        let list = json
            .get("result")
            .and_then(|v| v.get("list"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut seen = std::collections::HashSet::new();
        for item in list {
            if item.get("status").and_then(|v| v.as_str()) != Some("Trading") {
                continue;
            }
            // baseCoin + quoteCoin 우선; 없으면 symbol 문자열 파싱.
            let base = item.get("baseCoin").and_then(|v| v.as_str());
            let quote = item.get("quoteCoin").and_then(|v| v.as_str());
            if let (Some(b), Some(q)) = (base, quote) {
                seen.insert(format!("{}_{}", b, q));
                continue;
            }
            if let Some(sym) = item.get("symbol").and_then(|v| v.as_str()) {
                seen.insert(from_bybit_symbol(sym));
            }
        }
        let mut out: Vec<Symbol> = seen.into_iter().map(Symbol::new).collect();
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        info!(count = out.len(), "bybit available_symbols fetched");
        Ok(out)
    }

    async fn stream(
        &self,
        symbols: &[Symbol],
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        if symbols.is_empty() {
            debug!("bybit stream: no symbols — no-op");
            return Ok(());
        }
        let subs: Vec<Symbol> = symbols
            .iter()
            .filter(|s| !s.as_str().is_empty())
            .cloned()
            .collect();
        if subs.is_empty() {
            debug!("bybit stream: all empty — no-op");
            return Ok(());
        }
        self.run_primary(subs, emit, cancel).await
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Primary 루프
// ────────────────────────────────────────────────────────────────────────────

impl BybitFeed {
    async fn run_primary(
        &self,
        symbols: Vec<Symbol>,
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        // 구독 심볼에 대해 intern pool prewarm + wire→canonical 사전계산.
        self.cache.prewarm(symbols.iter().map(|s| s.as_str()));
        let raw_to_sym = build_raw_to_sym(&symbols, &self.cache);

        let mut backoff = Backoff::new(self.cfg.backoff_base_ms, self.cfg.backoff_cap_ms);
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let attempt = backoff.attempts();
            info!(attempt, "bybit connecting");

            let outcome = tokio::select! {
                _ = cancel.cancelled() => {
                    info!("bybit cancelled during connect");
                    return Ok(());
                }
                r = self.run_primary_once(&symbols, &raw_to_sym, &emit, &cancel) => r,
            };

            match outcome {
                Ok(()) => {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    warn!("bybit disconnected cleanly, reconnecting");
                    backoff.reset();
                }
                Err(e) => {
                    warn!(error = %e, "bybit error; backoff");
                }
            }

            let delay_ms = backoff.next_ms();
            tokio::select! {
                _ = cancel.cancelled() => { return Ok(()); }
                _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            }
        }
    }

    async fn run_primary_once(
        &self,
        symbols: &[Symbol],
        raw_to_sym: &RawSymMap,
        emit: &Emitter,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let url = Url::parse(&self.cfg.ws_url)?;
        let (ws, resp) = connect_async(url.as_str()).await?;
        info!(status = ?resp.status(), "bybit connected");
        let (mut write, mut read) = ws.split();

        // 구독: orderbook.1.<WIRE_SYM> + publicTrade.<WIRE_SYM> (각 심볼 당 2 토픽).
        // Bybit 서버 제약 상 한 번에 args 최대 10개 이므로 chunk 로 분할 전송.
        let mut topics: Vec<String> = Vec::with_capacity(symbols.len() * 2);
        for s in symbols {
            let wire = to_bybit_symbol(s.as_str());
            topics.push(format!("orderbook.1.{}", wire));
            topics.push(format!("publicTrade.{}", wire));
        }
        for chunk in topics.chunks(MAX_SUBSCRIBE_ARGS) {
            let sub = serde_json::json!({
                "op": "subscribe",
                "args": chunk,
            });
            write.send(Message::Text(sub.to_string())).await?;
        }
        info!(
            symbols = symbols.len(),
            topics = topics.len(),
            "bybit subscribed orderbook.1+publicTrade"
        );

        let read_to = Duration::from_secs(self.cfg.read_timeout_secs);
        let mut ping_iv =
            tokio::time::interval(Duration::from_secs(self.cfg.ping_interval_secs));
        ping_iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 첫 tick 은 즉시 발화 — skip.
        ping_iv.tick().await;

        // hot-path 재사용 버퍼.
        // - `bin_scratch`: gzip/zlib 해제 후 UTF-8 payload.
        //   text frame 은 `&str` 바이트를 직접 파싱하므로 별도 스크래치 불필요.
        let mut bin_scratch: Vec<u8> = Vec::with_capacity(self.cfg.scratch_capacity);

        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => {
                    info!("bybit cancel received");
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                _ = ping_iv.tick() => {
                    if let Err(e) = write.send(Message::Ping(Vec::new())).await {
                        warn!(error = %e, "bybit ping send failed");
                        return Err(e.into());
                    }
                    continue;
                }
                res = tokio::time::timeout(read_to, read.next()) => res,
            };

            let msg = match msg {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => {
                    warn!(error = %e, "bybit read error");
                    return Err(e.into());
                }
                Ok(None) => {
                    warn!("bybit stream ended");
                    return Ok(());
                }
                Err(_) => {
                    warn!(timeout_s = self.cfg.read_timeout_secs, "bybit read timeout");
                    return Err(anyhow::anyhow!("read timeout"));
                }
            };

            match msg {
                Message::Text(text) => {
                    let ws_recv = Stamp::now(&*self.clock);
                    let reply =
                        dispatch_text_bytes(text.as_bytes(), ws_recv, raw_to_sym, &self.cache, emit);
                    if let Some(out) = reply {
                        if let Err(e) = write.send(out).await {
                            warn!(error = %e, "bybit pong (text) send failed");
                            return Err(e.into());
                        }
                    }
                }
                Message::Binary(bytes) => {
                    let ws_recv = Stamp::now(&*self.clock);
                    match decode_into(&bytes, &mut bin_scratch) {
                        Ok(()) => {
                            let reply = dispatch_text_bytes(
                                &bin_scratch,
                                ws_recv,
                                raw_to_sym,
                                &self.cache,
                                emit,
                            );
                            if let Some(out) = reply {
                                if let Err(e) = write.send(out).await {
                                    warn!(error = %e, "bybit pong (text) send failed");
                                    return Err(e.into());
                                }
                            }
                        }
                        Err(e) => {
                            trace!(error = %e, len = bytes.len(), "bybit binary decode failed");
                        }
                    }
                }
                Message::Ping(p) => {
                    if let Err(e) = write.send(Message::Pong(p)).await {
                        warn!(error = %e, "bybit pong send failed");
                        return Err(e.into());
                    }
                }
                Message::Pong(_) => {}
                Message::Close(frame) => {
                    warn!(?frame, "bybit server close");
                    return Ok(());
                }
                _ => {}
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 타이핑 된 wire schema — borrow-only, hot-path alloc 제거
// ────────────────────────────────────────────────────────────────────────────

/// 수신 envelope. control frame (op 존재) 와 data frame 을 한 번에 파싱하여
/// 분기한다. `data` 는 object / array 둘 다 올 수 있으므로 `&RawValue` 로
/// 지연 파싱.
#[derive(Debug, Deserialize)]
struct BybitFrame<'a> {
    #[serde(borrow, default)]
    op: Option<&'a str>,
    #[serde(borrow, default)]
    topic: Option<&'a str>,
    /// subscribe ack 의 성공 여부.
    #[serde(default)]
    success: Option<bool>,
    /// subscribe ack / ping 의 correlation id.
    #[serde(borrow, default)]
    req_id: Option<&'a str>,
    /// 실패 시 서버 메시지.
    #[serde(borrow, default)]
    ret_msg: Option<&'a str>,
    /// Level-1 orderbook payload (object 또는 `[object]`).
    #[serde(borrow, default)]
    data: Option<&'a RawValue>,
    /// 프레임 server-time (ms).
    #[serde(default, deserialize_with = "deserialize_i64_lenient_opt")]
    ts: Option<i64>,
}

/// `data` object — Bybit 의 Level-1 orderbook.
///
/// `b` / `a` 는 `[price, size]` string array 로 오는데 `BookLevel::Deserialize`
/// 가 lenient 하게 f64 로 변환한다.
#[derive(Debug, Deserialize)]
struct BybitBook<'a> {
    /// wire symbol (예: `BTCUSDT`).
    #[serde(borrow, default)]
    s: &'a str,
    /// bid levels (orderbook.1 이면 1개).
    #[serde(borrow, default)]
    b: Vec<BookLevel>,
    /// ask levels.
    #[serde(borrow, default)]
    a: Vec<BookLevel>,
    /// data-scope timestamp (ms) — 일부 배포본.
    #[serde(default, deserialize_with = "deserialize_i64_lenient_opt")]
    ts: Option<i64>,
    /// match-engine timestamp (ms) — 일부 배포본.
    #[serde(default, deserialize_with = "deserialize_i64_lenient_opt")]
    cts: Option<i64>,
    #[serde(default)]
    #[allow(dead_code)]
    u: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    seq: Option<u64>,
}

/// `publicTrade.<SYM>` 의 `data` array element.
///
/// ```json
/// {"T":1672304486865,"s":"BTCUSDT","S":"Buy","v":"0.001","p":"16578.5",
///  "L":"PlusTick","i":"20f43950-d8dd-5b31-9112-a178eb6023af","BT":false}
/// ```
/// - `S` (Buy/Sell) → size 부호.
/// - `v` (qty) / `p` (price): string → f64 lenient.
/// - `i`: UUID 문자열 — i64 변환 시 hash fallback (`trade_id_from_str`).
/// - `BT`: "block trade" 플래그 — 내부로 매핑 (`is_internal`).
#[derive(Debug, Deserialize)]
struct TradeEntry<'a> {
    /// 체결 시각 ms.
    #[serde(default, rename = "T", deserialize_with = "deserialize_i64_lenient_opt")]
    trade_time_ms: Option<i64>,
    /// wire symbol.
    #[serde(borrow, default)]
    s: &'a str,
    /// Buy / Sell.
    #[serde(borrow, default, rename = "S")]
    side: &'a str,
    /// qty (string). 음수 없음.
    #[serde(default, rename = "v", deserialize_with = "deserialize_f64_lenient")]
    v: f64,
    /// price (string).
    #[serde(default, rename = "p", deserialize_with = "deserialize_f64_lenient")]
    p: f64,
    /// trade id — UUID 문자열 또는 숫자 문자열.
    #[serde(borrow, default, rename = "i")]
    i: &'a str,
    /// block trade flag. true → is_internal 로 매핑.
    #[serde(default, rename = "BT")]
    block_trade: Option<bool>,
}

/// `[price, size]` 튜플을 f64 쌍으로 파싱.
#[derive(Debug, Clone, Copy)]
struct BookLevel {
    price: f64,
    size: f64,
}

impl<'de> Deserialize<'de> for BookLevel {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // `#[serde(deserialize_with = ...)]` 를 array element 에 직접 달 수
        // 없어 newtype wrapper 로 lenient f64 파싱을 위임.
        #[derive(Deserialize)]
        struct F(#[serde(deserialize_with = "deserialize_f64_lenient")] f64);

        struct V;
        impl<'de> de::Visitor<'de> for V {
            type Value = BookLevel;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("[price, size] array")
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<BookLevel, A::Error> {
                let p: F = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &"2"))?;
                let s: F = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &"2"))?;
                // 추가 요소가 있어도 무시 (forward-compat).
                while seq.next_element::<de::IgnoredAny>()?.is_some() {}
                Ok(BookLevel {
                    price: p.0,
                    size: s.0,
                })
            }
        }
        d.deserialize_seq(V)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Dispatch (정적 함수) — 재연결/락 없이 hot-path 에서 반복 호출
// ────────────────────────────────────────────────────────────────────────────

/// wire symbol (`BTCUSDT`) → canonical `Symbol` 매핑. 구독 시점에 미리 빌드.
type RawSymMap = HashMap<String, Symbol, ahash::RandomState>;

fn build_raw_to_sym(symbols: &[Symbol], cache: &SymbolCache) -> RawSymMap {
    let mut m = RawSymMap::with_capacity_and_hasher(symbols.len(), ahash::RandomState::new());
    for s in symbols {
        let interned = cache.intern(s.as_str());
        let raw = to_bybit_symbol(s.as_str());
        m.insert(raw, interned);
    }
    m
}

/// 단일 프레임 dispatch. 데이터면 `emit`, 제어면 필요한 경우 reply `Message` 반환.
///
/// - 제어 프레임 (`op` 있음): subscribe ack / ping / pong 처리.
/// - 데이터 프레임 (`op` 없음 + `data` 있음): `bookTicker` emit.
///
/// 반환값이 `Some(Message)` 면 caller 가 해당 메시지를 write sink 에 즉시 send.
fn dispatch_text_bytes(
    bytes: &[u8],
    ws_recv: Stamp,
    raw_to_sym: &RawSymMap,
    cache: &SymbolCache,
    emit: &Emitter,
) -> Option<Message> {
    // ── Phase 1: envelope 파싱 ──────────────────────────────────────────
    let frame: BybitFrame<'_> = match serde_json::from_slice(bytes) {
        Ok(f) => f,
        Err(e) => {
            let snippet = snippet(bytes, 200);
            debug!(error = %e, snippet = %snippet, "bybit non-JSON");
            return None;
        }
    };

    // ── Phase 2: control frame 분기 ─────────────────────────────────────
    if let Some(op) = frame.op {
        return handle_control(op, frame.success, frame.req_id, frame.ret_msg, bytes);
    }

    // ── Phase 3: data frame ──────────────────────────────────────────────
    let Some(data_raw) = frame.data else {
        // data 없는 비-control 프레임은 조용히 무시 (heartbeat 등).
        let snippet = snippet(bytes, 120);
        trace!(snippet = %snippet, "bybit frame w/o op and data, skipped");
        return None;
    };

    // topic prefix 로 채널 분기. 미상 토픽은 drop.
    let topic = frame.topic.unwrap_or("");
    if topic.starts_with("orderbook.") {
        handle_book_frame(data_raw.get(), topic, frame.ts, ws_recv, raw_to_sym, cache, emit);
    } else if topic.starts_with("publicTrade.") {
        handle_trade_frame(data_raw.get(), topic, frame.ts, ws_recv, raw_to_sym, cache, emit);
    } else {
        trace!(topic, "bybit unknown topic — drop");
    }
    None
}

/// `orderbook.1.<SYM>` payload 처리.
fn handle_book_frame(
    data_str: &str,
    topic: &str,
    frame_ts: Option<i64>,
    ws_recv: Stamp,
    raw_to_sym: &RawSymMap,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    let book = match parse_book(data_str) {
        Ok(b) => b,
        Err(e) => {
            let snippet = str_snippet(data_str, 200);
            debug!(error = %e, snippet = %snippet, "bybit book parse failed");
            return;
        }
    };

    // delta 에서 top-of-book 미변경이면 b/a 가 비어있음 — skip.
    if book.b.is_empty() || book.a.is_empty() {
        trace!(topic, "bybit top-of-book unchanged, skipped");
        return;
    }

    let bid = &book.b[0];
    let ask = &book.a[0];

    // 심볼 해석: 우선 data.s, 없으면 topic 의 마지막 dot segment.
    let wire_sym = if !book.s.is_empty() {
        book.s
    } else {
        symbol_from_topic(topic)
    };
    if wire_sym.is_empty() {
        trace!("bybit missing symbol, skipped");
        return;
    }

    let sym = resolve_symbol(wire_sym, raw_to_sym, cache);
    let server_time_ms = frame_ts.or(book.ts).or(book.cts).unwrap_or(0);

    let bt = BookTicker {
        exchange: ExchangeId::Bybit,
        symbol: sym,
        bid_price: Price(bid.price),
        ask_price: Price(ask.price),
        bid_size: Size(bid.size),
        ask_size: Size(ask.size),
        event_time_ms: server_time_ms,
        server_time_ms,
    };

    let ls = stamps_with_ws(ws_recv, server_time_ms);
    emit(MarketEvent::BookTicker(bt), ls);
}

/// `publicTrade.<SYM>` payload 처리. `data` 는 array — 각 원소에 대해 1회 emit.
fn handle_trade_frame(
    data_str: &str,
    topic: &str,
    frame_ts: Option<i64>,
    ws_recv: Stamp,
    raw_to_sym: &RawSymMap,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    let entries: Vec<TradeEntry<'_>> = match serde_json::from_str(data_str) {
        Ok(v) => v,
        Err(e) => {
            let snippet = str_snippet(data_str, 200);
            debug!(error = %e, snippet = %snippet, "bybit trade parse failed");
            return;
        }
    };

    if entries.is_empty() {
        trace!(topic, "bybit trade array empty");
        return;
    }

    // topic fallback 은 최초 1회만 계산 — 대부분의 entry 에 `s` 가 들어있지만
    // 방어적으로 준비.
    let topic_sym = symbol_from_topic(topic);

    for e in entries {
        let wire_sym = if !e.s.is_empty() { e.s } else { topic_sym };
        if wire_sym.is_empty() {
            trace!("bybit trade entry missing symbol");
            continue;
        }
        let sym = resolve_symbol(wire_sym, raw_to_sym, cache);

        // side 부호 적용. 알 수 없으면 양수 (defensive — 대부분의 콜러가 부호
        // 없이 기록하는 것보다 0이 아닌 값이 안전).
        let signed_qty = match e.side {
            "Sell" | "sell" | "SELL" => -e.v,
            _ => e.v,
        };

        let trade_time_ms = e.trade_time_ms.or(frame_ts).unwrap_or(0);
        let trade_id = trade_id_from_str(e.i);

        let t = Trade {
            exchange: ExchangeId::Bybit,
            symbol: sym,
            price: Price(e.p),
            size: Size(signed_qty),
            trade_id,
            create_time_s: trade_time_ms / 1_000,
            create_time_ms: trade_time_ms,
            server_time_ms: trade_time_ms,
            is_internal: e.block_trade.unwrap_or(false),
        };

        let ls = stamps_with_ws(ws_recv, trade_time_ms);
        emit(MarketEvent::Trade(t), ls);
    }
}

/// Bybit trade id 변환. 숫자 문자열이면 parse, 아니면 stable 64-bit hash 의 하위
/// 63bit 를 양수 i64 로 매핑.
///
/// 같은 UUID 에 대해 항상 같은 숫자를 반환해야 하므로 seed-free `ahash::RandomState`
/// 는 사용하지 않는다. 대신 `ahash::AHasher` 의 고정 시드 파생을 사용.
fn trade_id_from_str(s: &str) -> i64 {
    if s.is_empty() {
        return 0;
    }
    if let Ok(n) = s.parse::<i64>() {
        return n;
    }
    // 고정 시드 — 프로세스 / 세션 간 일관성 위해 상수. 4개의 non-zero u64.
    use std::hash::{BuildHasher, Hasher};
    const K0: u64 = 0xDEAD_BEEF_CAFE_BABE;
    const K1: u64 = 0x0123_4567_89AB_CDEF;
    const K2: u64 = 0xFEDC_BA98_7654_3210;
    const K3: u64 = 0xBADF_00D_0C0F_FEE0;
    let state = ahash::RandomState::with_seeds(K0, K1, K2, K3);
    let mut h = state.build_hasher();
    h.write(s.as_bytes());
    // 하위 63bit 만 사용 → 항상 양수.
    (h.finish() & 0x7FFF_FFFF_FFFF_FFFF) as i64
}

/// `&str` 을 안전한 debug snippet 으로 자른다.
fn str_snippet(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // char boundary safe cut.
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        s[..end].to_string()
    }
}

/// `data` 가 object 형 / array-of-object 형 둘 다 올 수 있어 두 단계 fallback.
///
/// 반환 lifetime 은 `s` 에 바인딩된다 — 내부 `&str` 은 `s` 를 borrow.
fn parse_book(s: &str) -> Result<BybitBook<'_>, serde_json::Error> {
    // object 단일 케이스 (표준).
    match serde_json::from_str::<BybitBook<'_>>(s) {
        Ok(b) => Ok(b),
        Err(obj_err) => {
            // array-of-object 케이스 (legacy / rare).
            match serde_json::from_str::<Vec<BybitBook<'_>>>(s) {
                Ok(mut v) if !v.is_empty() => Ok(v.remove(0)),
                // array 성공했지만 비어있거나 array 파싱도 실패 → 원래 object err 로 반환.
                _ => Err(obj_err),
            }
        }
    }
}

/// `op` 있는 제어 프레임 처리. 필요 시 reply `Message::Text` 반환.
fn handle_control(
    op: &str,
    success: Option<bool>,
    req_id: Option<&str>,
    ret_msg: Option<&str>,
    raw_bytes: &[u8],
) -> Option<Message> {
    match op {
        "subscribe" => {
            if success.unwrap_or(false) {
                debug!(req_id = ?req_id, "bybit subscribe confirmed");
            } else {
                let snippet = snippet(raw_bytes, 200);
                error!(
                    req_id = ?req_id,
                    ret_msg = ?ret_msg,
                    snippet = %snippet,
                    "bybit subscribe FAILED"
                );
            }
            None
        }
        "ping" => {
            // 서버 발신 JSON ping → pong 응답 (req_id echo).
            let pong = if let Some(id) = req_id {
                serde_json::json!({"op": "pong", "req_id": id}).to_string()
            } else {
                r#"{"op":"pong"}"#.to_string()
            };
            Some(Message::Text(pong))
        }
        "pong" => {
            trace!("bybit pong ack");
            None
        }
        other => {
            trace!(op = other, "bybit control frame");
            None
        }
    }
}

/// wire symbol → canonical `Symbol`. 미리 빌드된 `raw_to_sym` 테이블 hit 시
/// clone 1회로 끝. miss 시 `from_bybit_symbol` + `cache.intern` fallback.
#[inline]
fn resolve_symbol(wire_sym: &str, raw_to_sym: &RawSymMap, cache: &SymbolCache) -> Symbol {
    if let Some(sym) = raw_to_sym.get(wire_sym) {
        return sym.clone();
    }
    let canonical = from_bybit_symbol(wire_sym);
    cache.intern(&canonical)
}

/// `orderbook.1.BTCUSDT` → `BTCUSDT` (마지막 dot segment).
#[inline]
fn symbol_from_topic(topic: &str) -> &str {
    match topic.rsplit_once('.') {
        Some((_, s)) => s,
        None => "",
    }
}

/// Debug log 용 UTF-8 safe snippet (멀티바이트 경계 보호).
fn snippet(bytes: &[u8], max: usize) -> String {
    let end = bytes.len().min(max);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// ────────────────────────────────────────────────────────────────────────────
// 심볼 변환
// ────────────────────────────────────────────────────────────────────────────

/// `BTC_USDT` → `BTCUSDT`.
fn to_bybit_symbol(sym: &str) -> String {
    sym.replace('_', "").to_uppercase()
}

/// `BTCUSDT` → `BTC_USDT`. quote suffix 우선: `USDT` > `USDC` > `USD`.
fn from_bybit_symbol(sym: &str) -> String {
    if sym.contains('_') {
        return sym.to_string();
    }
    for q in ["USDT", "USDC", "USD"] {
        if sym.ends_with(q) && sym.len() > q.len() {
            let base = &sym[..sym.len() - q.len()];
            return format!("{}_{}", base, q);
        }
    }
    sym.to_string()
}

// ────────────────────────────────────────────────────────────────────────────
// 압축 해제
// ────────────────────────────────────────────────────────────────────────────

/// gzip / zlib magic 기반 분기. plain UTF-8 는 그대로 복사.
///
/// 기존 구현은 gz → zlib → utf8 순서로 무조건 시도했는데, gzip 포맷이
/// 맞아도 implicit parse 가 느리므로 magic byte 를 먼저 본다.
/// - `1f 8b`: gzip
/// - `78 XX`: zlib (deflate) — XX ∈ {01, 5e, 9c, da} 등
/// - 그 외: plain
fn decode_into(bytes: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
    out.clear();
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        GzDecoder::new(bytes)
            .read_to_end(out)
            .map_err(|e| anyhow::anyhow!("bybit gzip decode failed: {}", e))?;
        return Ok(());
    }
    if bytes.len() >= 2 && bytes[0] == 0x78 {
        // zlib — 실패 시 plain 으로 fallback.
        if ZlibDecoder::new(bytes).read_to_end(out).is_ok() {
            return Ok(());
        }
        out.clear();
    }
    // plain: UTF-8 유효성은 파싱 단계에서 자연히 걸러짐.
    out.extend_from_slice(bytes);
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// type asserts
// ────────────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
fn _type_asserts() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BybitFeed>();
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hft_time::MockClock;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // 공통 헬퍼 ─────────────────────────────────────────────────────────

    fn make_symbols() -> Vec<Symbol> {
        vec![Symbol::new("BTC_USDT"), Symbol::new("ETH_USDT")]
    }

    fn make_raw_map(cache: &SymbolCache) -> RawSymMap {
        build_raw_to_sym(&make_symbols(), cache)
    }

    fn emit_counter() -> (Emitter, Arc<AtomicUsize>, Arc<Mutex<Vec<BookTicker>>>) {
        let cnt = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(Vec::<BookTicker>::new()));
        let cnt_c = cnt.clone();
        let last_c = last.clone();
        let emit: Emitter = Arc::new(move |ev, _ls| {
            if let MarketEvent::BookTicker(bt) = ev {
                cnt_c.fetch_add(1, Ordering::SeqCst);
                last_c.lock().unwrap().push(bt);
            }
        });
        (emit, cnt, last)
    }

    fn sample_snapshot_bytes() -> Vec<u8> {
        serde_json::json!({
            "topic": "orderbook.1.BTCUSDT",
            "type": "snapshot",
            "data": {
                "s": "BTCUSDT",
                "b": [["40000.5", "3.2"]],
                "a": [["40001.0", "1.1"]],
                "u": 1,
                "seq": 100
            },
            "ts": 1_700_000_000_123i64
        })
        .to_string()
        .into_bytes()
    }

    fn sample_delta_empty_bytes() -> Vec<u8> {
        serde_json::json!({
            "topic": "orderbook.1.BTCUSDT",
            "type": "delta",
            "data": {
                "s": "BTCUSDT",
                "b": [],
                "a": [],
                "u": 2,
                "seq": 101
            },
            "ts": 1_700_000_000_200i64
        })
        .to_string()
        .into_bytes()
    }

    fn sample_data_array_bytes() -> Vec<u8> {
        serde_json::json!({
            "topic": "orderbook.1.ETHUSDT",
            "type": "snapshot",
            "data": [{
                "s": "ETHUSDT",
                "b": [["2500.1", "5"]],
                "a": [["2500.2", "7"]],
                "ts": 999i64
            }],
            "ts": 1000i64
        })
        .to_string()
        .into_bytes()
    }

    // ── BookLevel 파싱 ────────────────────────────────────────────────

    #[test]
    fn book_level_parses_string_pair() {
        let lv: BookLevel = serde_json::from_str(r#"["100.5","2.25"]"#).unwrap();
        assert_eq!(lv.price, 100.5);
        assert_eq!(lv.size, 2.25);
    }

    #[test]
    fn book_level_parses_numeric_pair() {
        let lv: BookLevel = serde_json::from_str("[100.5, 2.25]").unwrap();
        assert_eq!(lv.price, 100.5);
        assert_eq!(lv.size, 2.25);
    }

    #[test]
    fn book_level_rejects_single_element() {
        let r: Result<BookLevel, _> = serde_json::from_str("[\"100.5\"]");
        assert!(r.is_err());
    }

    #[test]
    fn book_level_ignores_extra_fields() {
        // 3-tuple 이 와도 첫 2개만 쓰고 나머지 skip.
        let lv: BookLevel = serde_json::from_str(r#"["1","2","3"]"#).unwrap();
        assert_eq!(lv.price, 1.0);
        assert_eq!(lv.size, 2.0);
    }

    // ── 심볼 변환 ──────────────────────────────────────────────────────

    #[test]
    fn to_bybit_symbol_upper_no_underscore() {
        assert_eq!(to_bybit_symbol("btc_usdt"), "BTCUSDT");
        assert_eq!(to_bybit_symbol("ETH_USD"), "ETHUSD");
    }

    #[test]
    fn from_bybit_symbol_longest_match() {
        assert_eq!(from_bybit_symbol("BTCUSDT"), "BTC_USDT");
        assert_eq!(from_bybit_symbol("ETHUSDC"), "ETH_USDC");
        assert_eq!(from_bybit_symbol("ETHUSD"), "ETH_USD");
        assert_eq!(from_bybit_symbol("ETH_USDT"), "ETH_USDT");
        assert_eq!(from_bybit_symbol("EXOTIC"), "EXOTIC");
    }

    #[test]
    fn symbol_from_topic_extracts_last() {
        assert_eq!(symbol_from_topic("orderbook.1.BTCUSDT"), "BTCUSDT");
        assert_eq!(symbol_from_topic("orderbook.50.SOLUSDT"), "SOLUSDT");
        assert_eq!(symbol_from_topic(""), "");
        assert_eq!(symbol_from_topic("no-dots"), "");
    }

    // ── dispatch hot-path ──────────────────────────────────────────────

    fn recv_stamp() -> Stamp {
        Stamp {
            wall_ms: 10,
            mono_ns: 10,
        }
    }

    #[test]
    fn dispatch_snapshot_emits_bookticker() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, last) = emit_counter();
        let reply =
            dispatch_text_bytes(&sample_snapshot_bytes(), recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 1);
        let bts = last.lock().unwrap();
        let bt = bts.first().unwrap();
        assert_eq!(bt.exchange, ExchangeId::Bybit);
        assert_eq!(bt.symbol.as_str(), "BTC_USDT");
        assert_eq!(bt.bid_price.0, 40000.5);
        assert_eq!(bt.ask_price.0, 40001.0);
        assert_eq!(bt.bid_size.0, 3.2);
        assert_eq!(bt.ask_size.0, 1.1);
        assert_eq!(bt.server_time_ms, 1_700_000_000_123);
        assert!(bt.is_valid());
    }

    #[test]
    fn dispatch_delta_empty_is_skipped() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(
            &sample_delta_empty_bytes(),
            recv_stamp(),
            &raw,
            &cache,
            &emit,
        );
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_data_array_format() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, last) = emit_counter();
        let reply = dispatch_text_bytes(
            &sample_data_array_bytes(),
            recv_stamp(),
            &raw,
            &cache,
            &emit,
        );
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 1);
        let bts = last.lock().unwrap();
        let bt = bts.first().unwrap();
        assert_eq!(bt.symbol.as_str(), "ETH_USDT");
        assert_eq!(bt.bid_price.0, 2500.1);
        // top-level ts 우선.
        assert_eq!(bt.server_time_ms, 1000);
    }

    #[test]
    fn dispatch_resolves_symbol_from_topic_when_data_s_missing() {
        // data.s 를 일부러 비움 → topic 의 마지막 segment 를 쓴다.
        let bytes = serde_json::json!({
            "topic": "orderbook.1.SOLUSDT",
            "data": {
                "s": "",
                "b": [["100", "10"]],
                "a": [["101", "20"]]
            },
            "ts": 500i64
        })
        .to_string()
        .into_bytes();

        let cache = Arc::new(SymbolCache::new());
        let raw = build_raw_to_sym(&[Symbol::new("SOL_USDT")], &cache);
        let (emit, cnt, last) = emit_counter();
        let reply = dispatch_text_bytes(&bytes, recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 1);
        assert_eq!(last.lock().unwrap()[0].symbol.as_str(), "SOL_USDT");
    }

    #[test]
    fn dispatch_reuses_cached_symbol_across_frames() {
        // 같은 wire symbol 이 여러 프레임에 걸쳐 나와도 동일한 `Arc<str>` 포인터.
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, _cnt, last) = emit_counter();
        dispatch_text_bytes(&sample_snapshot_bytes(), recv_stamp(), &raw, &cache, &emit);
        dispatch_text_bytes(&sample_snapshot_bytes(), recv_stamp(), &raw, &cache, &emit);
        let bts = last.lock().unwrap();
        assert_eq!(bts.len(), 2);
        assert!(
            bts[0].symbol.ptr_eq(&bts[1].symbol),
            "symbol Arc must be identical across frames"
        );
    }

    #[test]
    fn dispatch_unknown_wire_symbol_falls_back_to_intern() {
        // raw_to_sym 에 없는 심볼 → from_bybit_symbol + intern fallback.
        let cache = Arc::new(SymbolCache::new());
        let raw = RawSymMap::with_hasher(ahash::RandomState::new());
        let (emit, cnt, last) = emit_counter();
        let reply =
            dispatch_text_bytes(&sample_snapshot_bytes(), recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 1);
        assert_eq!(last.lock().unwrap()[0].symbol.as_str(), "BTC_USDT");
        assert_eq!(cache.len(), 1);
    }

    // ── 제어 프레임 ─────────────────────────────────────────────────────

    #[test]
    fn dispatch_subscribe_success_no_reply_no_emit() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let bytes = br#"{"op":"subscribe","success":true,"req_id":"abc"}"#;
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_subscribe_failure_no_reply_logs() {
        // failure 는 emit 없이 silent — log 만 남기고 None 반환.
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let bytes =
            br#"{"op":"subscribe","success":false,"req_id":"x","ret_msg":"bad topic"}"#;
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_server_ping_replies_with_pong_echoing_req_id() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let bytes = br#"{"op":"ping","req_id":"abc"}"#;
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &raw, &cache, &emit);
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
        let Some(Message::Text(pong)) = reply else {
            panic!("expected pong text reply, got {:?}", reply);
        };
        assert!(pong.contains("\"op\":\"pong\""));
        assert!(pong.contains("\"req_id\":\"abc\""));
    }

    #[test]
    fn dispatch_server_ping_without_req_id_replies_with_plain_pong() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, _cnt, _) = emit_counter();
        let bytes = br#"{"op":"ping"}"#;
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &raw, &cache, &emit);
        let Some(Message::Text(pong)) = reply else {
            panic!("expected plain pong text reply, got {:?}", reply);
        };
        assert_eq!(pong, r#"{"op":"pong"}"#);
    }

    #[test]
    fn dispatch_pong_ack_no_reply() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, _cnt, _) = emit_counter();
        let bytes = br#"{"op":"pong"}"#;
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
    }

    // ── 에러/엣지 케이스 ────────────────────────────────────────────────

    #[test]
    fn dispatch_malformed_json_drops() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(b"not json at all", recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_frame_without_op_or_data_silently_drops() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(b"{\"topic\":\"x\"}", recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_missing_bids_drops() {
        // b 필드 없음 → empty default → top-of-book skip.
        let bytes = br#"{"topic":"orderbook.1.BTCUSDT","data":{"s":"BTCUSDT","a":[["1","1"]]},"ts":1}"#;
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    // ── 압축 해제 ───────────────────────────────────────────────────────

    #[test]
    fn decode_into_plain_utf8_passthrough() {
        let mut out = Vec::new();
        decode_into(br#"{"op":"pong"}"#, &mut out).unwrap();
        assert_eq!(&out, br#"{"op":"pong"}"#);
    }

    #[test]
    fn decode_into_gzip_roundtrip() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write as _;
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(br#"{"hello":"world"}"#).unwrap();
        let compressed = enc.finish().unwrap();

        let mut out = Vec::new();
        decode_into(&compressed, &mut out).unwrap();
        assert_eq!(&out, br#"{"hello":"world"}"#);
    }

    #[test]
    fn decode_into_zlib_roundtrip() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write as _;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(br#"{"z":"lib"}"#).unwrap();
        let compressed = enc.finish().unwrap();

        let mut out = Vec::new();
        decode_into(&compressed, &mut out).unwrap();
        assert_eq!(&out, br#"{"z":"lib"}"#);
    }

    // ── BybitFeed wiring ────────────────────────────────────────────────

    #[test]
    fn bybit_config_default_sane() {
        let c = BybitConfig::default();
        assert!(c.ws_url.starts_with("wss://"));
        assert!(c.rest_base_url.starts_with("https://"));
        assert!(c.backoff_base_ms < c.backoff_cap_ms);
        assert_eq!(c.ping_interval_secs, 20);
        assert!(c.scratch_capacity >= 512);
    }

    #[test]
    fn bybit_feed_id_and_role() {
        let f = BybitFeed::new(BybitConfig::default());
        assert_eq!(f.id(), ExchangeId::Bybit);
        assert_eq!(f.role(), DataRole::Primary);
        assert_eq!(f.label(), "bybit:Primary");
    }

    #[test]
    fn bybit_feed_is_trait_object() {
        let _boxed: Arc<dyn ExchangeFeed> =
            Arc::new(BybitFeed::new(BybitConfig::default()));
    }

    #[test]
    fn bybit_feed_with_clock_uses_injected_clock() {
        let mock = MockClock::new(42, 7);
        let clock: Arc<dyn Clock> = mock.clone();
        let f = BybitFeed::with_clock(BybitConfig::default(), clock.clone());
        // mock 이 injection 됐는지 stamp 로 검증.
        let s = Stamp::now(&*f.clock());
        assert_eq!(s.wall_ms, 42);
    }

    #[test]
    fn bybit_feed_with_cache_shares_pool() {
        let shared = Arc::new(SymbolCache::new());
        let f1 = BybitFeed::with_cache(BybitConfig::default(), shared.clone());
        let f2 = BybitFeed::with_cache(BybitConfig::default(), shared.clone());
        let a = f1.symbol_cache().intern("BTC_USDT");
        let b = f2.symbol_cache().intern("BTC_USDT");
        assert!(a.ptr_eq(&b), "cache must be shared between feed instances");
    }

    #[test]
    fn bybit_feed_with_clock_and_cache_uses_both() {
        let mock = MockClock::new(100, 3);
        let clock: Arc<dyn Clock> = mock.clone();
        let shared = Arc::new(SymbolCache::new());
        let f = BybitFeed::with_clock_and_cache(
            BybitConfig::default(),
            clock.clone(),
            shared.clone(),
        );
        let s = Stamp::now(&*f.clock());
        assert_eq!(s.wall_ms, 100);
        assert!(Arc::ptr_eq(f.symbol_cache(), &shared));
    }

    #[tokio::test]
    async fn empty_symbols_stream_is_noop() {
        let f = BybitFeed::new(BybitConfig::default());
        let cancel = CancellationToken::new();
        let cnt = Arc::new(AtomicUsize::new(0));
        let c = cnt.clone();
        let emit: Emitter = Arc::new(move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        f.stream(&[], emit, cancel).await.unwrap();
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    // ── build_raw_to_sym ────────────────────────────────────────────────

    #[test]
    fn build_raw_to_sym_maps_wire_to_canonical() {
        let cache = Arc::new(SymbolCache::new());
        let m = build_raw_to_sym(
            &[Symbol::new("BTC_USDT"), Symbol::new("ETH_USDC")],
            &cache,
        );
        assert_eq!(m.len(), 2);
        assert_eq!(m.get("BTCUSDT").map(|s| s.as_str()), Some("BTC_USDT"));
        assert_eq!(m.get("ETHUSDC").map(|s| s.as_str()), Some("ETH_USDC"));
    }

    // ── stamps / latency ────────────────────────────────────────────────

    // ── publicTrade ────────────────────────────────────────────────────

    fn emit_capture_all() -> (Emitter, Arc<Mutex<Vec<(MarketEvent, hft_time::LatencyStamps)>>>) {
        let buf: Arc<Mutex<Vec<_>>> = Arc::new(Mutex::new(Vec::new()));
        let b = buf.clone();
        let e: Emitter = Arc::new(move |ev, ls| {
            b.lock().unwrap().push((ev, ls));
        });
        (e, buf)
    }

    fn sample_trade_buy_bytes() -> Vec<u8> {
        // Bybit v5 publicTrade — side=Buy 매수 aggressor → 양수 size.
        r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1700000000300,"data":[{"T":1700000000299,"s":"BTCUSDT","S":"Buy","v":"0.5","p":"40010.5","L":"PlusTick","i":"abc-uuid-1","BT":false}]}"#
            .as_bytes()
            .to_vec()
    }

    fn sample_trade_sell_bytes() -> Vec<u8> {
        r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1700000000400,"data":[{"T":1700000000399,"s":"BTCUSDT","S":"Sell","v":"1.25","p":"40005.0","L":"MinusTick","i":"123456789","BT":false}]}"#
            .as_bytes()
            .to_vec()
    }

    fn sample_trade_batch_bytes() -> Vec<u8> {
        // 한 프레임에 여러 체결. block trade 1건 포함.
        r#"{"topic":"publicTrade.ETHUSDT","type":"snapshot","ts":1700000000500,"data":[
            {"T":1700000000498,"s":"ETHUSDT","S":"Buy","v":"2","p":"2500","i":"t1","BT":false},
            {"T":1700000000499,"s":"ETHUSDT","S":"Sell","v":"3","p":"2501","i":"t2","BT":true}
        ]}"#
            .as_bytes()
            .to_vec()
    }

    #[test]
    fn dispatch_trade_buy_emits_positive_size() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, buf) = emit_capture_all();

        let reply =
            dispatch_text_bytes(&sample_trade_buy_bytes(), recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::Trade(t) = &events[0].0 else {
            panic!("expected Trade");
        };
        assert_eq!(t.exchange, ExchangeId::Bybit);
        assert_eq!(t.symbol.as_str(), "BTC_USDT");
        assert_eq!(t.price.0, 40010.5);
        assert_eq!(t.size.0, 0.5); // Buy → 양수
        assert_eq!(t.create_time_ms, 1_700_000_000_299);
        assert_eq!(t.create_time_s, 1_700_000_000);
        assert_eq!(t.server_time_ms, 1_700_000_000_299);
        assert!(!t.is_internal);
        // UUID-ish string → hash id. 0 이 아니어야 하고 양수.
        assert!(t.trade_id != 0);
        assert!(t.trade_id > 0);
    }

    #[test]
    fn dispatch_trade_sell_emits_negative_size_and_parses_numeric_id() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, buf) = emit_capture_all();

        dispatch_text_bytes(&sample_trade_sell_bytes(), recv_stamp(), &raw, &cache, &emit);
        let events = buf.lock().unwrap();
        let MarketEvent::Trade(t) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(t.size.0, -1.25); // Sell → 음수
        // 순수 숫자 문자열 ID 는 parse 되어야.
        assert_eq!(t.trade_id, 123_456_789);
    }

    #[test]
    fn dispatch_trade_batch_emits_per_entry_and_block_flag() {
        let cache = Arc::new(SymbolCache::new());
        let raw = build_raw_to_sym(&[Symbol::new("ETH_USDT")], &cache);
        let (emit, buf) = emit_capture_all();

        dispatch_text_bytes(&sample_trade_batch_bytes(), recv_stamp(), &raw, &cache, &emit);
        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 2);

        let MarketEvent::Trade(t0) = &events[0].0 else {
            unreachable!()
        };
        let MarketEvent::Trade(t1) = &events[1].0 else {
            unreachable!()
        };
        assert_eq!(t0.size.0, 2.0);
        assert!(!t0.is_internal);
        assert_eq!(t1.size.0, -3.0);
        assert!(t1.is_internal, "BT=true → is_internal");
        // 같은 symbol Arc 공유.
        assert!(t0.symbol.ptr_eq(&t1.symbol));
    }

    #[test]
    fn dispatch_trade_missing_symbol_falls_back_to_topic() {
        // data[].s 가 비어도 topic 의 마지막 segment 를 symbol 로 사용.
        let bytes = r#"{"topic":"publicTrade.SOLUSDT","type":"snapshot","ts":1,"data":[{"T":1,"s":"","S":"Buy","v":"1","p":"100","i":"x"}]}"#
            .as_bytes()
            .to_vec();
        let cache = Arc::new(SymbolCache::new());
        let raw = build_raw_to_sym(&[Symbol::new("SOL_USDT")], &cache);
        let (emit, buf) = emit_capture_all();

        dispatch_text_bytes(&bytes, recv_stamp(), &raw, &cache, &emit);
        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::Trade(t) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(t.symbol.as_str(), "SOL_USDT");
    }

    #[test]
    fn dispatch_trade_empty_data_array_no_emit() {
        let bytes = br#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1,"data":[]}"#;
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_trade_malformed_data_drops() {
        // data 가 object 라 array 파싱 실패.
        let bytes = br#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","data":{"oops":true}}"#;
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_trade_falls_back_to_frame_ts_when_T_missing() {
        let bytes = r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1700000000999,"data":[{"s":"BTCUSDT","S":"Buy","v":"1","p":"1","i":"x"}]}"#.as_bytes().to_vec();
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, buf) = emit_capture_all();
        dispatch_text_bytes(&bytes, recv_stamp(), &raw, &cache, &emit);
        let events = buf.lock().unwrap();
        let MarketEvent::Trade(t) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(t.create_time_ms, 1_700_000_000_999);
    }

    #[test]
    fn dispatch_trade_unknown_side_defaults_positive() {
        // 서버가 다른 대소문자로 보낼 경우 대비. 알 수 없는 값은 양수 fallback.
        let bytes = r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1,"data":[{"T":1,"s":"BTCUSDT","S":"weird","v":"2","p":"1","i":"x"}]}"#.as_bytes().to_vec();
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, buf) = emit_capture_all();
        dispatch_text_bytes(&bytes, recv_stamp(), &raw, &cache, &emit);
        let MarketEvent::Trade(t) = &buf.lock().unwrap()[0].0 else {
            unreachable!()
        };
        assert!(t.size.0 > 0.0);
    }

    #[test]
    fn trade_id_from_str_is_stable_and_nonzero() {
        let a = trade_id_from_str("abc-uuid-xyz");
        let b = trade_id_from_str("abc-uuid-xyz");
        let c = trade_id_from_str("different-uuid");
        assert_eq!(a, b);
        assert!(a != c);
        assert!(a > 0, "hash is clamped to i63");
        assert_eq!(trade_id_from_str(""), 0);
        assert_eq!(trade_id_from_str("42"), 42);
    }

    #[test]
    fn dispatch_unknown_topic_silently_drops() {
        // trade 도 book 도 아닌 토픽 → no-op.
        let bytes = br#"{"topic":"kline.1m.BTCUSDT","type":"snapshot","data":[]}"#;
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &raw, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn stamps_captures_both_recv_and_server() {
        let cache = Arc::new(SymbolCache::new());
        let raw = make_raw_map(&cache);
        let stamps = Arc::new(Mutex::new(Option::<hft_time::LatencyStamps>::None));
        let stamps_c = stamps.clone();
        let emit: Emitter = Arc::new(move |_ev, ls| {
            *stamps_c.lock().unwrap() = Some(ls);
        });
        let ws = Stamp {
            wall_ms: 500,
            mono_ns: 12345,
        };
        let bytes = sample_snapshot_bytes();
        let _ = dispatch_text_bytes(&bytes, ws, &raw, &cache, &emit);

        let got = stamps.lock().unwrap().unwrap();
        assert_eq!(got.ws_received.wall_ms, 500);
        assert_eq!(got.exchange_server.wall_ms, 1_700_000_000_123);
        // serialized stage 는 미채움.
        assert_eq!(got.serialized.mono_ns, 0);
    }
}

//! hft-exchange-bitget — Bitget USDT-M Futures feed 구현.
//!
//! ## 설계
//!
//! - URL: `wss://ws.bitget.com/v2/ws/public`.
//! - 구독 프로토콜: symbol 당 `books1` + `trade` 두 채널을 동시에 구독.
//!   ```json
//!   {"op":"subscribe","args":[
//!     {"instType":"USDT-FUTURES","channel":"books1","instId":"BTCUSDT"},
//!     {"instType":"USDT-FUTURES","channel":"trade", "instId":"BTCUSDT"}, …]}
//!   ```
//! - `books1` 채널 — best bid/ask 만 포함하는 경량 orderbook.
//! - `trade` 채널 — public trades. `side="sell"` 이면 size 부호 음수.
//! - Client ping: 20초 interval. 주의: Bitget 은 **WS protocol Ping frame 이
//!   아니라 Message::Text("ping") 텍스트** 를 기대한다. 서버도 마찬가지로
//!   `"ping"` 문자열을 보내고 `"pong"` 문자열을 기대.
//! - 일부 환경에서는 gzip / zlib 압축 바이너리로 프레임이 도착할 수 있다 —
//!   `decode_into` 가 gzip magic → zlib magic → 원본 bytes 순으로 처리.
//! - **books1 Frame**: `{"action":"snapshot|update",
//!    "arg":{"instType":"USDT-FUTURES","channel":"books1","instId":"BTCUSDT"},
//!    "data":[{"bids":[["p","s"],...],"asks":[["p","s"],...],"ts":"..."}]}`.
//!   `ts` 는 ms (string 또는 number). `bids/asks` 가 비어있으면 skip.
//! - **trade Frame**: `{"action":"snapshot|update",
//!    "arg":{"instType":"USDT-FUTURES","channel":"trade","instId":"BTCUSDT"},
//!    "data":[{"ts":"1697694270252","price":"27715.9","size":"0.01",
//!             "side":"buy","tradeId":"1110273002"}, ...]}`.
//!   프레임 1건의 `data` 배열 길이만큼 [`MarketEvent::Trade`] emit.
//!
//! ## 심볼 변환
//!
//! - 내부: `BTC_USDT` → Bitget: `BTCUSDT` (uppercase, 언더스코어 제거).
//! - 수신 instId 는 구독 시 구축한 `InstMap` 으로 `Arc::clone` 만에 복원.
//!   미등록 instId 는 quote 접미사 (USDT / USDC / USD) fallback → `SymbolCache::intern`.
//!
//! ## 성능 — Phase 2 refactor
//!
//! - DOM (`serde_json::Value`) parsing 제거. typed borrow-based Deserialize 로 교체.
//!   string/number 혼용은 `deserialize_f64_lenient` / `deserialize_i64_lenient_opt`.
//! - `SymbolCache` 로 Arc<str> intern, hot path 는 `Arc::clone` 만.
//! - `JsonScratch` + 별도 `bin_scratch: Vec<u8>` 로 text/binary 둘 다 reusable buffer
//!   사용. gzip/zlib 압축 풀이도 scratch 로 in-place.
//! - `BookLevel` manual Deserialize 로 `[p, s]` 배열 형태를 zero-copy 파싱
//!   (f64 lenient).
//!
//! ## available_symbols
//!
//! - Phase 1 파리티: REST 조회 미구현 — 빈 벡터 반환. 상위 config 에서 주입.
//!
//! ## 계약
//!
//! - [`BitgetFeed::stream`] 은 내부 재연결. 상위에는 `Ok(())` (cancel) / `Err`
//!   (초기화 오류) 만 전파.
//! - `books1` 프레임 1건당 [`MarketEvent::BookTicker`] 를 **정확히 1회** emit.
//! - `trade` 프레임 1건당 `data[]` 길이만큼 [`MarketEvent::Trade`] emit.
//! - [`LatencyStamps`] 는 `exchange_server_ms` 와 `ws_received` 두 stage 채움.

#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]

pub mod executor;
pub use executor::{BitgetExecutor, BitgetExecutorConfig};

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use flate2::read::{GzDecoder, ZlibDecoder};
use futures_util::{SinkExt, StreamExt};
use serde::de::{Deserializer, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, trace, warn};
use url::Url;

use hft_exchange_api::{Backoff, CancellationToken, Emitter, ExchangeFeed};
use hft_exchange_common::{
    deserialize_f64_lenient, deserialize_i64_lenient_opt, stamps_with_ws, JsonScratch, SymbolCache,
};
use hft_time::{Clock, Stamp, SystemClock};
use hft_types::{BookTicker, DataRole, ExchangeId, MarketEvent, Price, Size, Symbol, Trade};

/// 한 subscribe 메시지당 args 개수 상한 (보수적) — 너무 크면 서버가 거부 가능.
const MAX_SUBSCRIBE_ARGS: usize = 50;

// ────────────────────────────────────────────────────────────────────────────
// BitgetConfig
// ────────────────────────────────────────────────────────────────────────────

/// Bitget feed 구성.
#[derive(Debug, Clone)]
pub struct BitgetConfig {
    /// Public WebSocket URL (v2).
    pub ws_url: String,
    /// REST API base (Phase 1 에서는 미사용 — 차후 확장용).
    pub rest_base_url: String,
    /// 상품군 구분자 — `"USDT-FUTURES"` 가 USDT-M perp.
    pub inst_type: String,
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
    /// JSON / 압축 scratch 버퍼 초기 capacity. books1 frame 은 ~500B ~ ~2KiB 범위.
    pub scratch_capacity: usize,
}

impl Default for BitgetConfig {
    fn default() -> Self {
        Self {
            ws_url: "wss://ws.bitget.com/v2/ws/public".into(),
            rest_base_url: "https://api.bitget.com".into(),
            inst_type: "USDT-FUTURES".into(),
            read_timeout_secs: 60,
            ping_interval_secs: 20,
            backoff_base_ms: 200,
            backoff_cap_ms: 5_000,
            rest_timeout_ms: 10_000,
            scratch_capacity: 4096,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// BitgetFeed
// ────────────────────────────────────────────────────────────────────────────

/// Bitget wire `instId` ("BTCUSDT") → canonical `Symbol` ("BTC_USDT") 매핑.
///
/// 구독 시점에 한 번 채우고 hot path 에선 `get` + `Arc::clone` 만 한다.
type InstMap = HashMap<String, Symbol, ahash::RandomState>;

/// Bitget USDT-FUTURES `books1` feed.
pub struct BitgetFeed {
    cfg: Arc<BitgetConfig>,
    clock: Arc<dyn Clock>,
    cache: Arc<SymbolCache>,
}

impl BitgetFeed {
    /// 기본 `SystemClock` + 새 `SymbolCache` 로 feed 생성.
    pub fn new(cfg: BitgetConfig) -> Self {
        Self::with_clock_and_cache(
            cfg,
            Arc::new(SystemClock::new()),
            Arc::new(SymbolCache::new()),
        )
    }

    /// 테스트 / MockClock 주입용.
    pub fn with_clock(cfg: BitgetConfig, clock: Arc<dyn Clock>) -> Self {
        Self::with_clock_and_cache(cfg, clock, Arc::new(SymbolCache::new()))
    }

    /// 여러 feed 가 `SymbolCache` 를 공유하도록 외부에서 주입.
    pub fn with_cache(cfg: BitgetConfig, cache: Arc<SymbolCache>) -> Self {
        Self::with_clock_and_cache(cfg, Arc::new(SystemClock::new()), cache)
    }

    /// 가장 일반적인 생성자 — clock 과 cache 를 모두 주입.
    pub fn with_clock_and_cache(
        cfg: BitgetConfig,
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

    pub fn config(&self) -> &BitgetConfig {
        &self.cfg
    }

    pub fn symbol_cache(&self) -> &Arc<SymbolCache> {
        &self.cache
    }
}

#[async_trait]
impl ExchangeFeed for BitgetFeed {
    fn id(&self) -> ExchangeId {
        ExchangeId::Bitget
    }

    fn role(&self) -> DataRole {
        DataRole::Primary
    }

    /// Phase 1 은 REST 기반 심볼 조회 미구현 — 빈 벡터 반환.
    /// 상위 레이어에서 config 로 직접 심볼 목록을 주입한다.
    async fn available_symbols(&self) -> anyhow::Result<Vec<Symbol>> {
        debug!("bitget available_symbols: REST not implemented (Phase 1) — returning empty");
        Ok(Vec::new())
    }

    async fn stream(
        &self,
        symbols: &[Symbol],
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        if symbols.is_empty() {
            debug!("bitget stream: no symbols — no-op");
            return Ok(());
        }
        let subs: Vec<Symbol> = symbols
            .iter()
            .filter(|s| !s.as_str().is_empty())
            .cloned()
            .collect();
        if subs.is_empty() {
            debug!("bitget stream: all empty — no-op");
            return Ok(());
        }

        // hot-path 를 위해 instId → Symbol 매핑과 wire-form inst_ids 를 선행 구축.
        self.cache.prewarm(subs.iter().map(|s| s.as_str()));
        let mut inst_map: InstMap = HashMap::with_hasher(ahash::RandomState::new());
        inst_map.reserve(subs.len());
        let mut inst_ids: Vec<String> = Vec::with_capacity(subs.len());
        for s in &subs {
            let iid = to_bitget_inst_id(s.as_str());
            let sym = self.cache.intern(s.as_str());
            inst_map.insert(iid.clone(), sym);
            inst_ids.push(iid);
        }
        let inst_map = Arc::new(inst_map);
        let inst_ids = Arc::new(inst_ids);

        self.run_primary(inst_ids, inst_map, emit, cancel).await
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Primary 루프
// ────────────────────────────────────────────────────────────────────────────

impl BitgetFeed {
    async fn run_primary(
        &self,
        inst_ids: Arc<Vec<String>>,
        inst_map: Arc<InstMap>,
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let mut backoff = Backoff::new(self.cfg.backoff_base_ms, self.cfg.backoff_cap_ms);
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let attempt = backoff.attempts();
            info!(attempt, "bitget connecting");

            let outcome = tokio::select! {
                _ = cancel.cancelled() => {
                    info!("bitget cancelled during connect");
                    return Ok(());
                }
                r = self.run_primary_once(&inst_ids, &inst_map, &emit, &cancel) => r,
            };

            match outcome {
                Ok(()) => {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    warn!("bitget disconnected cleanly, reconnecting");
                    backoff.reset();
                }
                Err(e) => {
                    warn!(error = %e, "bitget error; backoff");
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
        inst_ids: &[String],
        inst_map: &InstMap,
        emit: &Emitter,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let url = Url::parse(&self.cfg.ws_url)?;
        let (ws, resp) = connect_async(url.as_str()).await?;
        info!(status = ?resp.status(), "bitget connected");
        let (mut write, mut read) = ws.split();

        // 구독: symbol 당 books1 + trade 두 채널. chunk 크기는 인자 수 기준.
        let per_symbol: usize = 2;
        let chunk_sz = (MAX_SUBSCRIBE_ARGS / per_symbol).max(1);
        for chunk in inst_ids.chunks(chunk_sz) {
            let mut args: Vec<serde_json::Value> = Vec::with_capacity(chunk.len() * per_symbol);
            for iid in chunk {
                args.push(serde_json::json!({
                    "instType": self.cfg.inst_type,
                    "channel": "books1",
                    "instId": iid,
                }));
                args.push(serde_json::json!({
                    "instType": self.cfg.inst_type,
                    "channel": "trade",
                    "instId": iid,
                }));
            }
            let sub = serde_json::json!({ "op": "subscribe", "args": args });
            write.send(Message::Text(sub.to_string())).await?;
        }
        info!(symbols = inst_ids.len(), "bitget subscribed books1 + trade");

        let read_to = Duration::from_secs(self.cfg.read_timeout_secs);
        let mut ping_iv = tokio::time::interval(Duration::from_secs(self.cfg.ping_interval_secs));
        ping_iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 첫 tick 은 즉시 발화 — skip.
        ping_iv.tick().await;

        let mut text_scratch = JsonScratch::with_capacity(self.cfg.scratch_capacity);
        let mut bin_scratch: Vec<u8> = Vec::with_capacity(self.cfg.scratch_capacity);

        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => {
                    info!("bitget cancel received");
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                _ = ping_iv.tick() => {
                    // Bitget: text "ping" (WS protocol Ping frame 이 아님).
                    if let Err(e) = write.send(Message::Text("ping".into())).await {
                        warn!(error = %e, "bitget ping send failed");
                        return Err(e.into());
                    }
                    continue;
                }
                res = tokio::time::timeout(read_to, read.next()) => res,
            };

            let msg = match msg {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => {
                    warn!(error = %e, "bitget read error");
                    return Err(e.into());
                }
                Ok(None) => {
                    warn!("bitget stream ended");
                    return Ok(());
                }
                Err(_) => {
                    warn!(
                        timeout_s = self.cfg.read_timeout_secs,
                        "bitget read timeout"
                    );
                    return Err(anyhow::anyhow!("read timeout"));
                }
            };

            match msg {
                Message::Text(text) => {
                    if is_control_text(&text) {
                        trace!(ctrl = %text.trim(), "bitget text control");
                        continue;
                    }
                    let ws_recv = Stamp::now(&*self.clock);
                    let bytes = text_scratch.reset_and_fill(&text);
                    dispatch_primary_bytes(bytes, ws_recv, inst_map, &self.cache, emit);
                }
                Message::Binary(bin) => {
                    let ws_recv = Stamp::now(&*self.clock);
                    bin_scratch.clear();
                    match decode_into(&bin, &mut bin_scratch) {
                        Ok(()) => {
                            dispatch_primary_bytes(
                                &bin_scratch,
                                ws_recv,
                                inst_map,
                                &self.cache,
                                emit,
                            );
                        }
                        Err(e) => {
                            trace!(error = %e, len = bin.len(), "bitget binary decode failed");
                        }
                    }
                }
                // Bitget 은 문자열 ping/pong 을 쓰지만, 프록시/브라우저 중간 노드가
                // WS protocol Ping frame 을 쓸 수도 있으므로 방어적으로 Pong 반사.
                Message::Ping(p) => {
                    if let Err(e) = write.send(Message::Pong(p)).await {
                        warn!(error = %e, "bitget ws-pong send failed");
                        return Err(e.into());
                    }
                }
                Message::Pong(_) => {}
                Message::Close(frame) => {
                    warn!(?frame, "bitget server close");
                    return Ok(());
                }
                _ => {}
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Typed schemas — borrow-based
// ────────────────────────────────────────────────────────────────────────────

/// 최상위 WS frame. `event` 가 있으면 제어 메시지, `data` 가 있으면 데이터.
///
/// `data` 는 채널별로 스키마가 다르므로 `&RawValue` 로 지연 파싱. 2차 파싱은
/// `arg.channel` 분기 후 채널 전용 struct 로 수행.
#[derive(Deserialize)]
struct BitgetFrame<'a> {
    #[serde(borrow, default)]
    event: Option<&'a str>,
    #[serde(borrow, default)]
    code: Option<&'a str>,
    #[serde(borrow, default)]
    msg: Option<&'a str>,
    #[serde(borrow, default)]
    arg: Option<BitgetArg<'a>>,
    #[serde(borrow, default)]
    data: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct BitgetArg<'a> {
    #[serde(rename = "instId", borrow, default)]
    inst_id: Option<&'a str>,
    #[serde(borrow, default)]
    channel: Option<&'a str>,
}

/// books1 의 한 snapshot/update 엔트리.
#[derive(Deserialize)]
struct BooksEntry<'a> {
    #[serde(default)]
    bids: Vec<BookLevel>,
    #[serde(default)]
    asks: Vec<BookLevel>,
    /// `ts` (ms) — string 또는 number.
    #[serde(default, deserialize_with = "deserialize_i64_lenient_opt")]
    ts: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_i64_lenient_opt")]
    timestamp: Option<i64>,
    /// 일부 schema drift 에 대비해 미사용 필드는 무시.
    #[serde(borrow, default)]
    #[allow(dead_code)]
    checksum: Option<&'a str>,
}

/// `trade` 채널의 한 체결 엔트리.
///
/// Bitget v2 공식 스키마:
/// `{"ts":"…","price":"…","size":"…","side":"buy"|"sell","tradeId":"…"}`.
/// `price`/`size`/`ts`/`tradeId` 모두 string 기본이지만 lenient 파싱으로 수치형도
/// 수용. `side` 는 문자열 enum.
#[derive(Deserialize)]
struct TradeEntry<'a> {
    #[serde(default, deserialize_with = "deserialize_i64_lenient_opt")]
    ts: Option<i64>,
    #[serde(deserialize_with = "deserialize_f64_lenient")]
    price: f64,
    #[serde(deserialize_with = "deserialize_f64_lenient")]
    size: f64,
    #[serde(borrow, default)]
    side: Option<&'a str>,
    #[serde(rename = "tradeId", borrow, default)]
    trade_id: Option<&'a str>,
}

/// `["price", "size"]` 형태의 level — f64 lenient 로 파싱.
#[derive(Debug, Clone, Copy)]
struct BookLevel {
    price: f64,
    size: f64,
}

impl<'de> Deserialize<'de> for BookLevel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = BookLevel;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a [price, size] tuple (string or number elements)")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<BookLevel, A::Error> {
                // f64 lenient 를 inline 위해 newtype helper 사용.
                #[derive(Deserialize)]
                struct F(#[serde(deserialize_with = "deserialize_f64_lenient")] f64);
                let F(price) = seq
                    .next_element::<F>()?
                    .ok_or_else(|| serde::de::Error::custom("level missing price"))?;
                let size = match seq.next_element::<F>()? {
                    Some(F(s)) => s,
                    None => 0.0,
                };
                // 이후 원소는 무시 (size 이후 meta 필드 호환).
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(BookLevel { price, size })
            }
        }
        d.deserialize_seq(V)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Hot-path dispatch
// ────────────────────────────────────────────────────────────────────────────

/// WS text (또는 decoded binary) bytes → [`MarketEvent`] emit.
///
/// - 제어 메시지 (`event` 필드가 있는 subscribe/error) 는 로그만.
/// - `arg.channel` 기반으로 데이터 분기:
///   - `books1`: best bid/ask → [`MarketEvent::BookTicker`] 1 건.
///   - `trade`: 체결 배열 → 원소 수만큼 [`MarketEvent::Trade`].
/// - 미지원 채널은 조용히 trace 후 drop.
fn dispatch_primary_bytes(
    bytes: &[u8],
    ws_recv: Stamp,
    inst_map: &InstMap,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    let frame: BitgetFrame<'_> = match serde_json::from_slice(bytes) {
        Ok(f) => f,
        Err(e) => {
            debug!(error = %e, snippet = snippet(bytes), "bitget non-JSON");
            return;
        }
    };

    // 제어 메시지: subscribe / error 등.
    if let Some(event) = frame.event {
        match event {
            "subscribe" => debug!("bitget subscribe confirmed"),
            "error" => {
                let code = frame.code.unwrap_or("?");
                let m = frame.msg.unwrap_or("?");
                error!(code, msg = m, "bitget server error event");
            }
            other => trace!(event = other, "bitget control event"),
        }
        return;
    }

    // 데이터 프레임.
    let Some(arg) = frame.arg else {
        debug!(snippet = snippet(bytes), "bitget missing arg");
        return;
    };
    let Some(inst_id) = arg.inst_id else {
        debug!(snippet = snippet(bytes), "bitget missing arg.instId");
        return;
    };
    let Some(raw_data) = frame.data else {
        debug!(snippet = snippet(bytes), "bitget missing data");
        return;
    };
    let channel = arg.channel.unwrap_or("");

    match channel {
        "books1" => handle_books1_frame(inst_id, raw_data, ws_recv, inst_map, cache, emit),
        "trade" => handle_trade_frame(inst_id, raw_data, ws_recv, inst_map, cache, emit),
        other => {
            trace!(channel = other, inst_id, "bitget unhandled channel — drop");
        }
    }
}

/// books1 프레임 처리: `data[0]` 의 best bid/ask 로 `BookTicker` 1 건 emit.
fn handle_books1_frame(
    inst_id: &str,
    raw_data: &RawValue,
    ws_recv: Stamp,
    inst_map: &InstMap,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    let data_vec: Vec<BooksEntry<'_>> = match serde_json::from_str(raw_data.get()) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, inst_id, "bitget books1 data parse failed");
            return;
        }
    };
    let Some(entry) = data_vec.into_iter().next() else {
        trace!(inst_id, "bitget empty books1 data array");
        return;
    };

    // best-level 유효성 — 빈 배열 / 비정상은 drop.
    if entry.bids.is_empty() || entry.asks.is_empty() {
        trace!(
            inst = inst_id,
            "bitget best level empty (delta w/o top-of-book) — drop"
        );
        return;
    }
    let best_bid = entry.bids[0];
    let best_ask = entry.asks[0];

    let symbol = resolve_symbol(inst_id, inst_map, cache);
    let server_time_ms = entry.ts.or(entry.timestamp).unwrap_or(0);

    let out = BookTicker {
        exchange: ExchangeId::Bitget,
        symbol,
        bid_price: Price(best_bid.price),
        ask_price: Price(best_ask.price),
        bid_size: Size(best_bid.size),
        ask_size: Size(best_ask.size),
        event_time_ms: server_time_ms,
        server_time_ms,
    };

    let ls = stamps_with_ws(ws_recv, server_time_ms);
    emit(MarketEvent::BookTicker(out), ls);
}

/// trade 프레임 처리: `data[i]` 각각 `Trade` 1 건 emit. `side="sell"` 은 음수.
fn handle_trade_frame(
    inst_id: &str,
    raw_data: &RawValue,
    ws_recv: Stamp,
    inst_map: &InstMap,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    let entries: Vec<TradeEntry<'_>> = match serde_json::from_str(raw_data.get()) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, inst_id, "bitget trade data parse failed");
            return;
        }
    };
    if entries.is_empty() {
        trace!(inst_id, "bitget empty trade data array");
        return;
    }

    let sym = resolve_symbol(inst_id, inst_map, cache);

    for e in entries {
        let signed_size = match e.side {
            Some("sell") | Some("SELL") | Some("Sell") => -e.size,
            _ => e.size,
        };
        let create_time_ms = e.ts.unwrap_or(0);
        let create_time_s = if create_time_ms > 0 {
            create_time_ms / 1_000
        } else {
            0
        };
        let trade_id = match e.trade_id {
            Some(tid) if !tid.is_empty() => parse_or_hash_trade_id(tid),
            _ => 0,
        };
        let t = Trade {
            exchange: ExchangeId::Bitget,
            symbol: sym.clone(),
            price: Price(e.price),
            size: Size(signed_size),
            trade_id,
            create_time_s,
            create_time_ms,
            server_time_ms: create_time_ms,
            is_internal: false,
        };
        let ls = stamps_with_ws(ws_recv, create_time_ms);
        emit(MarketEvent::Trade(t), ls);
    }
}

/// instId → canonical `Symbol`. map hit 시 clone 1 회, miss 는 fallback intern.
#[inline]
fn resolve_symbol(inst_id: &str, inst_map: &InstMap, cache: &SymbolCache) -> Symbol {
    if let Some(sym) = inst_map.get(inst_id) {
        return sym.clone();
    }
    let canonical = from_bitget_inst_id(inst_id);
    cache.intern(&canonical)
}

/// Bitget `tradeId` 는 수치 문자열. i64 파싱 성공하면 MSB mask, 실패 시
/// u64 fallback, 그래도 실패 시 fixed-seed ahash 로 i63 양수 해시.
#[inline]
fn parse_or_hash_trade_id(s: &str) -> i64 {
    if let Ok(n) = s.parse::<i64>() {
        return n & 0x7FFF_FFFF_FFFF_FFFF;
    }
    if let Ok(u) = s.parse::<u64>() {
        return (u & 0x7FFF_FFFF_FFFF_FFFF) as i64;
    }
    const K0: u64 = 0xDEAD_BEEF_CAFE_BABE;
    const K1: u64 = 0x0123_4567_89AB_CDEF;
    const K2: u64 = 0xFEDC_BA98_7654_3210;
    const K3: u64 = 0x0BAD_F00D_0C0F_FEE0;
    let state = ahash::RandomState::with_seeds(K0, K1, K2, K3);
    (state.hash_one(s) & 0x7FFF_FFFF_FFFF_FFFF) as i64
}

// ────────────────────────────────────────────────────────────────────────────
// 심볼 변환
// ────────────────────────────────────────────────────────────────────────────

fn to_bitget_inst_id(sym: &str) -> String {
    let mut s = sym.replace('_', "");
    s.make_ascii_uppercase();
    s
}

fn from_bitget_inst_id(inst_id: &str) -> String {
    if inst_id.contains('_') {
        return inst_id.to_string();
    }
    for q in ["USDT", "USDC", "USD"] {
        if inst_id.ends_with(q) && inst_id.len() > q.len() {
            let base = &inst_id[..inst_id.len() - q.len()];
            return format!("{}_{}", base, q);
        }
    }
    inst_id.to_string()
}

// ────────────────────────────────────────────────────────────────────────────
// 압축 해제 (gzip / zlib / raw)
// ────────────────────────────────────────────────────────────────────────────

/// 바이너리 프레임을 `out` 에 in-place 압축 해제.
///
/// - gzip magic (`1f 8b`) 또는 zlib magic (`78 XX`) 을 먼저 확인해 올바른 decoder 선택.
/// - 어떤 decoder 도 실패하면 원본 bytes 를 그대로 out 에 복사 (UTF-8 plain JSON 가정).
fn decode_into(bytes: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
    out.clear();
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        GzDecoder::new(bytes)
            .read_to_end(out)
            .map_err(|e| anyhow::anyhow!("gzip decode: {e}"))?;
        return Ok(());
    }
    // zlib magic: 1st byte 0x78, 2nd byte 중 일부 조합. 성공하면 OK, 실패 시 plain fallback.
    if bytes.len() >= 2 && bytes[0] == 0x78 {
        if ZlibDecoder::new(bytes).read_to_end(out).is_ok() {
            return Ok(());
        }
        out.clear();
    }
    // plain — JSON parser 가 직접 UTF-8 검증.
    out.extend_from_slice(bytes);
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// 내부 유틸
// ────────────────────────────────────────────────────────────────────────────

/// `"ping"` / `"pong"` / `"PONG"` 등 순수 제어 텍스트 여부.
fn is_control_text(text: &str) -> bool {
    let t = text.trim();
    t.eq_ignore_ascii_case("ping") || t.eq_ignore_ascii_case("pong")
}

/// 에러 로그용 safe snippet (UTF-8 경계 보존, 최대 200B).
fn snippet(bytes: &[u8]) -> String {
    let n = bytes.len().min(200);
    String::from_utf8_lossy(&bytes[..n]).into_owned()
}

#[allow(dead_code)]
fn _type_asserts() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BitgetFeed>();
    assert_send_sync::<Arc<BitgetFeed>>();
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // ─── fixtures ───────────────────────────────────────────────────────────

    fn sample_snapshot_text() -> String {
        r#"{"action":"snapshot","arg":{"instType":"USDT-FUTURES","channel":"books1","instId":"BTCUSDT"},"data":[{"bids":[["40000.5","3.2"]],"asks":[["40001.0","1.1"]],"ts":"1700000000000"}]}"#
            .to_string()
    }

    fn mk_inst_map(cache: &SymbolCache, canonicals: &[&str]) -> InstMap {
        let mut m: InstMap = HashMap::with_hasher(ahash::RandomState::new());
        for s in canonicals {
            let raw = to_bitget_inst_id(s);
            let sym = cache.intern(s);
            m.insert(raw, sym);
        }
        m
    }

    type CapturedEvents = Arc<Mutex<Vec<(MarketEvent, hft_time::LatencyStamps)>>>;

    fn capture_emitter() -> (Emitter, CapturedEvents) {
        let buf: CapturedEvents = Arc::new(Mutex::new(Vec::new()));
        let b = buf.clone();
        let e: Emitter = Arc::new(move |ev, ls| {
            b.lock().unwrap().push((ev, ls));
        });
        (e, buf)
    }

    fn count_emitter() -> (Emitter, Arc<AtomicUsize>) {
        let cnt = Arc::new(AtomicUsize::new(0));
        let c = cnt.clone();
        let emit: Emitter = Arc::new(move |_, _| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        (emit, cnt)
    }

    // ─── dispatch: happy paths ─────────────────────────────────────────────

    #[test]
    fn dispatch_snapshot_emits_bookticker() {
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let text = sample_snapshot_text();
        let bytes = text.as_bytes();
        dispatch_primary_bytes(
            bytes,
            Stamp {
                wall_ms: 10,
                mono_ns: 20,
            },
            &inst_map,
            &cache,
            &emit,
        );

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::BookTicker(bt) = &events[0].0 else {
            panic!("expected BookTicker");
        };
        assert_eq!(bt.exchange, ExchangeId::Bitget);
        assert_eq!(bt.symbol.as_str(), "BTC_USDT");
        assert!((bt.bid_price.0 - 40000.5).abs() < 1e-9);
        assert!((bt.ask_price.0 - 40001.0).abs() < 1e-9);
        assert!((bt.bid_size.0 - 3.2).abs() < 1e-9);
        assert!((bt.ask_size.0 - 1.1).abs() < 1e-9);
        assert_eq!(bt.server_time_ms, 1_700_000_000_000);
        assert_eq!(bt.event_time_ms, 1_700_000_000_000);

        let ls = &events[0].1;
        assert_eq!(ls.ws_received.wall_ms, 10);
        assert_eq!(ls.ws_received.mono_ns, 20);
        assert_eq!(ls.exchange_server.wall_ms, 1_700_000_000_000);
    }

    #[test]
    fn dispatch_update_with_numeric_ts_falls_back_to_intern() {
        // inst_map 에 없는 심볼 → cache.intern 으로 fallback.
        let cache = SymbolCache::new();
        let inst_map: InstMap = HashMap::with_hasher(ahash::RandomState::new());
        let (emit, buf) = capture_emitter();

        let bytes = br#"{"action":"update","arg":{"instType":"USDT-FUTURES","channel":"books1","instId":"ETHUSDT"},"data":[{"bids":[[2000.0,10.0]],"asks":[[2001.0,5.0]],"ts":1700000000001}]}"#;
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::BookTicker(bt) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(bt.symbol.as_str(), "ETH_USDT");
        assert_eq!(bt.server_time_ms, 1_700_000_000_001);
        assert!(!cache.is_empty()); // fallback intern 되었어야 함.
    }

    #[test]
    fn dispatch_lenient_price_string_vs_number() {
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        // channel 라우팅 이후 books1 로 들어가도록 명시.
        let bytes = br#"{"arg":{"channel":"books1","instId":"BTCUSDT"},"data":[{"bids":[["40000.5",3.2]],"asks":[[40001.0,"1.1"]],"ts":1}]}"#;
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::BookTicker(bt) = &events[0].0 else {
            unreachable!()
        };
        assert!((bt.bid_size.0 - 3.2).abs() < 1e-9);
        assert!((bt.ask_price.0 - 40001.0).abs() < 1e-9);
    }

    // ─── dispatch: control messages & edge cases ──────────────────────────

    #[test]
    fn dispatch_subscribe_ack_is_silent() {
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();

        let bytes = br#"{"event":"subscribe","arg":{"channel":"books1","instId":"BTCUSDT"}}"#;
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);

        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_error_event_is_silent() {
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();

        let bytes = br#"{"event":"error","code":"30001","msg":"invalid"}"#;
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);

        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_missing_data_drops() {
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();

        let bytes = br#"{"action":"snapshot","arg":{"instId":"BTCUSDT"}}"#;
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_empty_bids_drops() {
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();

        // books1 채널에서 bids 가 비면 drop (delta w/o top-of-book).
        let bytes = br#"{"arg":{"channel":"books1","instId":"BTCUSDT"},"data":[{"bids":[],"asks":[["1","1"]],"ts":"1"}]}"#;
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_missing_arg_drops() {
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();

        let bytes = br#"{"data":[{"bids":[["1","1"]],"asks":[["2","1"]],"ts":"1"}]}"#;
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_malformed_json_drops() {
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();

        let bytes = b"not json at all";
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_reuses_cached_symbol_across_frames() {
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let t = sample_snapshot_text();
        dispatch_primary_bytes(t.as_bytes(), Stamp::default(), &inst_map, &cache, &emit);
        dispatch_primary_bytes(t.as_bytes(), Stamp::default(), &inst_map, &cache, &emit);

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 2);
        let MarketEvent::BookTicker(bt0) = &events[0].0 else {
            unreachable!()
        };
        let MarketEvent::BookTicker(bt1) = &events[1].0 else {
            unreachable!()
        };
        assert!(
            bt0.symbol.ptr_eq(&bt1.symbol),
            "symbol Arc must be shared across frames"
        );
    }

    // ─── control text detection ─────────────────────────────────────────────

    #[test]
    fn is_control_text_matches_ping_pong() {
        assert!(is_control_text("ping"));
        assert!(is_control_text("pong"));
        assert!(is_control_text(" PING "));
        assert!(is_control_text("Pong\n"));
        assert!(!is_control_text(r#"{"event":"subscribe"}"#));
    }

    // ─── symbol conversion ─────────────────────────────────────────────────

    #[test]
    fn symbol_round_trip_usdt_and_usdc() {
        assert_eq!(to_bitget_inst_id("BTC_USDT"), "BTCUSDT");
        assert_eq!(from_bitget_inst_id("BTCUSDT"), "BTC_USDT");
        assert_eq!(to_bitget_inst_id("sol_usdc"), "SOLUSDC");
        assert_eq!(from_bitget_inst_id("SOLUSDC"), "SOL_USDC");
    }

    #[test]
    fn symbol_longest_match_prefers_usdt_over_usd() {
        assert_eq!(from_bitget_inst_id("BTCUSDT"), "BTC_USDT");
        assert_eq!(from_bitget_inst_id("BTCUSDC"), "BTC_USDC");
        assert_eq!(from_bitget_inst_id("BTCUSD"), "BTC_USD");
    }

    #[test]
    fn symbol_unknown_quote_passes_through() {
        assert_eq!(from_bitget_inst_id("BTCXYZ"), "BTCXYZ");
    }

    #[test]
    fn symbol_already_underscored_is_identity() {
        assert_eq!(from_bitget_inst_id("BTC_USDT"), "BTC_USDT");
    }

    // ─── compression ────────────────────────────────────────────────────────

    #[test]
    fn decode_into_roundtrip_gzip() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let original = br#"{"arg":{"instId":"BTCUSDT"}}"#;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(original).unwrap();
        let compressed = enc.finish().unwrap();
        let mut out = Vec::new();
        decode_into(&compressed, &mut out).unwrap();
        assert_eq!(out.as_slice(), original);
    }

    #[test]
    fn decode_into_roundtrip_zlib() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let original = br#"{"arg":{"instId":"ETHUSDT"}}"#;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(original).unwrap();
        let compressed = enc.finish().unwrap();
        let mut out = Vec::new();
        decode_into(&compressed, &mut out).unwrap();
        assert_eq!(out.as_slice(), original);
    }

    #[test]
    fn decode_into_plain_utf8_fallback() {
        let raw = b"plain utf-8";
        let mut out = Vec::new();
        decode_into(raw, &mut out).unwrap();
        assert_eq!(out.as_slice(), raw);
    }

    // ─── feed construction ─────────────────────────────────────────────────

    #[test]
    fn bitget_feed_with_cache_shares_pool() {
        let shared = Arc::new(SymbolCache::new());
        let f = BitgetFeed::with_cache(BitgetConfig::default(), shared.clone());
        assert!(Arc::ptr_eq(f.symbol_cache(), &shared));
    }

    #[test]
    fn feed_is_trait_object_safe() {
        let feed: Arc<dyn ExchangeFeed> = Arc::new(BitgetFeed::new(BitgetConfig::default()));
        assert_eq!(feed.id(), ExchangeId::Bitget);
        assert_eq!(feed.role(), DataRole::Primary);
        assert!(feed.label().contains("bitget"));
    }

    #[test]
    fn config_defaults_are_sane() {
        let c = BitgetConfig::default();
        assert!(c.ws_url.starts_with("wss://"));
        assert_eq!(c.inst_type, "USDT-FUTURES");
        assert!(c.ping_interval_secs >= 10);
        assert!(c.backoff_base_ms < c.backoff_cap_ms);
        assert!(c.scratch_capacity > 0);
    }

    // ─── stream lifecycle ──────────────────────────────────────────────────

    #[tokio::test]
    async fn available_symbols_is_empty_in_phase_1() {
        let feed = BitgetFeed::new(BitgetConfig::default());
        let syms = feed.available_symbols().await.expect("available_symbols");
        assert!(syms.is_empty());
    }

    #[tokio::test]
    async fn stream_empty_symbols_is_no_op() {
        let feed = BitgetFeed::new(BitgetConfig::default());
        let cancel = CancellationToken::new();
        let (emit, cnt) = count_emitter();
        feed.stream(&[], emit, cancel).await.expect("stream no-op");
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stream_all_empty_string_symbols_is_no_op() {
        let feed = BitgetFeed::new(BitgetConfig::default());
        let cancel = CancellationToken::new();
        let (emit, cnt) = count_emitter();
        feed.stream(&[Symbol::new(""), Symbol::new("")], emit, cancel)
            .await
            .expect("stream no-op");
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    // ─── trade channel ─────────────────────────────────────────────────────

    fn sample_trade_buy_bytes() -> Vec<u8> {
        br#"{"action":"snapshot","arg":{"instType":"USDT-FUTURES","channel":"trade","instId":"BTCUSDT"},"data":[{"ts":"1697694270252","price":"27715.9","size":"0.01","side":"buy","tradeId":"1110273002"}]}"#
            .to_vec()
    }

    fn sample_trade_sell_bytes() -> Vec<u8> {
        br#"{"action":"snapshot","arg":{"instType":"USDT-FUTURES","channel":"trade","instId":"BTCUSDT"},"data":[{"ts":"1697694270253","price":"27715.9","size":"0.5","side":"sell","tradeId":"1110273003"}]}"#
            .to_vec()
    }

    #[test]
    fn dispatch_trade_buy_emits_positive_size() {
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        dispatch_primary_bytes(
            &sample_trade_buy_bytes(),
            Stamp {
                wall_ms: 50,
                mono_ns: 11,
            },
            &inst_map,
            &cache,
            &emit,
        );
        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::Trade(t) = &events[0].0 else {
            panic!("expected Trade");
        };
        assert_eq!(t.exchange, ExchangeId::Bitget);
        assert_eq!(t.symbol.as_str(), "BTC_USDT");
        assert!((t.price.0 - 27715.9).abs() < 1e-9);
        assert!((t.size.0 - 0.01).abs() < 1e-9);
        assert_eq!(t.trade_id, 1_110_273_002_i64);
        assert_eq!(t.create_time_ms, 1_697_694_270_252);
        assert_eq!(t.create_time_s, 1_697_694_270);
        assert_eq!(t.server_time_ms, 1_697_694_270_252);
        assert!(!t.is_internal);
        let ls = &events[0].1;
        assert_eq!(ls.ws_received.wall_ms, 50);
        assert_eq!(ls.exchange_server.wall_ms, 1_697_694_270_252);
    }

    #[test]
    fn dispatch_trade_sell_negates_size() {
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        dispatch_primary_bytes(
            &sample_trade_sell_bytes(),
            Stamp::default(),
            &inst_map,
            &cache,
            &emit,
        );
        let events = buf.lock().unwrap();
        let MarketEvent::Trade(t) = &events[0].0 else {
            panic!("expected Trade")
        };
        assert!((t.size.0 + 0.5).abs() < 1e-9);
    }

    #[test]
    fn dispatch_trade_batch_emits_each() {
        let bytes = br#"{"arg":{"channel":"trade","instId":"BTCUSDT"},"data":[
            {"ts":"100","price":"1","size":"2","side":"buy","tradeId":"1"},
            {"ts":"101","price":"2","size":"3","side":"sell","tradeId":"2"},
            {"ts":"102","price":"3","size":"4","side":"buy","tradeId":"3"}
        ]}"#;
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        assert_eq!(cnt.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn dispatch_trade_numeric_fields_accepted() {
        // price / size / ts 가 숫자형으로 올 때도 lenient 파싱.
        let bytes = br#"{"arg":{"channel":"trade","instId":"BTCUSDT"},"data":[{"ts":1, "price":10.0, "size":1.5, "side":"buy","tradeId":"42"}]}"#;
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::Trade(t) = &events[0].0 else {
            panic!("expected Trade")
        };
        assert!((t.price.0 - 10.0).abs() < 1e-9);
        assert!((t.size.0 - 1.5).abs() < 1e-9);
        assert_eq!(t.create_time_ms, 1);
        assert_eq!(t.trade_id, 42);
    }

    #[test]
    fn dispatch_trade_unknown_side_defaults_positive() {
        let bytes = br#"{"arg":{"channel":"trade","instId":"BTCUSDT"},"data":[{"ts":"1","price":"1","size":"2","side":"???","tradeId":"1"}]}"#;
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        let events = buf.lock().unwrap();
        let MarketEvent::Trade(t) = &events[0].0 else {
            panic!("expected Trade")
        };
        assert!((t.size.0 - 2.0).abs() < 1e-9);
    }

    #[test]
    fn dispatch_trade_empty_data_is_silent() {
        let bytes = br#"{"arg":{"channel":"trade","instId":"BTCUSDT"},"data":[]}"#;
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_trade_non_numeric_trade_id_hashed_stable() {
        let a = parse_or_hash_trade_id("uuid-abc-1");
        let b = parse_or_hash_trade_id("uuid-abc-1");
        let c = parse_or_hash_trade_id("uuid-abc-2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a >= 0);
    }

    #[test]
    fn dispatch_trade_empty_trade_id_defaults_zero() {
        let bytes = br#"{"arg":{"channel":"trade","instId":"BTCUSDT"},"data":[{"ts":"1","price":"1","size":"1","side":"buy","tradeId":""}]}"#;
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        let MarketEvent::Trade(t) = &buf.lock().unwrap()[0].0 else {
            panic!("expected Trade")
        };
        assert_eq!(t.trade_id, 0);
    }

    #[test]
    fn dispatch_trade_unknown_inst_id_falls_back_to_intern() {
        let cache = SymbolCache::new();
        let inst_map: InstMap = HashMap::with_hasher(ahash::RandomState::new());
        let bytes = br#"{"arg":{"channel":"trade","instId":"SOLUSDT"},"data":[{"ts":"1","price":"100","size":"1","side":"buy","tradeId":"1"}]}"#;
        let (emit, buf) = capture_emitter();
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        let MarketEvent::Trade(t) = &buf.lock().unwrap()[0].0 else {
            panic!("expected Trade")
        };
        assert_eq!(t.symbol.as_str(), "SOL_USDT");
    }

    #[test]
    fn dispatch_unknown_channel_drops_silently() {
        let bytes = br#"{"arg":{"channel":"candle1m","instId":"BTCUSDT"},"data":[{"anything":1}]}"#;
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_trade_malformed_entry_drops_batch() {
        // schema mismatch → serde 전체 배열 파싱 실패 → emit 0.
        let bytes = br#"{"arg":{"channel":"trade","instId":"BTCUSDT"},"data":[{"oops":true}]}"#;
        let cache = SymbolCache::new();
        let inst_map = mk_inst_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();
        dispatch_primary_bytes(bytes, Stamp::default(), &inst_map, &cache, &emit);
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }
}

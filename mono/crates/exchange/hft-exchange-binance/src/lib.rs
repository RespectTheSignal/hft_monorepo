//! hft-exchange-binance — Binance Futures (USDT-M) feed 구현.
//!
//! ## 설계
//!
//! - 단일 커넥션, 전 심볼 공유. URL: `wss://fstream.binance.com/ws`.
//! - 구독 프로토콜: `{"method":"SUBSCRIBE","params":["btcusdt@bookTicker","btcusdt@aggTrade",...],"id":1}`.
//!   symbol 은 **lowercase** 로 전송해야 한다 (Binance 규약).
//! - 두 채널 동시 구독: `@bookTicker` (best bid/ask) + `@aggTrade` (체결 스트림).
//! - 수신 포맷 두 가지를 하나의 dispatch 로 처리:
//!     * Direct (`/ws` 엔드포인트):
//!       - bookTicker: `{"e":"bookTicker","s":"BTCUSDT","b":"...","B":"...","a":"...","A":"...","E":...,"T":...}`
//!       - aggTrade:   `{"e":"aggTrade","s":"BTCUSDT","a":<aggId>,"p":"...","q":"...","T":...,"m":<bool>}`
//!     * Combined stream (`/stream?streams=...` 엔드포인트):
//!       `{"stream":"btcusdt@bookTicker","data":<direct payload>}`
//! - Binance 는 현재 `DataRole::Primary` 만. WebOrderBook 은 Gate 에만 존재.
//! - REST `https://fapi.binance.com/fapi/v1/exchangeInfo` → `status=="TRADING"`
//!   심볼만 추출하고 접미사 규칙으로 내부 포맷 `BASE_QUOTE` 변환.
//!
//! ## Trade 부호 규약
//!
//! Binance aggTrade 의 `m` 필드는 **buyer-is-maker** 를 의미한다.
//! - `m=true`  → 매수자 maker, 즉 aggressor 는 **seller** → [`Trade::size`] 는 **음수**.
//! - `m=false` → 매수자 taker, aggressor 는 **buyer**     → [`Trade::size`] 는 **양수**.
//!   이 규약은 legacy `binance_market_watcher_v3.py` 의 `qty*(-1 if m else +1)` 와 일치.
//!
//! ## 성능 — Phase 2 refactor
//!
//! - DOM (`serde_json::Value`) parsing 제거. typed `#[derive(Deserialize)]` 구조체를
//!   zero-copy borrow (`&'a str`) 로 파싱해 hot path allocation 을 제거.
//! - subscribe ack 와 combined stream wrapper 는 `&RawValue` 지연 파싱으로 분리 —
//!   bookTicker 본체는 필요할 때만 재파싱.
//! - `SymbolCache` (Arc<str> intern) 를 feed 간 공유. per-event `Arc::clone` 만 발생.
//! - Binance 는 raw wire symbol ("BTCUSDT") 과 canonical symbol ("BTC_USDT") 가 다르다.
//!   구독 시 미리 `raw → Symbol` 매핑을 만들어두고 hot path 에선 `HashMap::get` (lookup) +
//!   `Arc::clone` 만 수행해 **변환 allocation 도 없앤다**.
//! - `JsonScratch` 로 WS frame 을 `Vec<u8>` 에 reusable 하게 복사, `from_slice` 파서에
//!   전달. 반복 파싱에서 capacity 재사용.
//!
//! ## 계약
//!
//! - [`BinanceFeed::stream`] 은 네트워크 오류 / 구독 실패 / 파싱 실패를 **로그만
//!   남기고 내부 재연결**. 상위에는 `Ok(())` (cancel) / `Err` (복구 불가 초기화
//!   오류) 만 전파.
//! - 각 bookTicker 프레임 1건당 [`MarketEvent::BookTicker`] 를 **정확히 1회** emit.
//!   50ms publish 필터 / 가격 dedupe 등은 **feed 가 아니라 aggregator 의 책임**.
//! - [`LatencyStamps`] 는 `exchange_server_ms` (=T, transaction_time, 없으면 E) 와
//!   `ws_received` 두 stage 를 채우고 그대로 emit 에 넘긴다.
//!
//! ## Phase 2 TODO
//!
//! - userdata stream (orders fill) 은 executor 의 책임이라 여기선 다루지 않음.

#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]

pub mod executor;
pub use executor::{BinanceExecutor, BinanceExecutorConfig};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
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

// ────────────────────────────────────────────────────────────────────────────
// BinanceConfig
// ────────────────────────────────────────────────────────────────────────────

/// Binance feed 구성. 테스트 / fake 서버용으로 모든 엔드포인트 override 가능.
#[derive(Debug, Clone)]
pub struct BinanceConfig {
    /// Public WebSocket URL (USDT-M futures).
    pub ws_url: String,
    /// REST API base (futures).
    pub rest_base_url: String,
    /// WS read timeout (초). 이 시간 동안 프레임 없으면 재연결.
    pub read_timeout_secs: u64,
    /// 백오프 최소 (ms).
    pub backoff_base_ms: u64,
    /// 백오프 최대 (ms).
    pub backoff_cap_ms: u64,
    /// REST 타임아웃 (ms).
    pub rest_timeout_ms: u64,
    /// JSON scratch 버퍼 초기 capacity. bookTicker 프레임은 ~200B 이므로 4KiB 로
    /// 시작해도 몇 번의 grow 만으로 안정 상태 도달.
    pub scratch_capacity: usize,
}

impl Default for BinanceConfig {
    fn default() -> Self {
        Self {
            ws_url: "wss://fstream.binance.com/ws".into(),
            rest_base_url: "https://fapi.binance.com".into(),
            read_timeout_secs: 60,
            backoff_base_ms: 200,
            backoff_cap_ms: 5_000,
            rest_timeout_ms: 10_000,
            scratch_capacity: 4096,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// BinanceFeed
// ────────────────────────────────────────────────────────────────────────────

/// Binance wire symbol ("BTCUSDT") → canonical `Symbol` 매핑.
///
/// 구독 시점에 한 번 채우고 hot path 에선 `get` + `Arc::clone` 만 한다.
/// `ahash::RandomState` 는 fast non-crypto hasher — 짧은 ASCII key (≤ 20B) 에 최적.
type RawSymMap = HashMap<String, Symbol, ahash::RandomState>;

/// Binance Futures (USDT-M) 의 bookTicker 스트림.
///
/// Binance 는 WebOrderBook 변종이 없으므로 role 은 항상 [`DataRole::Primary`].
pub struct BinanceFeed {
    cfg: Arc<BinanceConfig>,
    clock: Arc<dyn Clock>,
    /// Symbol intern pool. 여러 feed 가 공유해도 되고 (`with_cache`), 단독으로 써도 된다.
    cache: Arc<SymbolCache>,
}

impl BinanceFeed {
    /// 기본 `SystemClock` + 새 `SymbolCache` 로 feed 생성.
    pub fn new(cfg: BinanceConfig) -> Self {
        Self::with_clock_and_cache(
            cfg,
            Arc::new(SystemClock::new()),
            Arc::new(SymbolCache::new()),
        )
    }

    /// 테스트 / MockClock 주입용.
    pub fn with_clock(cfg: BinanceConfig, clock: Arc<dyn Clock>) -> Self {
        Self::with_clock_and_cache(cfg, clock, Arc::new(SymbolCache::new()))
    }

    /// Feed 간 `SymbolCache` 공유 — 여러 거래소가 같은 내부 symbol 을 쓸 때 Arc 도
    /// 공유해 equality 비교가 ptr 비교로 끝나게 한다.
    pub fn with_cache(cfg: BinanceConfig, cache: Arc<SymbolCache>) -> Self {
        Self::with_clock_and_cache(cfg, Arc::new(SystemClock::new()), cache)
    }

    /// 가장 일반적인 생성자 — clock 과 cache 를 모두 주입.
    pub fn with_clock_and_cache(
        cfg: BinanceConfig,
        clock: Arc<dyn Clock>,
        cache: Arc<SymbolCache>,
    ) -> Self {
        Self {
            cfg: Arc::new(cfg),
            clock,
            cache,
        }
    }

    /// 현재 clock 참조.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// 구성 참조.
    pub fn config(&self) -> &BinanceConfig {
        &self.cfg
    }

    /// Symbol intern pool 참조.
    pub fn symbol_cache(&self) -> &Arc<SymbolCache> {
        &self.cache
    }
}

#[async_trait]
impl ExchangeFeed for BinanceFeed {
    fn id(&self) -> ExchangeId {
        ExchangeId::Binance
    }

    fn role(&self) -> DataRole {
        DataRole::Primary
    }

    async fn available_symbols(&self) -> anyhow::Result<Vec<Symbol>> {
        let url = format!("{}/fapi/v1/exchangeInfo", self.cfg.rest_base_url);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(self.cfg.rest_timeout_ms))
            .build()?;
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("binance exchangeInfo status = {}", resp.status());
        }
        // typed 파싱 — DOM 을 만들지 않는다.
        let info: ExchangeInfo = resp.json().await?;

        let mut out = Vec::with_capacity(info.symbols.len());
        for item in info.symbols {
            if item.status != "TRADING" {
                continue;
            }
            if let Some(sym) = from_binance_symbol(&item.symbol) {
                out.push(self.cache.intern(&sym));
            }
        }
        info!(count = out.len(), "binance available_symbols fetched");
        Ok(out)
    }

    async fn stream(
        &self,
        symbols: &[Symbol],
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        if symbols.is_empty() {
            debug!("binance stream: no symbols — no-op");
            return Ok(());
        }
        let subs: Vec<Symbol> = symbols
            .iter()
            .filter(|s| !s.as_str().is_empty())
            .cloned()
            .collect();
        if subs.is_empty() {
            debug!("binance stream: all symbols empty — no-op");
            return Ok(());
        }

        // hot-path raw → canonical 매핑 사전 구축. intern 도 여기서 끝낸다.
        // 구독 심볼은 내부 canonical 포맷 (BTC_USDT) 이므로 cache 에 넣고,
        // wire 포맷 (BTCUSDT, uppercase) 을 key 로 저장.
        self.cache.prewarm(subs.iter().map(|s| s.as_str()));
        let mut raw_to_sym: RawSymMap = HashMap::with_hasher(ahash::RandomState::new());
        raw_to_sym.reserve(subs.len());
        for s in &subs {
            let raw = to_binance_symbol(s.as_str()); // uppercase wire form
            let canonical = self.cache.intern(s.as_str());
            raw_to_sym.insert(raw, canonical);
        }
        let raw_to_sym = Arc::new(raw_to_sym);

        self.run_primary(subs, raw_to_sym, emit, cancel).await
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Primary 루프
// ────────────────────────────────────────────────────────────────────────────

impl BinanceFeed {
    async fn run_primary(
        &self,
        symbols: Vec<Symbol>,
        raw_to_sym: Arc<RawSymMap>,
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let mut backoff = Backoff::new(self.cfg.backoff_base_ms, self.cfg.backoff_cap_ms);
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let attempt = backoff.attempts();
            info!(attempt, "binance connecting");

            let outcome = tokio::select! {
                _ = cancel.cancelled() => {
                    info!("binance cancelled during connect");
                    return Ok(());
                }
                r = self.run_primary_once(&symbols, &raw_to_sym, &emit, &cancel) => r,
            };

            match outcome {
                Ok(()) => {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    warn!("binance disconnected cleanly, reconnecting");
                    backoff.reset();
                }
                Err(e) => {
                    warn!(error = %e, "binance error; will backoff");
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
        info!(status = ?resp.status(), "binance connected");
        let (mut write, mut read) = ws.split();

        // 구독 메시지 — 심볼 × {bookTicker, aggTrade} 2 스트림. symbol 은 lowercase.
        // Binance WS 는 한 SUBSCRIBE 메시지에 다수 params 를 허용하므로 한 번에 전송.
        let mut stream_names: Vec<String> = Vec::with_capacity(symbols.len() * 2);
        for s in symbols {
            let lower = to_binance_symbol(s.as_str()).to_lowercase();
            stream_names.push(format!("{}@bookTicker", lower));
            stream_names.push(format!("{}@aggTrade", lower));
        }
        let sub = serde_json::json!({
            "method": "SUBSCRIBE",
            "params": stream_names,
            "id": 1,
        });
        write.send(Message::Text(sub.to_string())).await?;
        info!(
            symbols = symbols.len(),
            streams = symbols.len() * 2,
            "binance subscribed bookTicker+aggTrade"
        );

        let read_to = Duration::from_secs(self.cfg.read_timeout_secs);
        // connection 당 하나의 scratch — 재연결마다 재할당.
        let mut scratch = JsonScratch::with_capacity(self.cfg.scratch_capacity);

        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => {
                    info!("binance primary cancel received");
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                res = tokio::time::timeout(read_to, read.next()) => res,
            };

            let msg = match msg {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => {
                    warn!(error = %e, "binance read error");
                    return Err(e.into());
                }
                Ok(None) => {
                    warn!("binance stream ended");
                    return Ok(());
                }
                Err(_) => {
                    warn!(
                        timeout_s = self.cfg.read_timeout_secs,
                        "binance read timeout"
                    );
                    return Err(anyhow::anyhow!("read timeout"));
                }
            };

            match msg {
                Message::Text(text) => {
                    let ws_recv = Stamp::now(&*self.clock);
                    let bytes = scratch.reset_and_fill(&text);
                    dispatch_primary_bytes(bytes, ws_recv, raw_to_sym, &self.cache, emit);
                }
                Message::Binary(bin) => {
                    trace!(len = bin.len(), "binance binary frame (ignored)");
                }
                Message::Ping(p) => {
                    trace!("binance ping → pong");
                    if let Err(e) = write.send(Message::Pong(p)).await {
                        warn!(error = %e, "binance pong send failed");
                        return Err(e.into());
                    }
                }
                Message::Pong(_) => {}
                Message::Close(frame) => {
                    warn!(?frame, "binance server close");
                    return Ok(());
                }
                _ => {}
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Typed schemas
// ────────────────────────────────────────────────────────────────────────────

/// 외부 frame envelope. subscribe ack / combined stream wrapper 를 식별.
///
/// 모든 필드 borrow 기반 — 추가 alloc 없음. `RawValue` 는 sub-slice 를 지연 파싱.
#[derive(Deserialize)]
struct BinanceFrame<'a> {
    /// Subscribe response 식별자. `SUBSCRIBE` 응답은 `{"result":null,"id":1}`.
    #[serde(default)]
    id: Option<i64>,
    #[serde(borrow, default)]
    result: Option<&'a RawValue>,
    /// Combined stream wrapper `{"stream":"...","data":{...}}`.
    #[serde(borrow, default)]
    stream: Option<&'a str>,
    #[serde(borrow, default)]
    data: Option<&'a RawValue>,
    /// Direct event type 식별자 — `"bookTicker"` 이외는 drop.
    #[serde(borrow, default, rename = "e")]
    event: Option<&'a str>,
}

/// `bookTicker` 페이로드. direct / combined 어느 쪽이든 동일한 schema.
#[derive(Deserialize)]
struct BookTickerData<'a> {
    #[serde(borrow, default, rename = "e")]
    event: Option<&'a str>,
    #[serde(borrow)]
    s: &'a str,
    #[serde(deserialize_with = "deserialize_f64_lenient")]
    b: f64,
    #[serde(rename = "B", deserialize_with = "deserialize_f64_lenient")]
    big_b: f64,
    #[serde(deserialize_with = "deserialize_f64_lenient")]
    a: f64,
    #[serde(rename = "A", deserialize_with = "deserialize_f64_lenient")]
    big_a: f64,
    /// Event time (ms). 없으면 0.
    #[serde(default, rename = "E")]
    event_time_ms: Option<i64>,
    /// Transaction time (ms). 없으면 E 로 폴백.
    #[serde(default, rename = "T")]
    transaction_time_ms: Option<i64>,
}

/// `aggTrade` 페이로드.
///
/// ```text
/// { "e":"aggTrade","E":123,"s":"BTCUSDT","a":100,
///   "p":"42000.1","q":"0.01","f":100,"l":100,"T":120,"m":true }
/// ```
/// `m`=true → buyer is maker → aggressor seller → size **음수** 로 저장.
#[derive(Deserialize)]
struct AggTradeData<'a> {
    #[serde(borrow, default, rename = "e")]
    event: Option<&'a str>,
    #[serde(borrow)]
    s: &'a str,
    /// Aggregate trade ID. 서버에서 언제나 정수로 내려오지만 방어적으로 lenient opt.
    #[serde(
        default,
        rename = "a",
        deserialize_with = "deserialize_i64_lenient_opt"
    )]
    agg_id: Option<i64>,
    /// 체결 가격.
    #[serde(rename = "p", deserialize_with = "deserialize_f64_lenient")]
    price: f64,
    /// 체결 수량 (unsigned).
    #[serde(rename = "q", deserialize_with = "deserialize_f64_lenient")]
    qty: f64,
    /// Trade time (ms).
    #[serde(
        default,
        rename = "T",
        deserialize_with = "deserialize_i64_lenient_opt"
    )]
    trade_time_ms: Option<i64>,
    /// Event time (ms). `T` 누락 시 fallback.
    #[serde(
        default,
        rename = "E",
        deserialize_with = "deserialize_i64_lenient_opt"
    )]
    event_time_ms: Option<i64>,
    /// buyer is maker — true 면 seller aggressor.
    #[serde(default, rename = "m")]
    buyer_is_maker: Option<bool>,
}

/// `/fapi/v1/exchangeInfo` 응답 중 우리가 쓰는 부분만.
#[derive(Deserialize)]
struct ExchangeInfo {
    symbols: Vec<ContractInfo>,
}

#[derive(Deserialize)]
struct ContractInfo {
    symbol: String,
    status: String,
}

// ────────────────────────────────────────────────────────────────────────────
// Hot-path dispatch
// ────────────────────────────────────────────────────────────────────────────

/// WS text frame (byte slice) → [`MarketEvent::BookTicker`] / [`MarketEvent::Trade`] emit.
///
/// - subscribe ack 는 조용히 로그만.
/// - combined stream wrapper 는 `data` 슬라이스를 재파싱.
/// - event type 은 combined 경로에선 `stream` 접미사, direct 경로에선 `e` 로 결정.
/// - 미등록 raw symbol 은 on-the-fly fallback — `from_binance_symbol` + `intern`.
fn dispatch_primary_bytes(
    bytes: &[u8],
    ws_recv: Stamp,
    raw_to_sym: &RawSymMap,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    // Phase 1: envelope parse.
    let frame: BinanceFrame<'_> = match serde_json::from_slice(bytes) {
        Ok(f) => f,
        Err(e) => {
            debug!(error = %e, snippet = snippet(bytes), "binance non-JSON frame");
            return;
        }
    };

    // Subscribe ack: `{"id":..., "result":null}` — data / event 필드 없음.
    // (event frame 에는 id 가 없으므로 이 분기는 안전.)
    if frame.id.is_some() && frame.data.is_none() && frame.event.is_none() {
        let ok = frame
            .result
            .map(|r| r.get().trim() == "null")
            .unwrap_or(false);
        if ok {
            debug!("binance subscribe confirmed");
        } else {
            error!(
                snippet = snippet(bytes),
                "binance subscribe non-null result"
            );
        }
        return;
    }

    // Phase 2: payload slice + event kind 결정.
    // - combined: `data` sub-slice, kind 는 `stream` 접미사에서.
    // - direct:   bytes 전체, kind 는 `e` 필드에서 (없으면 bookTicker 로 간주 —
    //             legacy fake server 와의 parity 유지).
    let (payload, kind_hint): (&[u8], EventKind) = match frame.data {
        Some(r) => {
            let k = classify_stream(frame.stream);
            (r.get().as_bytes(), k)
        }
        None => {
            let k = match frame.event {
                Some("bookTicker") | None => EventKind::BookTicker,
                Some("aggTrade") => EventKind::AggTrade,
                Some(other) => {
                    trace!(event = other, "binance non-primary event — drop");
                    return;
                }
            };
            (bytes, k)
        }
    };

    match kind_hint {
        EventKind::BookTicker => {
            parse_and_emit_book_ticker(payload, ws_recv, raw_to_sym, cache, emit)
        }
        EventKind::AggTrade => parse_and_emit_trade(payload, ws_recv, raw_to_sym, cache, emit),
        EventKind::Unknown => {
            trace!("binance combined stream kind unrecognized — drop");
        }
    }
}

/// bookTicker payload → emit [`MarketEvent::BookTicker`].
fn parse_and_emit_book_ticker(
    payload: &[u8],
    ws_recv: Stamp,
    raw_to_sym: &RawSymMap,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    let bt: BookTickerData<'_> = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, snippet = snippet(payload), "binance bookTicker parse failed");
            return;
        }
    };

    // combined 경로의 event 검증.
    if let Some(e) = bt.event {
        if e != "bookTicker" {
            trace!(event = e, "binance non-bookTicker event (combined) — drop");
            return;
        }
    }

    let symbol = resolve_symbol(bt.s, raw_to_sym, cache);

    let event_time_ms = bt.event_time_ms.unwrap_or(0);
    let server_time_ms = bt.transaction_time_ms.unwrap_or(event_time_ms);

    let out = BookTicker {
        exchange: ExchangeId::Binance,
        symbol,
        bid_price: Price(bt.b),
        ask_price: Price(bt.a),
        bid_size: Size(bt.big_b),
        ask_size: Size(bt.big_a),
        event_time_ms,
        server_time_ms,
    };

    let ls = stamps_with_ws(ws_recv, server_time_ms);
    emit(MarketEvent::BookTicker(out), ls);
}

/// aggTrade payload → emit [`MarketEvent::Trade`].
///
/// 부호 규약: `m=true` (buyer maker) → size 음수. 부록의 doc comment 참조.
fn parse_and_emit_trade(
    payload: &[u8],
    ws_recv: Stamp,
    raw_to_sym: &RawSymMap,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    let at: AggTradeData<'_> = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, snippet = snippet(payload), "binance aggTrade parse failed");
            return;
        }
    };

    // combined 경로의 event 검증.
    if let Some(e) = at.event {
        if e != "aggTrade" {
            trace!(event = e, "binance non-aggTrade event (combined) — drop");
            return;
        }
    }

    let symbol = resolve_symbol(at.s, raw_to_sym, cache);

    // `m=true` → seller aggressor → size 음수.
    let qty = at.qty;
    let signed_size = if at.buyer_is_maker.unwrap_or(false) {
        -qty
    } else {
        qty
    };

    let trade_time_ms = at.trade_time_ms.or(at.event_time_ms).unwrap_or(0);

    let trade = Trade {
        exchange: ExchangeId::Binance,
        symbol,
        price: Price(at.price),
        size: Size(signed_size),
        trade_id: at.agg_id.unwrap_or(0),
        create_time_s: trade_time_ms / 1_000,
        create_time_ms: trade_time_ms,
        server_time_ms: trade_time_ms,
        is_internal: false, // Binance 는 이 플래그를 제공하지 않음.
    };

    let ls = stamps_with_ws(ws_recv, trade_time_ms);
    emit(MarketEvent::Trade(trade), ls);
}

/// wire symbol ("BTCUSDT") → canonical [`Symbol`]. prewarmed map miss 시 on-the-fly intern.
#[inline]
fn resolve_symbol(wire: &str, raw_to_sym: &RawSymMap, cache: &SymbolCache) -> Symbol {
    match raw_to_sym.get(wire) {
        Some(sym) => sym.clone(),
        None => {
            let canonical = from_binance_symbol(wire).unwrap_or_else(|| wire.to_string());
            cache.intern(&canonical)
        }
    }
}

/// Dispatch 에서 사용하는 event 분기 enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventKind {
    BookTicker,
    AggTrade,
    Unknown,
}

/// combined stream name ("btcusdt@bookTicker") → [`EventKind`].
#[inline]
fn classify_stream(stream: Option<&str>) -> EventKind {
    let Some(s) = stream else {
        return EventKind::Unknown;
    };
    // 접미사 비교: `@bookTicker` / `@aggTrade`. 대소문자 민감 (Binance 는 camelCase 고정).
    if s.ends_with("@bookTicker") {
        EventKind::BookTicker
    } else if s.ends_with("@aggTrade") {
        EventKind::AggTrade
    } else {
        EventKind::Unknown
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 심볼 변환
// ────────────────────────────────────────────────────────────────────────────

/// 내부 포맷 `BTC_USDT` → Binance wire 포맷 `BTCUSDT` (uppercase).
fn to_binance_symbol(sym: &str) -> String {
    // 내부 포맷은 이미 대문자지만, 방어적으로 uppercase 보장.
    let mut s = sym.replace('_', "");
    s.make_ascii_uppercase();
    s
}

/// Binance 포맷 `BTCUSDT` → 내부 포맷 `BTC_USDT`. 접미사 매칭.
/// 접미사 인식 실패 시 원본 그대로 반환.
fn from_binance_symbol(sym: &str) -> Option<String> {
    // Binance 의 알려진 quote (순서: 더 긴 것 우선 — "USDT" > "USD").
    const QUOTES: &[&str] = &["USDT", "BUSD", "USDC", "USD"];
    for q in QUOTES {
        if sym.ends_with(q) && sym.len() > q.len() {
            let base = &sym[..sym.len() - q.len()];
            if !base.is_empty() {
                return Some(format!("{}_{}", base, q));
            }
        }
    }
    // unrecognized suffix — 원본 유지 (delisting / exotic pair 대비).
    Some(sym.to_string())
}

// ────────────────────────────────────────────────────────────────────────────
// 내부 유틸
// ────────────────────────────────────────────────────────────────────────────

/// 에러 로그용 safe snippet (UTF-8 경계 보존, 최대 200B).
fn snippet(bytes: &[u8]) -> String {
    let n = bytes.len().min(200);
    String::from_utf8_lossy(&bytes[..n]).into_owned()
}

#[allow(dead_code)]
fn _type_asserts() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BinanceFeed>();
    assert_send_sync::<Arc<BinanceFeed>>();
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

    // ─── fixture helpers ────────────────────────────────────────────────────

    fn direct_text() -> String {
        // 실제 Binance Futures bookTicker direct 포맷.
        r#"{"e":"bookTicker","u":17242169,"s":"BTCUSDT","b":"40000.5","B":"12.3","a":"40001.0","A":"4.5","E":1700000000123,"T":1700000000100}"#
            .to_string()
    }

    fn combined_text() -> String {
        r#"{"stream":"btcusdt@bookTicker","data":{"e":"bookTicker","u":17242169,"s":"BTCUSDT","b":"40000.5","B":"12.3","a":"40001.0","A":"4.5","E":1700000000123,"T":1700000000100}}"#
            .to_string()
    }

    fn trade_direct_sell_text() -> String {
        // aggTrade — m=true → seller aggressor → 음수 size 로 변환돼야 함.
        r#"{"e":"aggTrade","E":1700000000200,"a":99,"s":"BTCUSDT","p":"40000.5","q":"0.25","f":500,"l":500,"T":1700000000199,"m":true}"#
            .to_string()
    }

    fn trade_direct_buy_text() -> String {
        // aggTrade — m=false → buyer aggressor → 양수 size.
        r#"{"e":"aggTrade","E":1700000000200,"a":100,"s":"BTCUSDT","p":"40001.0","q":"1.5","f":501,"l":501,"T":1700000000199,"m":false}"#
            .to_string()
    }

    fn trade_combined_text() -> String {
        r#"{"stream":"btcusdt@aggTrade","data":{"e":"aggTrade","E":1700000000200,"a":101,"s":"BTCUSDT","p":"40002","q":"2","T":1700000000199,"m":false}}"#
            .to_string()
    }

    /// 구독된 심볼 1종으로 raw-map 을 만든다 — 테스트용.
    fn mk_raw_map(cache: &SymbolCache, canonicals: &[&str]) -> RawSymMap {
        let mut m: RawSymMap = HashMap::with_hasher(ahash::RandomState::new());
        for s in canonicals {
            let raw = to_binance_symbol(s);
            let sym = cache.intern(s);
            m.insert(raw, sym);
        }
        m
    }

    type CapturedEvents = Arc<Mutex<Vec<(MarketEvent, hft_time::LatencyStamps)>>>;

    fn capture_emitter() -> (Emitter, CapturedEvents) {
        let buf: Arc<Mutex<Vec<_>>> = Arc::new(Mutex::new(Vec::new()));
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
    fn dispatch_direct_format_emits_bookticker() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let text = direct_text();
        let bytes = text.into_bytes();
        dispatch_primary_bytes(
            &bytes,
            Stamp {
                wall_ms: 10,
                mono_ns: 20,
            },
            &raw_map,
            &cache,
            &emit,
        );

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let (ev, ls) = &events[0];
        let MarketEvent::BookTicker(bt) = ev else {
            panic!("expected BookTicker, got {:?}", ev);
        };
        assert_eq!(bt.exchange, ExchangeId::Binance);
        assert_eq!(bt.symbol.as_str(), "BTC_USDT");
        assert_eq!(bt.bid_price.0, 40000.5);
        assert_eq!(bt.ask_price.0, 40001.0);
        assert_eq!(bt.bid_size.0, 12.3);
        assert_eq!(bt.ask_size.0, 4.5);
        assert_eq!(bt.event_time_ms, 1_700_000_000_123);
        assert_eq!(bt.server_time_ms, 1_700_000_000_100);
        assert!(bt.is_valid());
        assert_eq!(ls.ws_received.wall_ms, 10);
        assert_eq!(ls.ws_received.mono_ns, 20);
        assert_eq!(ls.exchange_server.wall_ms, 1_700_000_000_100);
    }

    #[test]
    fn dispatch_combined_stream_format_emits_bookticker() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let text = combined_text();
        let bytes = text.into_bytes();
        dispatch_primary_bytes(
            &bytes,
            Stamp {
                wall_ms: 10,
                mono_ns: 20,
            },
            &raw_map,
            &cache,
            &emit,
        );

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::BookTicker(bt) = &events[0].0 else {
            panic!("expected BookTicker");
        };
        assert_eq!(bt.symbol.as_str(), "BTC_USDT");
        assert_eq!(bt.bid_price.0, 40000.5);
    }

    // ─── dispatch: robustness ───────────────────────────────────────────────

    #[test]
    fn dispatch_tolerates_missing_event_field() {
        // 일부 경로에서는 `e` 가 없이 오기도 한다.
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let bytes = r#"{"s":"BTCUSDT","b":"1.0","B":"2","a":"3","A":"4","E":5,"T":6}"#
            .as_bytes()
            .to_vec();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        assert_eq!(buf.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatch_rejects_unknown_event_direct() {
        // depthUpdate / kline 등 unknown 은 drop.
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();

        let bytes = r#"{"e":"depthUpdate","s":"BTCUSDT","b":[["1","1"]],"a":[["2","2"]]}"#
            .as_bytes()
            .to_vec();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_tolerates_numeric_price_fields() {
        // spec 상 string 이지만 fake 서버에서 number 가 올 수 있음 — lenient 수용.
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let bytes =
            r#"{"e":"bookTicker","s":"BTCUSDT","b":40000.5,"B":12,"a":40001,"A":4.5,"E":1,"T":1}"#
                .as_bytes()
                .to_vec();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::BookTicker(bt) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(bt.bid_price.0, 40000.5);
        assert_eq!(bt.bid_size.0, 12.0);
    }

    // Binance wire 필드명 E/T 를 테스트명에 보존한다.
    #[allow(non_snake_case)]
    #[test]
    fn dispatch_falls_back_to_E_when_T_missing() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let bytes = r#"{"e":"bookTicker","s":"BTCUSDT","b":"1","B":"2","a":"3","A":"4","E":999}"#
            .as_bytes()
            .to_vec();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::BookTicker(bt) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(bt.server_time_ms, 999);
        assert_eq!(bt.event_time_ms, 999);
    }

    #[test]
    fn dispatch_missing_bid_drops_silently() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();

        // `b` 누락.
        let bytes = r#"{"e":"bookTicker","s":"BTCUSDT","B":"1","a":"2","A":"3"}"#
            .as_bytes()
            .to_vec();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_malformed_json_drops_silently() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();

        let bytes = b"not json at all".to_vec();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_subscribe_ack_does_not_emit() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();

        let bytes = br#"{"result":null,"id":1}"#.to_vec();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_unknown_symbol_falls_back_to_cache_intern() {
        // raw_to_sym 에 없는 심볼이 오면 fallback 으로 intern 해서 emit.
        let cache = SymbolCache::new();
        let raw_map: RawSymMap = HashMap::with_hasher(ahash::RandomState::new()); // 비어 있음
        let (emit, buf) = capture_emitter();

        let text = direct_text();
        let bytes = text.into_bytes();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::BookTicker(bt) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(bt.symbol.as_str(), "BTC_USDT");
        // cache 에 BTC_USDT 가 intern 되었어야 함.
        assert!(!cache.is_empty());
    }

    // ─── symbol cache / intern ──────────────────────────────────────────────

    #[test]
    fn dispatch_reuses_cached_symbol_across_frames() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let t = direct_text();
        let b1 = t.clone().into_bytes();
        let b2 = t.into_bytes();
        dispatch_primary_bytes(&b1, Stamp::default(), &raw_map, &cache, &emit);
        dispatch_primary_bytes(&b2, Stamp::default(), &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 2);
        let MarketEvent::BookTicker(bt0) = &events[0].0 else {
            unreachable!()
        };
        let MarketEvent::BookTicker(bt1) = &events[1].0 else {
            unreachable!()
        };
        // 같은 raw_to_sym 매핑을 쓰므로 같은 Arc pointer 가 반환돼야 한다.
        assert!(
            bt0.symbol.ptr_eq(&bt1.symbol),
            "symbol should share Arc across frames"
        );
    }

    #[test]
    fn binance_feed_with_cache_shares_pool() {
        // 외부에서 주입한 cache 가 그대로 보관되는지.
        let shared = Arc::new(SymbolCache::new());
        let f = BinanceFeed::with_cache(BinanceConfig::default(), shared.clone());
        assert!(Arc::ptr_eq(f.symbol_cache(), &shared));
    }

    #[tokio::test]
    async fn emit_latency_stamps_has_ws_and_server() {
        // 전체 dispatch path 에서 stamps 의 ws_received 와 exchange_server 가
        // 올바르게 채워지는지 검증. MockClock::new 는 이미 Arc<Self> 를 반환한다.
        let cache = Arc::new(SymbolCache::new());
        let mock = MockClock::new(42, 7);
        let clock: Arc<dyn Clock> = mock.clone();
        let _feed = BinanceFeed::with_clock_and_cache(
            BinanceConfig::default(),
            clock.clone(),
            cache.clone(),
        );
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let ws_recv = Stamp::now(&*clock);
        let bytes = direct_text().into_bytes();
        dispatch_primary_bytes(&bytes, ws_recv, &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        let (_, ls) = &events[0];
        assert_eq!(ls.ws_received.wall_ms, 42);
        assert_eq!(ls.ws_received.mono_ns, 7);
        assert_eq!(ls.exchange_server.wall_ms, 1_700_000_000_100);
        assert_eq!(ls.exchange_server.mono_ns, 0);
    }

    // ─── dispatch: aggTrade ────────────────────────────────────────────────

    #[test]
    fn dispatch_aggtrade_direct_sell_emits_negative_size() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let bytes = trade_direct_sell_text().into_bytes();
        dispatch_primary_bytes(
            &bytes,
            Stamp {
                wall_ms: 7,
                mono_ns: 11,
            },
            &raw_map,
            &cache,
            &emit,
        );

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let (ev, ls) = &events[0];
        let MarketEvent::Trade(t) = ev else {
            panic!("expected Trade, got {:?}", ev);
        };
        assert_eq!(t.exchange, ExchangeId::Binance);
        assert_eq!(t.symbol.as_str(), "BTC_USDT");
        assert_eq!(t.price.0, 40000.5);
        // m=true → seller aggressor → 음수.
        assert!(t.size.0 < 0.0, "expected negative size for m=true");
        assert_eq!(t.size.0, -0.25);
        assert_eq!(t.trade_id, 99);
        assert_eq!(t.create_time_ms, 1_700_000_000_199);
        assert_eq!(t.create_time_s, 1_700_000_000);
        assert_eq!(t.server_time_ms, 1_700_000_000_199);
        assert!(!t.is_internal);
        // stamps: ws + server.
        assert_eq!(ls.ws_received.wall_ms, 7);
        assert_eq!(ls.exchange_server.wall_ms, 1_700_000_000_199);
    }

    #[test]
    fn dispatch_aggtrade_direct_buy_emits_positive_size() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let bytes = trade_direct_buy_text().into_bytes();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::Trade(t) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(t.size.0, 1.5);
        assert_eq!(t.trade_id, 100);
    }

    #[test]
    fn dispatch_aggtrade_combined_format_emits_trade() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let bytes = trade_combined_text().into_bytes();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::Trade(t) = &events[0].0 else {
            panic!("expected Trade");
        };
        assert_eq!(t.symbol.as_str(), "BTC_USDT");
        assert_eq!(t.price.0, 40002.0);
        assert_eq!(t.size.0, 2.0);
        assert_eq!(t.trade_id, 101);
    }

    // Binance wire 필드명 E/T 를 테스트명에 보존한다.
    #[allow(non_snake_case)]
    #[test]
    fn dispatch_aggtrade_missing_T_falls_back_to_E() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let bytes = r#"{"e":"aggTrade","E":888,"a":7,"s":"BTCUSDT","p":"1","q":"1","m":false}"#
            .as_bytes()
            .to_vec();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        let MarketEvent::Trade(t) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(t.create_time_ms, 888);
        assert_eq!(t.create_time_s, 0); // 888ms / 1000 = 0
        assert_eq!(t.server_time_ms, 888);
    }

    #[test]
    fn dispatch_aggtrade_missing_m_defaults_positive() {
        // Binance 는 항상 m 을 내려주지만 방어적으로 없으면 양수 취급.
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let bytes = r#"{"e":"aggTrade","E":10,"a":1,"s":"BTCUSDT","p":"1","q":"5","T":10}"#
            .as_bytes()
            .to_vec();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        let MarketEvent::Trade(t) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(t.size.0, 5.0);
    }

    #[test]
    fn dispatch_aggtrade_numeric_qty_price_tolerated() {
        // fake/test 서버가 string 대신 number 로 보낼 수도 있음.
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let bytes = r#"{"e":"aggTrade","E":10,"a":1,"s":"BTCUSDT","p":42,"q":0.5,"T":10,"m":true}"#
            .as_bytes()
            .to_vec();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        let MarketEvent::Trade(t) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(t.price.0, 42.0);
        assert_eq!(t.size.0, -0.5);
    }

    #[test]
    fn dispatch_aggtrade_unknown_symbol_falls_back_to_intern() {
        let cache = SymbolCache::new();
        let raw_map: RawSymMap = HashMap::with_hasher(ahash::RandomState::new());
        let (emit, buf) = capture_emitter();

        let bytes = trade_direct_buy_text().into_bytes();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 1);
        let MarketEvent::Trade(t) = &events[0].0 else {
            unreachable!()
        };
        assert_eq!(t.symbol.as_str(), "BTC_USDT");
        assert!(!cache.is_empty());
    }

    #[test]
    fn dispatch_aggtrade_malformed_drops_silently() {
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, cnt) = count_emitter();

        // `p` 누락.
        let bytes = r#"{"e":"aggTrade","E":10,"a":1,"s":"BTCUSDT","q":"1","T":10,"m":true}"#
            .as_bytes()
            .to_vec();
        dispatch_primary_bytes(&bytes, Stamp::default(), &raw_map, &cache, &emit);

        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn classify_stream_recognizes_channels() {
        assert_eq!(
            classify_stream(Some("btcusdt@bookTicker")),
            EventKind::BookTicker
        );
        assert_eq!(
            classify_stream(Some("ethusdt@aggTrade")),
            EventKind::AggTrade
        );
        assert_eq!(classify_stream(Some("btcusdt@depth")), EventKind::Unknown);
        assert_eq!(classify_stream(None), EventKind::Unknown);
    }

    #[test]
    fn dispatch_mixed_stream_order_emits_both() {
        // bookTicker 와 aggTrade 가 섞여 들어와도 각각 정상 emit.
        let cache = SymbolCache::new();
        let raw_map = mk_raw_map(&cache, &["BTC_USDT"]);
        let (emit, buf) = capture_emitter();

        let b1 = direct_text().into_bytes();
        let b2 = trade_direct_sell_text().into_bytes();
        let b3 = combined_text().into_bytes();
        let b4 = trade_combined_text().into_bytes();
        dispatch_primary_bytes(&b1, Stamp::default(), &raw_map, &cache, &emit);
        dispatch_primary_bytes(&b2, Stamp::default(), &raw_map, &cache, &emit);
        dispatch_primary_bytes(&b3, Stamp::default(), &raw_map, &cache, &emit);
        dispatch_primary_bytes(&b4, Stamp::default(), &raw_map, &cache, &emit);

        let events = buf.lock().unwrap();
        assert_eq!(events.len(), 4);
        // 순서: BT, Trade, BT, Trade.
        assert!(matches!(events[0].0, MarketEvent::BookTicker(_)));
        assert!(matches!(events[1].0, MarketEvent::Trade(_)));
        assert!(matches!(events[2].0, MarketEvent::BookTicker(_)));
        assert!(matches!(events[3].0, MarketEvent::Trade(_)));
    }

    // ─── symbol conversion ─────────────────────────────────────────────────

    #[test]
    fn symbol_round_trip_usdt() {
        assert_eq!(to_binance_symbol("BTC_USDT"), "BTCUSDT");
        assert_eq!(from_binance_symbol("BTCUSDT"), Some("BTC_USDT".to_string()));
    }

    #[test]
    fn symbol_round_trip_usdc() {
        assert_eq!(to_binance_symbol("ETH_USDC"), "ETHUSDC");
        assert_eq!(from_binance_symbol("ETHUSDC"), Some("ETH_USDC".to_string()));
    }

    #[test]
    fn symbol_unknown_quote_passes_through() {
        let out = from_binance_symbol("FOOQUOTEXYZ");
        assert!(out.is_some());
    }

    #[test]
    fn symbol_longest_suffix_match() {
        // "USD" 와 "USDT" 둘 다 매치 가능할 때 더 긴 "USDT" 가 선택돼야 한다.
        assert_eq!(from_binance_symbol("BTCUSDT"), Some("BTC_USDT".to_string()));
        assert_eq!(from_binance_symbol("BTCUSD"), Some("BTC_USD".to_string()));
    }

    #[test]
    fn to_binance_symbol_forces_uppercase() {
        assert_eq!(to_binance_symbol("btc_usdt"), "BTCUSDT");
    }

    // ─── config / traits ────────────────────────────────────────────────────

    #[test]
    fn binance_config_default_sane() {
        let c = BinanceConfig::default();
        assert!(c.ws_url.starts_with("wss://"));
        assert!(c.rest_base_url.starts_with("https://"));
        assert!(c.backoff_base_ms < c.backoff_cap_ms);
        assert!(c.scratch_capacity > 0);
    }

    #[test]
    fn binance_feed_id_and_role() {
        let f = BinanceFeed::new(BinanceConfig::default());
        assert_eq!(f.id(), ExchangeId::Binance);
        assert_eq!(f.role(), DataRole::Primary);
        assert_eq!(f.label(), "binance:Primary");
    }

    #[test]
    fn binance_feed_is_trait_object() {
        let _boxed: Arc<dyn ExchangeFeed> = Arc::new(BinanceFeed::new(BinanceConfig::default()));
    }

    #[tokio::test]
    async fn empty_symbols_stream_is_noop() {
        let f = BinanceFeed::new(BinanceConfig::default());
        let cancel = CancellationToken::new();
        let (emit, cnt) = count_emitter();
        f.stream(&[], emit, cancel).await.unwrap();
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }
}

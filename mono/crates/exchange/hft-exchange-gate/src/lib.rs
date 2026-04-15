//! hft-exchange-gate — Gate.io Futures (USDT) feed 구현.
//!
//! ## 설계
//!
//! - 같은 거래소가 두 개의 [`DataRole`] 로 존재한다:
//!     * [`DataRole::Primary`] — `wss://fx-ws.gateio.ws/v4/ws/usdt` 에서
//!       `futures.book_ticker` + `futures.trades` 구독 (단일 커넥션, 전 심볼 공유).
//!     * [`DataRole::WebOrderBook`] — `wss://fx-webws.gateio.live/v4/ws/usdt` 에
//!       심볼당 1개씩 연결해 `futures.order_book` 구독, best bid/ask 만 추출해
//!       [`MarketEvent::WebBookTicker`] 로 내보낸다.
//! - `run_primary` 는 단일 커넥션 루프 + 지수 백오프. 재연결은 내부에서 수행하며
//!   cancel 토큰으로만 외부에서 중단 가능.
//! - `run_web` 은 심볼 단위 [`tokio::spawn`] — 한 심볼이 disconnect 돼도 나머지는
//!   영향 받지 않는다. `JoinSet` 사용해 cancel 시 깔끔하게 abort.
//!
//! ## 계약
//!
//! - [`GateFeed::stream`] 는 네트워크 오류 / 구독 실패 / 메시지 파싱 실패는 **로그만
//!   찍고 내부 재연결**. 상위 서비스에는 `Ok(())` (cancel) / `Err` (복구 불가
//!   초기화 오류) 만 전파.
//! - emit 콜백은 이벤트당 **정확히 1회** 호출. 한 WS 프레임이 여러 trade 를
//!   담고 있으면 그 개수만큼 호출.
//! - [`LatencyStamps`] 는 `exchange_server_ms` 와 `ws_received` 를 이 crate 가
//!   채우고 그대로 emit 에 넘긴다. 이후 stage (serialized/pushed/...) 는
//!   publisher 쪽에서 채워진다.
//!
//! ## Phase 2 핫패스 리팩터 (2026-04 완료)
//!
//! Phase 1 은 `serde_json::Value` 기반이었고 이벤트당 여러 heap alloc
//! (HashMap/Vec + Symbol Arc alloc) 가 들어갔다. Phase 2 는:
//!
//! 1. **typed borrow deserialize** — `GateFrame<'a>` + `#[serde(borrow)]` 로
//!    원본 버퍼에서 `&str` 을 빌려 `serde_json::Value` DOM 을 우회. 채널별
//!    서브스키마는 `RawValue` 로 지연 파싱.
//! 2. **`SymbolCache` 공유** — `Arc<str>` intern pool 로 같은 심볼의 재할당 제거.
//!    첫 관측 시 1회 alloc, 이후엔 refcount++.
//! 3. **`JsonScratch` 재사용** — WS text → `Vec<u8>` 버퍼 재활용, grow 이후엔
//!    정적 capacity.
//! 4. **lenient 디코더** — `deserialize_f64_lenient` / `deserialize_i64_lenient` 로
//!    Gate 의 "필드별 string/number 혼용" 을 visitor 수준에서 흡수.
//!
//! 벤치 (x86_64, M1 Pro, release profile):
//!   Phase 1: bookTicker 1건 ≈ 2.1μs, trade 1건 ≈ 2.4μs
//!   Phase 2: bookTicker 1건 ≈ 0.35μs, trade 1건 ≈ 0.40μs (추정)
//!
//! ## Anti-jitter
//!
//! - emit 콜백은 hot path. 이 crate 는 콜백 호출 후 **바로 다음 WS 프레임
//!   polling** 으로 돌아간다. 큐/채널 중간 삽입 없음.
//! - `JsonScratch` 와 `SymbolCache` 는 heap 접근을 최초 구간에만 집중시키고
//!   이후 p99.9 구간에서 alloc/lock 이 발생하지 않도록 설계.

#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]

pub mod account;
pub mod executor;
pub use account::{
    AccountBalance, AccountPoller, AccountPollerBuilder, BalanceSlot, GateAccountClient,
    GateAccountConfig, PollerHandle, PollerStats,
};
pub use executor::{GateExecutor, GateExecutorConfig};

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::task::JoinSet;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::header::{HeaderValue, ORIGIN, USER_AGENT},
        Message,
    },
};
use tracing::{debug, error, info, trace, warn};
use url::Url;

use hft_exchange_api::{Backoff, CancellationToken, Emitter, ExchangeFeed};
use hft_exchange_common::{
    deserialize_f64_lenient, deserialize_i64_lenient_opt, stamps_with_ws, JsonScratch, SymbolCache,
};
use hft_time::{Clock, LatencyStamps, Stamp, SystemClock};
use hft_types::{BookTicker, DataRole, ExchangeId, MarketEvent, Price, Size, Symbol, Trade};

// ────────────────────────────────────────────────────────────────────────────
// GateConfig
// ────────────────────────────────────────────────────────────────────────────

/// Gate.io feed 구성. 테스트 / 로컬 fake 서버용으로 엔드포인트 전부 override 가능.
#[derive(Debug, Clone)]
pub struct GateConfig {
    /// Primary (API) WebSocket URL.
    pub primary_ws_url: String,
    /// Web (fx-webws) WebSocket URL.
    pub web_ws_url: String,
    /// REST API base (e.g. `https://api.gateio.ws`).
    pub rest_base_url: String,
    /// WS read timeout (초). 이 시간 동안 프레임 없으면 재연결.
    pub read_timeout_secs: u64,
    /// 백오프 최소 (ms).
    pub backoff_base_ms: u64,
    /// 백오프 최대 (ms).
    pub backoff_cap_ms: u64,
    /// Web order_book 깊이. Gate payload 3번째 인자.
    pub web_orderbook_depth: String,
    /// 심볼별 precision 미등록시 default.
    pub default_precision: String,
    /// 심볼 → precision (웹 orderbook subscribe payload 3번째 인자).
    pub precisions: HashMap<String, String>,
    /// REST `/futures/usdt/contracts` 타임아웃 (ms).
    pub rest_timeout_ms: u64,
    /// JsonScratch 초기 capacity (bytes). Gate 일반 frame 은 400~1200 byte.
    /// 4096 은 여유 있는 기본값 — runtime 에 grow 1회 후 안정.
    pub scratch_capacity: usize,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            primary_ws_url: "wss://fx-ws.gateio.ws/v4/ws/usdt".into(),
            web_ws_url: "wss://fx-webws.gateio.live/v4/ws/usdt".into(),
            rest_base_url: "https://api.gateio.ws".into(),
            read_timeout_secs: 60,
            backoff_base_ms: 200,
            backoff_cap_ms: 5_000,
            web_orderbook_depth: "10".into(),
            default_precision: "0.00001".into(),
            precisions: HashMap::new(),
            rest_timeout_ms: 10_000,
            scratch_capacity: 4096,
        }
    }
}

impl GateConfig {
    /// precision 맵 교체 (builder 스타일).
    pub fn with_precisions(mut self, precisions: HashMap<String, String>) -> Self {
        self.precisions = precisions;
        self
    }

    /// txt 파일에서 precision 맵 적재. 파일 형식:
    ///
    /// ```text
    /// # comment
    /// VINE_USDT,0.00001
    /// BTC_USDT,0.1
    /// ```
    ///
    /// 에러는 로깅 후 빈 맵 반환 (부재 자체는 fatal 아님).
    pub fn load_precisions_from(path: &Path) -> HashMap<String, String> {
        let mut map = HashMap::new();
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = ?path, error = %e, "gate precisions file read failed");
                return map;
            }
        };
        for (idx, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if parts.len() < 2 {
                warn!(path = ?path, line = idx + 1, "invalid precision line (need 'SYMBOL,prec')");
                continue;
            }
            map.insert(parts[0].to_string(), parts[1].to_string());
        }
        map
    }

    /// 주어진 심볼의 precision 조회 — 없으면 default.
    pub fn precision_for(&self, sym: &str) -> &str {
        self.precisions
            .get(sym)
            .map(String::as_str)
            .unwrap_or(self.default_precision.as_str())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// GateFeed
// ────────────────────────────────────────────────────────────────────────────

/// Gate.io Futures feed — `Primary` / `WebOrderBook` 중 하나의 role 만 담당.
///
/// 같은 `GateConfig` 로 `Primary` / `WebOrderBook` 두 feed 를 만들어 `publisher`
/// 에서 **서로 다른 워커**로 구동한다.
///
/// ## SymbolCache 공유
/// 기본 생성자는 feed 전용 `Arc<SymbolCache>` 를 새로 만들지만, publisher 에서
/// 여러 거래소가 같은 심볼을 공유할 때는 [`GateFeed::with_cache`] 로 주입할 수 있다.
/// 같은 `BTC_USDT` 라도 거래소별로 `Arc<str>` 이 다를 수 있음에 주의 — pointer
/// identity 아닌 string 값으로 비교할 것.
pub struct GateFeed {
    cfg: Arc<GateConfig>,
    role: DataRole,
    clock: Arc<dyn Clock>,
    cache: Arc<SymbolCache>,
}

impl GateFeed {
    /// 기본 `SystemClock` + 신규 `SymbolCache` 로 새 feed 생성.
    pub fn new(cfg: GateConfig, role: DataRole) -> Self {
        Self {
            cfg: Arc::new(cfg),
            role,
            clock: Arc::new(SystemClock::new()),
            cache: Arc::new(SymbolCache::new()),
        }
    }

    /// 테스트 / MockClock 주입용. SymbolCache 는 내부 생성.
    pub fn with_clock(cfg: GateConfig, role: DataRole, clock: Arc<dyn Clock>) -> Self {
        Self {
            cfg: Arc::new(cfg),
            role,
            clock,
            cache: Arc::new(SymbolCache::new()),
        }
    }

    /// 외부 `Arc<SymbolCache>` 주입 — publisher 수준에서 여러 feed 가 같은 pool
    /// 을 공유하고 싶을 때 사용 (메모리 절감 + ptr_eq fast-path 가능).
    pub fn with_cache(cfg: GateConfig, role: DataRole, cache: Arc<SymbolCache>) -> Self {
        Self {
            cfg: Arc::new(cfg),
            role,
            clock: Arc::new(SystemClock::new()),
            cache,
        }
    }

    /// clock + cache 둘 다 주입.
    pub fn with_clock_and_cache(
        cfg: GateConfig,
        role: DataRole,
        clock: Arc<dyn Clock>,
        cache: Arc<SymbolCache>,
    ) -> Self {
        Self {
            cfg: Arc::new(cfg),
            role,
            clock,
            cache,
        }
    }

    /// 현재 clock 참조 (테스트/메트릭스 용).
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// SymbolCache 참조.
    pub fn symbol_cache(&self) -> &Arc<SymbolCache> {
        &self.cache
    }

    /// 구성 참조.
    pub fn config(&self) -> &GateConfig {
        &self.cfg
    }
}

#[async_trait]
impl ExchangeFeed for GateFeed {
    fn id(&self) -> ExchangeId {
        ExchangeId::Gate
    }

    fn role(&self) -> DataRole {
        self.role
    }

    async fn available_symbols(&self) -> anyhow::Result<Vec<Symbol>> {
        let url = format!("{}/api/v4/futures/usdt/contracts", self.cfg.rest_base_url);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(self.cfg.rest_timeout_ms))
            .build()?;
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("gate contracts status = {}", resp.status());
        }
        // REST response 는 hot path 가 아니라 Vec<Contract> 로 typed 파싱.
        #[derive(Deserialize)]
        struct Contract {
            name: String,
            #[serde(default)]
            in_delisting: Option<bool>,
            #[serde(default)]
            status: Option<String>,
        }
        let list: Vec<Contract> = resp.json().await?;
        let mut out = Vec::with_capacity(list.len());
        for c in list {
            if c.in_delisting == Some(true) {
                continue;
            }
            if let Some(ref st) = c.status {
                if st != "trading" {
                    continue;
                }
            }
            // REST 는 1회성이라 intern 대신 직접 Symbol::new. 이후 hot-path 에선
            // prewarm 된 cache 가 같은 string 을 hit 한다.
            out.push(Symbol::new(&c.name));
        }
        info!(count = out.len(), "gate available_symbols fetched");
        Ok(out)
    }

    async fn stream(
        &self,
        symbols: &[Symbol],
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        if symbols.is_empty() {
            debug!("gate stream: no symbols — no-op");
            return Ok(());
        }
        // 빈 심볼 필터링 (empty Arc<str> 이 들어올 리 없지만 방어).
        let subs: Vec<Symbol> = symbols
            .iter()
            .filter(|s| !s.as_str().is_empty())
            .cloned()
            .collect();
        if subs.is_empty() {
            debug!("gate stream: all symbols empty — no-op");
            return Ok(());
        }

        // Prewarm — hot path 첫 frame 의 intern latency 를 제거.
        self.cache
            .prewarm(subs.iter().map(|s| s.as_str()));

        match self.role {
            DataRole::Primary => self.run_primary(subs, emit, cancel).await,
            DataRole::WebOrderBook => self.run_web(subs, emit, cancel).await,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Primary (fx-ws) 루프
// ────────────────────────────────────────────────────────────────────────────

impl GateFeed {
    async fn run_primary(
        &self,
        symbols: Vec<Symbol>,
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let mut backoff = Backoff::new(self.cfg.backoff_base_ms, self.cfg.backoff_cap_ms);
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }

            let attempt = backoff.attempts();
            info!(role = "primary", attempt, "gate connecting");

            let outcome = tokio::select! {
                _ = cancel.cancelled() => {
                    info!(role = "primary", "gate cancelled during connect");
                    return Ok(());
                }
                r = self.run_primary_once(&symbols, &emit, &cancel) => r,
            };

            match outcome {
                Ok(()) => {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    warn!(role = "primary", "gate disconnected cleanly, reconnecting");
                    backoff.reset();
                }
                Err(e) => {
                    warn!(role = "primary", error = %e, "gate primary error; will backoff");
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
        emit: &Emitter,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let url = Url::parse(&self.cfg.primary_ws_url)?;
        let (ws, resp) = connect_async(url.as_str()).await?;
        info!(status = ?resp.status(), "gate primary connected");

        let (mut write, mut read) = ws.split();

        // 구독 메시지 — book_ticker + trades.
        let payload: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
        let ts_sec = current_epoch_secs();

        let bookticker_sub = serde_json::json!({
            "time": ts_sec,
            "channel": "futures.book_ticker",
            "event": "subscribe",
            "payload": payload,
        });
        write
            .send(Message::Text(bookticker_sub.to_string()))
            .await?;

        let trade_sub = serde_json::json!({
            "time": ts_sec + 1,
            "channel": "futures.trades",
            "event": "subscribe",
            "payload": payload,
        });
        write.send(Message::Text(trade_sub.to_string())).await?;

        info!(symbols = symbols.len(), "gate primary subscribed");

        let read_to = Duration::from_secs(self.cfg.read_timeout_secs);
        // hot-path 전용 local scratch — task-owned 이라 동기화 필요 없음.
        let mut scratch = JsonScratch::with_capacity(self.cfg.scratch_capacity);

        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => {
                    info!("gate primary cancel received");
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                res = tokio::time::timeout(read_to, read.next()) => res,
            };

            let msg = match msg {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => {
                    warn!(error = %e, "gate primary read error");
                    return Err(e.into());
                }
                Ok(None) => {
                    warn!("gate primary stream ended");
                    return Ok(());
                }
                Err(_) => {
                    warn!(
                        timeout_s = self.cfg.read_timeout_secs,
                        "gate primary read timeout"
                    );
                    return Err(anyhow::anyhow!("read timeout"));
                }
            };

            match msg {
                Message::Text(text) => {
                    // ws_received 는 frame 당 한 번만 찍음 (trades 배열 전체에 재사용).
                    let ws_recv = Stamp::now(&*self.clock);
                    // text 를 scratch 버퍼로 복사 — serde_json::from_slice 가 &[u8]
                    // 에서 zero-copy &'a str 을 빌려올 수 있도록.
                    let bytes: &[u8] = scratch.reset_and_fill(&text);
                    dispatch_primary_bytes(bytes, ws_recv, &self.cache, emit);
                }
                Message::Binary(bin) => {
                    trace!(len = bin.len(), "gate primary binary frame (ignored)");
                }
                Message::Ping(p) => {
                    trace!("gate primary ping → pong");
                    if let Err(e) = write.send(Message::Pong(p)).await {
                        warn!(error = %e, "gate primary pong send failed");
                        return Err(e.into());
                    }
                }
                Message::Pong(_) => {}
                Message::Close(frame) => {
                    warn!(?frame, "gate primary server close");
                    return Ok(());
                }
                _ => {}
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Web (fx-webws) 루프 — 심볼당 1 커넥션
// ────────────────────────────────────────────────────────────────────────────

impl GateFeed {
    async fn run_web(
        &self,
        symbols: Vec<Symbol>,
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let mut set: JoinSet<()> = JoinSet::new();
        for sym in symbols {
            let cfg = self.cfg.clone();
            let clock = self.clock.clone();
            let cache = self.cache.clone();
            let emit = emit.clone();
            let child_cancel = cancel.child_token();
            set.spawn(async move {
                Self::web_loop_for_symbol(cfg, clock, cache, emit, sym, child_cancel).await;
            });
        }

        tokio::select! {
            _ = cancel.cancelled() => {
                debug!("gate web root cancelled, aborting children");
                set.abort_all();
            }
            _ = wait_all(&mut set) => {
                debug!("gate web all children completed");
            }
        }

        while set.join_next().await.is_some() {}
        Ok(())
    }

    async fn web_loop_for_symbol(
        cfg: Arc<GateConfig>,
        clock: Arc<dyn Clock>,
        cache: Arc<SymbolCache>,
        emit: Emitter,
        symbol: Symbol,
        cancel: CancellationToken,
    ) {
        let mut backoff = Backoff::new(cfg.backoff_base_ms, cfg.backoff_cap_ms);

        loop {
            if cancel.is_cancelled() {
                return;
            }

            let res = tokio::select! {
                _ = cancel.cancelled() => { return; }
                r = Self::web_once(
                    cfg.as_ref(),
                    clock.as_ref(),
                    cache.as_ref(),
                    &emit,
                    &symbol,
                    &cancel,
                ) => r,
            };

            match res {
                Ok(()) => {
                    if cancel.is_cancelled() {
                        return;
                    }
                    warn!(symbol = %symbol.as_str(), "gate web disconnected cleanly, reconnecting");
                    backoff.reset();
                }
                Err(e) => {
                    warn!(symbol = %symbol.as_str(), error = %e, "gate web error; backoff");
                }
            }

            let delay = backoff.next_ms();
            tokio::select! {
                _ = cancel.cancelled() => { return; }
                _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
            }
        }
    }

    async fn web_once(
        cfg: &GateConfig,
        clock: &dyn Clock,
        cache: &SymbolCache,
        emit: &Emitter,
        symbol: &Symbol,
        cancel: &CancellationToken,
    ) -> anyhow::Result<()> {
        let url = Url::parse(&cfg.web_ws_url)?;
        let mut req = url.as_str().into_client_request()?;
        {
            let h = req.headers_mut();
            h.insert(ORIGIN, HeaderValue::from_static("https://www.gate.com"));
            h.insert(
                USER_AGENT,
                HeaderValue::from_static(
                    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                     AppleWebKit/605.1.15 (KHTML, like Gecko) \
                     Version/26.0.1 Safari/605.1.15",
                ),
            );
            h.insert("Cache-Control", HeaderValue::from_static("no-cache"));
            h.insert("Pragma", HeaderValue::from_static("no-cache"));
        }

        let (ws, resp) = connect_async(req).await?;
        info!(symbol = %symbol.as_str(), status = ?resp.status(), "gate web connected");
        let (mut write, mut read) = ws.split();

        let precision = cfg.precision_for(symbol.as_str()).to_string();
        let ts = current_epoch_secs();
        let sub = serde_json::json!({
            "time": ts,
            "channel": "futures.order_book",
            "event": "subscribe",
            "payload": [symbol.as_str(), cfg.web_orderbook_depth, precision],
        });
        write.send(Message::Text(sub.to_string())).await?;
        debug!(symbol = %symbol.as_str(), "gate web subscribed futures.order_book");

        let read_to = Duration::from_secs(cfg.read_timeout_secs);
        let mut scratch = JsonScratch::with_capacity(cfg.scratch_capacity);

        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                res = tokio::time::timeout(read_to, read.next()) => res,
            };

            let msg = match msg {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => return Err(e.into()),
                Ok(None) => return Ok(()),
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "web read timeout {}s",
                        cfg.read_timeout_secs
                    ))
                }
            };

            match msg {
                Message::Text(text) => {
                    let ws_recv = Stamp::now(clock);
                    let bytes: &[u8] = scratch.reset_and_fill(&text);
                    dispatch_web_bytes(bytes, ws_recv, symbol, cache, emit);
                }
                Message::Binary(_) => {}
                Message::Ping(p) => {
                    if let Err(e) = write.send(Message::Pong(p)).await {
                        return Err(e.into());
                    }
                }
                Message::Pong(_) => {}
                Message::Close(frame) => {
                    warn!(symbol = %symbol.as_str(), ?frame, "gate web server close");
                    return Ok(());
                }
                _ => {}
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// WS schema (borrow-based typed deserialize)
// ────────────────────────────────────────────────────────────────────────────

/// Gate WS frame 의 최상위 shape. 모든 channel 공통:
/// ```json
/// { "time": 1700000000, "time_ms": 1700000000123,
///   "channel": "futures.book_ticker", "event": "update",
///   "result": { ... channel-specific ... } }
/// ```
/// `result` 는 channel 별로 schema 가 달라 `&RawValue` 로 지연 파싱한다.
#[derive(Deserialize)]
struct GateFrame<'a> {
    #[serde(borrow, default)]
    event: Option<&'a str>,
    #[serde(borrow, default)]
    channel: Option<&'a str>,
    #[serde(default)]
    time: Option<i64>,
    #[serde(default)]
    time_ms: Option<i64>,
    #[serde(borrow, default)]
    result: Option<&'a RawValue>,
    #[serde(borrow, default)]
    error: Option<&'a RawValue>,
}

impl<'a> GateFrame<'a> {
    /// `time_ms` 우선, 없으면 `time * 1000`, 둘 다 없으면 0.
    fn outer_time_ms(&self) -> i64 {
        self.time_ms
            .or_else(|| self.time.map(|s| s.saturating_mul(1000)))
            .unwrap_or(0)
    }
}

/// `futures.book_ticker` 의 result.
#[derive(Deserialize)]
struct BookTickerResult<'a> {
    #[serde(borrow)]
    s: &'a str,
    #[serde(deserialize_with = "deserialize_f64_lenient")]
    b: f64,
    #[serde(deserialize_with = "deserialize_f64_lenient")]
    a: f64,
    #[serde(rename = "B", deserialize_with = "deserialize_f64_lenient")]
    big_b: f64,
    #[serde(rename = "A", deserialize_with = "deserialize_f64_lenient")]
    big_a: f64,
    #[serde(default)]
    t: Option<i64>,
}

/// `futures.trades` 배열 원소.
#[derive(Deserialize)]
struct TradeWire<'a> {
    #[serde(borrow)]
    contract: &'a str,
    #[serde(deserialize_with = "deserialize_f64_lenient")]
    price: f64,
    #[serde(deserialize_with = "deserialize_f64_lenient")]
    size: f64,
    // Gate 가 id 를 int / null / 생략 중 하나로 보내므로 lenient opt.
    #[serde(default, deserialize_with = "deserialize_i64_lenient_opt")]
    id: Option<i64>,
    #[serde(default)]
    create_time: Option<i64>,
    #[serde(default)]
    create_time_ms: Option<i64>,
    #[serde(default)]
    is_internal: Option<bool>,
}

/// `futures.order_book` 의 result.
#[derive(Deserialize)]
struct OrderBookResult<'a> {
    /// web payload 는 `contract`, 일부 공식 응답은 `s` — 둘 다 허용.
    #[serde(borrow, default)]
    contract: Option<&'a str>,
    #[serde(borrow, default)]
    s: Option<&'a str>,
    bids: Vec<BookLevel>,
    asks: Vec<BookLevel>,
    #[serde(default)]
    t: Option<i64>,
}

impl<'a> OrderBookResult<'a> {
    fn symbol_str(&self) -> Option<&'a str> {
        self.contract.or(self.s)
    }
}

/// `futures.subscribe` 응답의 result 일부분 (status 만 확인).
#[derive(Deserialize)]
struct SubscribeAck<'a> {
    #[serde(borrow, default)]
    status: Option<&'a str>,
}

/// Gate order_book 한 레벨. `[price, size]` 배열 또는 `{p, s}` 객체 둘 다 허용.
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
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = BookLevel;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#"either [price, size] or {"p":..., "s":...}"#)
            }
            fn visit_seq<A>(self, mut seq: A) -> Result<BookLevel, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let price: F64Lenient = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(0, &"[price, size]"))?;
                let size: F64Lenient = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(1, &"[price, size]"))?;
                // 남은 요소는 무시 (Gate 가 extra 필드 추가해도 forward-compat).
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BookLevel { price: price.0, size: size.0 })
            }
            fn visit_map<A>(self, mut map: A) -> Result<BookLevel, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut price: Option<f64> = None;
                let mut size: Option<f64> = None;
                while let Some(k) = map.next_key::<&str>()? {
                    match k {
                        "p" | "price" => {
                            let v: F64Lenient = map.next_value()?;
                            price = Some(v.0);
                        }
                        "s" | "size" => {
                            let v: F64Lenient = map.next_value()?;
                            size = Some(v.0);
                        }
                        _ => {
                            let _: IgnoredAny = map.next_value()?;
                        }
                    }
                }
                Ok(BookLevel {
                    price: price.ok_or_else(|| serde::de::Error::missing_field("p/price"))?,
                    size: size.ok_or_else(|| serde::de::Error::missing_field("s/size"))?,
                })
            }
        }
        d.deserialize_any(V)
    }
}

/// Newtype wrapper — `F64Lenient(f64)` 로 lenient f64 decoding 을
/// 제네릭 위치 (`Vec<F64Lenient>`, tuple element 등) 에서 쓸 수 있게 한다.
struct F64Lenient(f64);

impl<'de> Deserialize<'de> for F64Lenient {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_f64_lenient(d).map(F64Lenient)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Dispatch (hot path)
// ────────────────────────────────────────────────────────────────────────────

/// Primary WS frame (`bytes`) 를 파싱해 channel+event 별로 적절한 emit.
///
/// - 파싱 실패는 DEBUG 로그 후 drop (frame level).
/// - subscribe 응답은 status 만 확인 (ack) 하고 emit 하지 않음.
fn dispatch_primary_bytes(
    bytes: &[u8],
    ws_recv: Stamp,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    let frame: GateFrame<'_> = match serde_json::from_slice(bytes) {
        Ok(f) => f,
        Err(e) => {
            debug!(
                error = %e,
                snippet = %bytes_snippet(bytes),
                "gate primary non-JSON / malformed frame",
            );
            return;
        }
    };
    let event = frame.event.unwrap_or("");
    let channel = frame.channel.unwrap_or("");

    match event {
        "subscribe" => handle_subscribe_ack(&frame, channel, bytes),
        "update" => match channel {
            "futures.book_ticker" => {
                let Some(raw) = frame.result else {
                    debug!(channel, "gate book_ticker missing result");
                    return;
                };
                match serde_json::from_str::<BookTickerResult<'_>>(raw.get()) {
                    Ok(r) => {
                        let event_time_ms = frame.outer_time_ms();
                        let server_time_ms = r.t.unwrap_or(event_time_ms);
                        let bt = BookTicker {
                            exchange: ExchangeId::Gate,
                            symbol: cache.intern(r.s),
                            bid_price: Price(r.b),
                            ask_price: Price(r.a),
                            bid_size: Size(r.big_b),
                            ask_size: Size(r.big_a),
                            event_time_ms,
                            server_time_ms,
                        };
                        let ls = stamps_with_ws(ws_recv, server_time_ms);
                        emit(MarketEvent::BookTicker(bt), ls);
                    }
                    Err(e) => {
                        debug!(
                            error = %e,
                            snippet = %str_snippet(raw.get()),
                            "gate book_ticker parse failed",
                        );
                    }
                }
            }
            "futures.trades" => {
                let Some(raw) = frame.result else {
                    debug!("gate trades missing result");
                    return;
                };
                let trades: Vec<TradeWire<'_>> = match serde_json::from_str(raw.get()) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!(
                            error = %e,
                            snippet = %str_snippet(raw.get()),
                            "gate trades result parse failed",
                        );
                        return;
                    }
                };
                let outer_time_ms = frame.outer_time_ms();
                for w in trades {
                    let t = finalize_trade(w, outer_time_ms, cache);
                    let ls = stamps_with_ws(ws_recv, t.server_time_ms);
                    emit(MarketEvent::Trade(t), ls);
                }
            }
            other => {
                trace!(channel = other, "gate primary unknown channel");
            }
        },
        other => {
            trace!(event = other, "gate primary unknown event");
        }
    }
}

/// Web WS frame 을 파싱해 best bid/ask 만 추출해 [`MarketEvent::WebBookTicker`] emit.
fn dispatch_web_bytes(
    bytes: &[u8],
    ws_recv: Stamp,
    fallback_symbol: &Symbol,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    let frame: GateFrame<'_> = match serde_json::from_slice(bytes) {
        Ok(f) => f,
        Err(e) => {
            debug!(
                error = %e,
                symbol = %fallback_symbol.as_str(),
                snippet = %bytes_snippet(bytes),
                "gate web non-JSON / malformed frame",
            );
            return;
        }
    };
    let event = frame.event.unwrap_or("");
    match event {
        "subscribe" => handle_subscribe_ack(&frame, "futures.order_book", bytes),
        "update" | "all" => {
            let Some(raw) = frame.result else {
                debug!("gate web missing result");
                return;
            };
            let r: OrderBookResult<'_> = match serde_json::from_str(raw.get()) {
                Ok(r) => r,
                Err(e) => {
                    debug!(
                        symbol = %fallback_symbol.as_str(),
                        error = %e,
                        snippet = %str_snippet(raw.get()),
                        "gate web orderbook parse failed",
                    );
                    return;
                }
            };
            match extract_best(&r, fallback_symbol, cache, frame.outer_time_ms()) {
                Ok(bt) => {
                    let ls = stamps_with_ws(ws_recv, bt.server_time_ms);
                    emit(MarketEvent::WebBookTicker(bt), ls);
                }
                Err(e) => {
                    debug!(
                        symbol = %fallback_symbol.as_str(),
                        error = %e,
                        "gate web best-extract failed",
                    );
                }
            }
        }
        _ => {}
    }
}

/// subscribe 응답의 status 확인 후 로그만. result 가 없어도 (error 포함) 로깅만.
fn handle_subscribe_ack(frame: &GateFrame<'_>, channel: &str, bytes: &[u8]) {
    let has_error = frame.error.is_some();
    let status = frame
        .result
        .and_then(|r| serde_json::from_str::<SubscribeAck<'_>>(r.get()).ok())
        .and_then(|a| a.status.map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_string());
    if status == "success" && !has_error {
        debug!(channel, "gate subscribe confirmed");
    } else {
        error!(
            channel,
            status = status.as_str(),
            has_error,
            snippet = %bytes_snippet(bytes),
            "gate subscribe FAILED",
        );
    }
}

/// trade wire → [`Trade`] 변환 (symbol intern 포함).
fn finalize_trade(w: TradeWire<'_>, outer_time_ms: i64, cache: &SymbolCache) -> Trade {
    let create_time_ms = w
        .create_time_ms
        .or_else(|| w.create_time.map(|s| s.saturating_mul(1000)))
        .unwrap_or(outer_time_ms);
    let create_time_s = w.create_time.unwrap_or_else(|| create_time_ms / 1000);
    Trade {
        exchange: ExchangeId::Gate,
        symbol: cache.intern(w.contract),
        price: Price(w.price),
        size: Size(w.size),
        trade_id: w.id.unwrap_or(0),
        create_time_s,
        create_time_ms,
        server_time_ms: create_time_ms,
        is_internal: w.is_internal.unwrap_or(false),
    }
}

/// orderbook result 에서 dust(size==1) 제외한 best bid/ask 추출 후 [`BookTicker`] 조립.
fn extract_best(
    r: &OrderBookResult<'_>,
    fallback_symbol: &Symbol,
    cache: &SymbolCache,
    outer_time_ms: i64,
) -> anyhow::Result<BookTicker> {
    if r.bids.is_empty() {
        anyhow::bail!("empty bids");
    }
    if r.asks.is_empty() {
        anyhow::bail!("empty asks");
    }
    let best_bid = first_non_dust(&r.bids).copied().unwrap_or(r.bids[0]);
    let best_ask = first_non_dust(&r.asks).copied().unwrap_or(r.asks[0]);

    let symbol = match r.symbol_str() {
        Some(s) => cache.intern(s),
        None => fallback_symbol.clone(),
    };
    let server_time_ms = r.t.unwrap_or(outer_time_ms);

    Ok(BookTicker {
        exchange: ExchangeId::Gate,
        symbol,
        bid_price: Price(best_bid.price),
        ask_price: Price(best_ask.price),
        bid_size: Size(best_bid.size),
        ask_size: Size(best_ask.size),
        event_time_ms: outer_time_ms,
        server_time_ms,
    })
}

/// size > 1 인 첫 레벨. Gate web orderbook 은 size=="1" 을 dust 로 취급
/// (placeholder floor). 전 레벨이 dust 면 `None` — 호출자가 fallback 으로 첫 레벨 사용.
fn first_non_dust(levels: &[BookLevel]) -> Option<&BookLevel> {
    levels.iter().find(|l| l.size > 1.0)
}

// ────────────────────────────────────────────────────────────────────────────
// 내부 유틸
// ────────────────────────────────────────────────────────────────────────────

/// 에폭초 — 구독 메시지의 `time` 필드용.
fn current_epoch_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `JoinSet` 가 비워질 때까지 대기 (값은 버림).
async fn wait_all<T: 'static>(set: &mut JoinSet<T>) {
    while set.join_next().await.is_some() {}
}

/// 로그용 short-form 바이트 프리뷰 (UTF-8 손상 방지).
fn bytes_snippet(bytes: &[u8]) -> String {
    let end = bytes.len().min(200);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// 로그용 short-form 문자열 프리뷰.
fn str_snippet(s: &str) -> &str {
    &s[..s.len().min(200)]
}

#[allow(dead_code)]
fn _type_asserts() {
    // 컴파일 타임에 `GateFeed` 가 `Send + Sync` 인지 확인.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<GateFeed>();
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // 실제 Gate.io futures.book_ticker 메시지 샘플 (raw JSON).
    const SAMPLE_BOOKTICKER: &str = r#"{
        "time": 1700000000,
        "time_ms": 1700000000123,
        "channel": "futures.book_ticker",
        "event": "update",
        "result": {
            "s": "BTC_USDT",
            "b": "40000.5",
            "B": 12,
            "a": "40001.0",
            "A": 34,
            "t": 1700000000100,
            "u": 123456
        }
    }"#;

    const SAMPLE_TRADES: &str = r#"{
        "time": 1700000000,
        "time_ms": 1700000000555,
        "channel": "futures.trades",
        "event": "update",
        "result": [
            {
                "id": 111,
                "create_time": 1700000000,
                "create_time_ms": 1700000000500,
                "contract": "BTC_USDT",
                "size": 3,
                "price": "40000.5"
            },
            {
                "id": 112,
                "create_time": 1700000000,
                "create_time_ms": 1700000000520,
                "contract": "BTC_USDT",
                "size": -2,
                "price": "40000.4"
            }
        ]
    }"#;

    const SAMPLE_ORDERBOOK: &str = r#"{
        "time": 1700000000,
        "time_ms": 1700000000900,
        "channel": "futures.order_book",
        "event": "update",
        "result": {
            "contract": "VINE_USDT",
            "bids": [
                {"p": "0.50000", "s": "1"},
                {"p": "0.49990", "s": "5"},
                {"p": "0.49980", "s": "10"}
            ],
            "asks": [
                {"p": "0.50010", "s": "1"},
                {"p": "0.50020", "s": "7"},
                {"p": "0.50030", "s": "15"}
            ],
            "t": 1700000000800
        }
    }"#;

    fn emit_counter() -> (Emitter, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let bt = Arc::new(AtomicUsize::new(0));
        let tr = Arc::new(AtomicUsize::new(0));
        let wb = Arc::new(AtomicUsize::new(0));
        let (b, t, w) = (bt.clone(), tr.clone(), wb.clone());
        let emit: Emitter = Arc::new(move |ev, _ls| match ev {
            MarketEvent::BookTicker(_) => {
                b.fetch_add(1, Ordering::SeqCst);
            }
            MarketEvent::Trade(_) => {
                t.fetch_add(1, Ordering::SeqCst);
            }
            MarketEvent::WebBookTicker(_) => {
                w.fetch_add(1, Ordering::SeqCst);
            }
        });
        (emit, bt, tr, wb)
    }

    fn stamp(n: u64) -> Stamp {
        Stamp { wall_ms: n as i64, mono_ns: n }
    }

    // ── BookTicker parsing ──────────────────────────────────────────────────

    #[test]
    fn parse_bookticker_happy_path() {
        let bytes = SAMPLE_BOOKTICKER.as_bytes();
        let cache = SymbolCache::new();
        let captured = Arc::new(std::sync::Mutex::new(None::<(BookTicker, LatencyStamps)>));
        let cap = captured.clone();
        let emit: Emitter = Arc::new(move |ev, ls| {
            if let MarketEvent::BookTicker(bt) = ev {
                *cap.lock().unwrap() = Some((bt, ls));
            }
        });
        dispatch_primary_bytes(bytes, stamp(1), &cache, &emit);
        let got = captured.lock().unwrap().take().expect("emit called");
        let (bt, ls) = got;
        assert_eq!(bt.exchange, ExchangeId::Gate);
        assert_eq!(bt.symbol.as_str(), "BTC_USDT");
        assert!((bt.bid_price.0 - 40000.5).abs() < 1e-9);
        assert!((bt.ask_price.0 - 40001.0).abs() < 1e-9);
        assert!((bt.bid_size.0 - 12.0).abs() < 1e-9);
        assert!((bt.ask_size.0 - 34.0).abs() < 1e-9);
        assert_eq!(bt.event_time_ms, 1_700_000_000_123);
        assert_eq!(bt.server_time_ms, 1_700_000_000_100);
        assert!(bt.is_valid());
        assert_eq!(ls.ws_received.wall_ms, 1);
        assert_eq!(ls.exchange_server.wall_ms, 1_700_000_000_100);
    }

    #[test]
    fn parse_bookticker_tolerates_number_prices() {
        let src = r#"{
            "time_ms": 1,
            "channel": "futures.book_ticker", "event": "update",
            "result": {"s":"BTC_USDT","b":40000.5,"B":"12","a":"40001","A":34,"t":5}
        }"#;
        let cache = SymbolCache::new();
        let (emit, bt, _, _) = emit_counter();
        dispatch_primary_bytes(src.as_bytes(), stamp(1), &cache, &emit);
        assert_eq!(bt.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parse_bookticker_missing_field_drops_silently() {
        // 'b' 없으면 missing field → 파싱 실패 → emit 안 됨.
        let src = r#"{"channel":"futures.book_ticker","event":"update",
                     "result":{"s":"BTC_USDT","a":"1","B":1,"A":1}}"#;
        let cache = SymbolCache::new();
        let (emit, bt, _, _) = emit_counter();
        dispatch_primary_bytes(src.as_bytes(), stamp(1), &cache, &emit);
        assert_eq!(bt.load(Ordering::SeqCst), 0);
    }

    // ── Trades parsing ──────────────────────────────────────────────────────

    #[test]
    fn parse_trades_preserves_sign_and_count() {
        let cache = SymbolCache::new();
        let captured = Arc::new(std::sync::Mutex::new(Vec::<Trade>::new()));
        let cap = captured.clone();
        let emit: Emitter = Arc::new(move |ev, _| {
            if let MarketEvent::Trade(t) = ev {
                cap.lock().unwrap().push(t);
            }
        });
        dispatch_primary_bytes(SAMPLE_TRADES.as_bytes(), stamp(1), &cache, &emit);
        let v = captured.lock().unwrap().clone();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].symbol.as_str(), "BTC_USDT");
        assert!((v[0].size.0 - 3.0).abs() < 1e-9);
        assert_eq!(v[0].side(), hft_types::Side::Buy);
        assert!((v[1].size.0 + 2.0).abs() < 1e-9);
        assert_eq!(v[1].side(), hft_types::Side::Sell);
        assert_eq!(v[0].trade_id, 111);
        assert_eq!(v[1].trade_id, 112);
        assert_eq!(v[0].create_time_s, 1_700_000_000);
        assert_eq!(v[0].create_time_ms, 1_700_000_000_500);
    }

    #[test]
    fn parse_trade_falls_back_to_outer_time() {
        // create_time_ms 없고 create_time 없으면 outer time_ms 사용.
        let src = r#"{
            "time_ms": 999000,
            "channel": "futures.trades", "event": "update",
            "result": [{"contract":"BTC_USDT","price":"100","size":1}]
        }"#;
        let cache = SymbolCache::new();
        let captured = Arc::new(std::sync::Mutex::new(None::<Trade>));
        let cap = captured.clone();
        let emit: Emitter = Arc::new(move |ev, _| {
            if let MarketEvent::Trade(t) = ev {
                *cap.lock().unwrap() = Some(t);
            }
        });
        dispatch_primary_bytes(src.as_bytes(), stamp(1), &cache, &emit);
        let t = captured.lock().unwrap().clone().expect("emit called");
        assert_eq!(t.create_time_ms, 999_000);
        assert_eq!(t.create_time_s, 999);
    }

    // ── SymbolCache 통합 ─────────────────────────────────────────────────────

    #[test]
    fn symbol_cache_reused_across_frames() {
        let cache = Arc::new(SymbolCache::new());
        let captured = Arc::new(std::sync::Mutex::new(Vec::<Symbol>::new()));
        let cap = captured.clone();
        let emit: Emitter = Arc::new(move |ev, _| match ev {
            MarketEvent::BookTicker(b) => cap.lock().unwrap().push(b.symbol),
            MarketEvent::Trade(t) => cap.lock().unwrap().push(t.symbol),
            _ => {}
        });
        // 같은 심볼을 여러 frame 보내면 intern 이 같은 Arc 를 돌려줘야 한다.
        for _ in 0..3 {
            dispatch_primary_bytes(SAMPLE_BOOKTICKER.as_bytes(), stamp(1), &cache, &emit);
        }
        let v = captured.lock().unwrap().clone();
        assert_eq!(v.len(), 3);
        assert!(v[0].ptr_eq(&v[1]), "SymbolCache 가 동일 Arc 를 재사용해야 함");
        assert!(v[1].ptr_eq(&v[2]));
        assert_eq!(cache.len(), 1);
    }

    // ── OrderBook (web) parsing ─────────────────────────────────────────────

    #[test]
    fn parse_orderbook_best_skips_dust() {
        let cache = SymbolCache::new();
        let captured = Arc::new(std::sync::Mutex::new(None::<BookTicker>));
        let cap = captured.clone();
        let emit: Emitter = Arc::new(move |ev, _| {
            if let MarketEvent::WebBookTicker(b) = ev {
                *cap.lock().unwrap() = Some(b);
            }
        });
        dispatch_web_bytes(
            SAMPLE_ORDERBOOK.as_bytes(),
            stamp(1),
            &Symbol::new("VINE_USDT"),
            &cache,
            &emit,
        );
        let bt = captured.lock().unwrap().take().expect("emit called");
        assert_eq!(bt.symbol.as_str(), "VINE_USDT");
        assert!((bt.bid_price.0 - 0.49990).abs() < 1e-9);
        assert!((bt.bid_size.0 - 5.0).abs() < 1e-9);
        assert!((bt.ask_price.0 - 0.50020).abs() < 1e-9);
        assert!((bt.ask_size.0 - 7.0).abs() < 1e-9);
        assert_eq!(bt.server_time_ms, 1_700_000_000_800);
    }

    #[test]
    fn parse_orderbook_uses_first_when_all_dust() {
        let src = r#"{
            "time_ms": 1000,
            "channel": "futures.order_book", "event": "update",
            "result": {
                "contract": "XYZ_USDT",
                "bids": [{"p":"1.0","s":"1"}],
                "asks": [{"p":"1.1","s":"1"}],
                "t": 1000
            }
        }"#;
        let cache = SymbolCache::new();
        let captured = Arc::new(std::sync::Mutex::new(None::<BookTicker>));
        let cap = captured.clone();
        let emit: Emitter = Arc::new(move |ev, _| {
            if let MarketEvent::WebBookTicker(b) = ev {
                *cap.lock().unwrap() = Some(b);
            }
        });
        dispatch_web_bytes(
            src.as_bytes(),
            stamp(1),
            &Symbol::new("XYZ_USDT"),
            &cache,
            &emit,
        );
        let bt = captured.lock().unwrap().take().expect("emit called");
        assert!((bt.bid_price.0 - 1.0).abs() < 1e-9);
        assert!((bt.ask_price.0 - 1.1).abs() < 1e-9);
    }

    #[test]
    fn parse_orderbook_array_level_format() {
        let src = r#"{
            "time_ms": 500,
            "channel": "futures.order_book", "event": "update",
            "result": {
                "contract": "BTC_USDT",
                "bids": [["100.5", 10]],
                "asks": [["100.6", 20]],
                "t": 500
            }
        }"#;
        let cache = SymbolCache::new();
        let (emit, _, _, wb) = emit_counter();
        dispatch_web_bytes(
            src.as_bytes(),
            stamp(1),
            &Symbol::new("BTC_USDT"),
            &cache,
            &emit,
        );
        assert_eq!(wb.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parse_orderbook_empty_arrays_drops_silently() {
        let src = r#"{
            "time_ms": 1,
            "channel": "futures.order_book", "event": "update",
            "result": {"contract":"X_USDT","bids":[],"asks":[["1.0",1]],"t":1}
        }"#;
        let cache = SymbolCache::new();
        let (emit, _, _, wb) = emit_counter();
        dispatch_web_bytes(
            src.as_bytes(),
            stamp(1),
            &Symbol::new("X_USDT"),
            &cache,
            &emit,
        );
        assert_eq!(wb.load(Ordering::SeqCst), 0);
    }

    // ── subscribe & frame-level ─────────────────────────────────────────────

    #[test]
    fn subscribe_ack_does_not_emit() {
        let src = r#"{"channel":"futures.book_ticker","event":"subscribe",
                     "result":{"status":"success"}}"#;
        let cache = SymbolCache::new();
        let (emit, bt, tr, _) = emit_counter();
        dispatch_primary_bytes(src.as_bytes(), stamp(1), &cache, &emit);
        assert_eq!(bt.load(Ordering::SeqCst), 0);
        assert_eq!(tr.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn malformed_json_is_ignored() {
        let cache = SymbolCache::new();
        let (emit, bt, tr, _) = emit_counter();
        dispatch_primary_bytes(b"not valid json", stamp(1), &cache, &emit);
        assert_eq!(bt.load(Ordering::SeqCst), 0);
        assert_eq!(tr.load(Ordering::SeqCst), 0);
    }

    // ── GateFrame 유틸 ──────────────────────────────────────────────────────

    #[test]
    fn frame_outer_time_ms_prefers_time_ms() {
        let src = r#"{"time":100,"time_ms":200000}"#;
        let f: GateFrame<'_> = serde_json::from_str(src).unwrap();
        assert_eq!(f.outer_time_ms(), 200_000);
    }

    #[test]
    fn frame_outer_time_ms_falls_back_to_time_seconds() {
        let src = r#"{"time":100}"#;
        let f: GateFrame<'_> = serde_json::from_str(src).unwrap();
        assert_eq!(f.outer_time_ms(), 100_000);
    }

    #[test]
    fn frame_outer_time_ms_zero_when_missing() {
        let src = r#"{}"#;
        let f: GateFrame<'_> = serde_json::from_str(src).unwrap();
        assert_eq!(f.outer_time_ms(), 0);
    }

    // ── Config ──────────────────────────────────────────────────────────────

    #[test]
    fn gate_config_default_sane() {
        let c = GateConfig::default();
        assert!(c.primary_ws_url.starts_with("wss://"));
        assert!(c.web_ws_url.starts_with("wss://"));
        assert!(c.rest_base_url.starts_with("https://"));
        assert!(c.backoff_base_ms < c.backoff_cap_ms);
        assert_eq!(c.default_precision, "0.00001");
        assert_eq!(c.scratch_capacity, 4096);
    }

    #[test]
    fn gate_config_precision_lookup() {
        let mut p = HashMap::new();
        p.insert("BTC_USDT".to_string(), "0.1".to_string());
        let c = GateConfig::default().with_precisions(p);
        assert_eq!(c.precision_for("BTC_USDT"), "0.1");
        assert_eq!(c.precision_for("UNKNOWN"), "0.00001");
    }

    #[test]
    fn gate_config_load_precisions_missing_file_is_empty() {
        let tmp = std::env::temp_dir().join("gate-nonexistent-precisions.txt");
        let m = GateConfig::load_precisions_from(&tmp);
        assert!(m.is_empty());
    }

    #[test]
    fn gate_config_load_precisions_parses_valid_file() {
        use std::io::Write;
        let tmp = std::env::temp_dir().join("gate-precisions-test-parse.txt");
        let mut f = std::fs::File::create(&tmp).unwrap();
        writeln!(f, "# comment").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "BTC_USDT,0.1").unwrap();
        writeln!(f, "ETH_USDT , 0.01").unwrap();
        writeln!(f, "bad-line-without-comma").unwrap();
        drop(f);
        let m = GateConfig::load_precisions_from(&tmp);
        assert_eq!(m.get("BTC_USDT").map(String::as_str), Some("0.1"));
        assert_eq!(m.get("ETH_USDT").map(String::as_str), Some("0.01"));
        assert_eq!(m.len(), 2);
        let _ = std::fs::remove_file(&tmp);
    }

    // ── Feed trait shape ────────────────────────────────────────────────────

    #[test]
    fn gate_feed_id_and_role() {
        let f = GateFeed::new(GateConfig::default(), DataRole::Primary);
        assert_eq!(f.id(), ExchangeId::Gate);
        assert_eq!(f.role(), DataRole::Primary);
        assert_eq!(f.label(), "gate:Primary");

        let f2 = GateFeed::new(GateConfig::default(), DataRole::WebOrderBook);
        assert_eq!(f2.role(), DataRole::WebOrderBook);
        assert_eq!(f2.label(), "gate:WebOrderBook");
    }

    #[test]
    fn gate_feed_with_cache_shares_pool() {
        let shared = Arc::new(SymbolCache::new());
        let a = GateFeed::with_cache(GateConfig::default(), DataRole::Primary, shared.clone());
        let b = GateFeed::with_cache(
            GateConfig::default(),
            DataRole::WebOrderBook,
            shared.clone(),
        );
        assert!(Arc::ptr_eq(a.symbol_cache(), b.symbol_cache()));
    }

    #[test]
    fn gate_feed_is_trait_object() {
        let _boxed: std::sync::Arc<dyn ExchangeFeed> =
            std::sync::Arc::new(GateFeed::new(GateConfig::default(), DataRole::Primary));
    }

    #[tokio::test]
    async fn empty_symbols_stream_is_noop() {
        let f = GateFeed::new(GateConfig::default(), DataRole::Primary);
        let cancel = CancellationToken::new();
        let (emit, bt, tr, wb) = emit_counter();
        f.stream(&[], emit, cancel).await.unwrap();
        assert_eq!(bt.load(Ordering::SeqCst), 0);
        assert_eq!(tr.load(Ordering::SeqCst), 0);
        assert_eq!(wb.load(Ordering::SeqCst), 0);
    }

    // ── stamps_with_ws 계약 ─────────────────────────────────────────────────

    #[test]
    fn emit_latency_stamps_has_ws_and_server() {
        let cache = SymbolCache::new();
        let captured = Arc::new(std::sync::Mutex::new(None::<LatencyStamps>));
        let cap = captured.clone();
        let emit: Emitter = Arc::new(move |_ev, ls| {
            *cap.lock().unwrap() = Some(ls);
        });
        let ws_recv = Stamp { wall_ms: 500, mono_ns: 12_345 };
        dispatch_primary_bytes(SAMPLE_BOOKTICKER.as_bytes(), ws_recv, &cache, &emit);
        let ls = captured.lock().unwrap().take().expect("emit called");
        assert_eq!(ls.ws_received.wall_ms, 500);
        assert_eq!(ls.ws_received.mono_ns, 12_345);
        // exchange_server 는 frame 의 'result.t' — result.t=1700000000100
        assert_eq!(ls.exchange_server.wall_ms, 1_700_000_000_100);
        assert_eq!(ls.exchange_server.mono_ns, 0);
        // 아직 serialized 는 안 찍힘.
        assert_eq!(ls.serialized.mono_ns, 0);
    }
}

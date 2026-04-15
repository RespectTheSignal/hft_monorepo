//! hft-exchange-okx — OKX SWAP (v5) public feed.
//!
//! ## 설계 (Phase 2 refactor: zero-alloc hot-path)
//!
//! - **URL**: `wss://ws.okx.com:8443/ws/v5/public`.
//! - **Subscribe**: symbol 당 `bbo-tbt` + `trades` 두 채널을 동시에 구독.
//!   ```json
//!   {"op":"subscribe","args":[
//!     {"channel":"bbo-tbt","instId":"BTC-USDT-SWAP"},
//!     {"channel":"trades", "instId":"BTC-USDT-SWAP"}, …]}
//!   ```
//!   `args` 는 [`MAX_SUBSCRIBE_ARGS`] = 100 개씩 chunk.
//! - **bbo-tbt**: best bid/ask tick-by-tick → [`MarketEvent::BookTicker`].
//! - **trades**: public trade stream → 1 프레임 당 배열 원소 수 만큼
//!   [`MarketEvent::Trade`]. `side` 가 `sell` 이면 `size` 부호 음수.
//! - **Client ping**: [`OkxConfig::ping_interval_secs`] (기본 20s) 간격으로
//!   **text frame `"ping"`** 송신. 서버도 text `"pong"` 반환. 이는 WS
//!   protocol Ping frame 이 아니라 OKX 고유 spec. 중간 프록시가 WS Ping frame
//!   을 보낼 수도 있어 그것도 방어적으로 `Pong` 반사.
//! - **압축 없음, 바이너리 없음** (방어적으로 UTF-8 fallback 만 유지).
//! - **Bbo-tbt Frame**:
//!   ```json
//!   {
//!     "arg":{"channel":"bbo-tbt","instId":"BTC-USDT-SWAP"},
//!     "data":[{
//!       "bids":[["p","s","lo","co"],…],
//!       "asks":[["p","s","lo","co"],…],
//!       "ts":"1700000000000"
//!     }]
//!   }
//!   ```
//!   bbo-tbt 의 level 튜플은 `[price, size, liquidated_orders, count]` 지만
//!   `price/size` 만 사용. `bids/asks` 가 비어있으면 skip.
//! - **Trades Frame**:
//!   ```json
//!   {
//!     "arg":{"channel":"trades","instId":"BTC-USDT-SWAP"},
//!     "data":[{"instId":"BTC-USDT-SWAP","tradeId":"130639474",
//!              "px":"42219.9","sz":"0.12",
//!              "side":"buy","ts":"1629386781174"}]
//!   }
//!   ```
//!   `tradeId` 는 수치 문자열이지만 간혹 overflow/비숫자 가능 — 그때는
//!   fixed-seed ahash 로 i63 양수 해시 생성 후 fallback.
//!
//! ## 심볼 매핑
//!
//! - canonical `BTC_USDT` ↔ instId `BTC-USDT-SWAP`.
//! - 구독 시점에 **instId → `Symbol`** 테이블을 미리 빌드 → 이벤트당 lookup
//!   한 번으로 끝. miss 경로는 `from_okx_inst_id` + [`SymbolCache::intern`].
//!
//! ## 계약
//!
//! - [`OkxFeed::stream`] 은 내부 재연결. 상위에는 `Ok(())` (cancel) /
//!   초기화 실패 시 `Err`.
//! - `bbo-tbt` 프레임 1건 당 [`MarketEvent::BookTicker`] 를 **정확히 1회** emit.
//! - `trades` 프레임 1건 당 `data` 배열 길이만큼 [`MarketEvent::Trade`] emit.
//! - [`LatencyStamps`] 는 `ws_received` + `exchange_server_ms` (=entry.ts) 채움.

#![deny(rust_2018_idioms)]
#![deny(unused_must_use)]

pub mod executor;
pub use executor::{OkxExecutor, OkxExecutorConfig};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
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

/// OKX subscribe `args` chunk 최대값 (공식 권고치 대비 conservative).
const MAX_SUBSCRIBE_ARGS: usize = 100;

/// 텍스트 scratch 기본 capacity — 본 크레이트는 현재 직접 사용하지 않지만
/// config 의 일관성 확보를 위해 유지.
const DEFAULT_SCRATCH_CAPACITY: usize = 4096;

// ────────────────────────────────────────────────────────────────────────────
// OkxConfig
// ────────────────────────────────────────────────────────────────────────────

/// OKX feed 구성.
#[derive(Debug, Clone)]
pub struct OkxConfig {
    /// Public WebSocket URL (v5 SWAP public).
    pub ws_url: String,
    /// REST API base (www.okx.com — aws.okx.com 대체 가능).
    pub rest_base_url: String,
    /// 상품군 — `"SWAP"` 이 USDT-M perp.
    pub inst_type: String,
    /// 결제 통화 — `"USDT"` 만 필터링.
    pub settle_ccy: String,
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
    /// Hot-path scratch 버퍼 초기 capacity.
    pub scratch_capacity: usize,
}

impl Default for OkxConfig {
    fn default() -> Self {
        Self {
            ws_url: "wss://ws.okx.com:8443/ws/v5/public".into(),
            rest_base_url: "https://www.okx.com".into(),
            inst_type: "SWAP".into(),
            settle_ccy: "USDT".into(),
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
// OkxFeed
// ────────────────────────────────────────────────────────────────────────────

/// OKX SWAP `bbo-tbt` feed.
///
/// `SymbolCache` 는 여러 feed 인스턴스 간 공유 가능. 상위 오케스트레이션에서
/// Arc clone 하여 주입하면 동일한 intern pool 을 공유한다.
pub struct OkxFeed {
    cfg: Arc<OkxConfig>,
    clock: Arc<dyn Clock>,
    cache: Arc<SymbolCache>,
}

impl OkxFeed {
    /// 기본 `SystemClock` + 새 `SymbolCache` 로 생성.
    pub fn new(cfg: OkxConfig) -> Self {
        Self {
            cfg: Arc::new(cfg),
            clock: Arc::new(SystemClock::new()),
            cache: Arc::new(SymbolCache::new()),
        }
    }

    /// 외부 clock 주입 (테스트 `MockClock` 등).
    pub fn with_clock(cfg: OkxConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            cfg: Arc::new(cfg),
            clock,
            cache: Arc::new(SymbolCache::new()),
        }
    }

    /// 공유 `SymbolCache` 주입.
    pub fn with_cache(cfg: OkxConfig, cache: Arc<SymbolCache>) -> Self {
        Self {
            cfg: Arc::new(cfg),
            clock: Arc::new(SystemClock::new()),
            cache,
        }
    }

    /// clock + cache 둘 다 외부 주입.
    pub fn with_clock_and_cache(
        cfg: OkxConfig,
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

    pub fn config(&self) -> &OkxConfig {
        &self.cfg
    }

    /// 내부 `SymbolCache` 에 대한 shared reference.
    pub fn symbol_cache(&self) -> &Arc<SymbolCache> {
        &self.cache
    }
}

#[async_trait]
impl ExchangeFeed for OkxFeed {
    fn id(&self) -> ExchangeId {
        ExchangeId::Okx
    }

    fn role(&self) -> DataRole {
        DataRole::Primary
    }

    async fn available_symbols(&self) -> anyhow::Result<Vec<Symbol>> {
        let url = format!(
            "{}/api/v5/public/instruments?instType={}",
            self.cfg.rest_base_url, self.cfg.inst_type
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(self.cfg.rest_timeout_ms))
            .build()?;
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("okx instruments status = {}", resp.status());
        }
        let json: Value = resp.json().await?;
        let code = json.get("code").and_then(|v| v.as_str()).unwrap_or("0");
        if code != "0" {
            anyhow::bail!("okx code = {}", code);
        }
        let list = json
            .get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut seen = std::collections::HashSet::new();
        for item in list {
            if item.get("state").and_then(|v| v.as_str()) != Some("live") {
                continue;
            }
            if !self.cfg.settle_ccy.is_empty()
                && item.get("settleCcy").and_then(|v| v.as_str())
                    != Some(self.cfg.settle_ccy.as_str())
            {
                continue;
            }
            let base = item.get("baseCcy").and_then(|v| v.as_str());
            let quote = item.get("quoteCcy").and_then(|v| v.as_str());
            if let (Some(b), Some(q)) = (base, quote) {
                if !b.is_empty() && !q.is_empty() {
                    seen.insert(format!("{}_{}", b, q));
                    continue;
                }
            }
            // fallback: instId 파싱 (`BTC-USDT-SWAP`).
            if let Some(iid) = item.get("instId").and_then(|v| v.as_str()) {
                seen.insert(from_okx_inst_id(iid));
            }
        }
        let mut out: Vec<Symbol> = seen.into_iter().map(Symbol::new).collect();
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        info!(count = out.len(), "okx available_symbols fetched");
        Ok(out)
    }

    async fn stream(
        &self,
        symbols: &[Symbol],
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        if symbols.is_empty() {
            debug!("okx stream: no symbols — no-op");
            return Ok(());
        }
        let subs: Vec<Symbol> = symbols
            .iter()
            .filter(|s| !s.as_str().is_empty())
            .cloned()
            .collect();
        if subs.is_empty() {
            debug!("okx stream: all empty — no-op");
            return Ok(());
        }
        self.run_primary(subs, emit, cancel).await
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Primary 루프
// ────────────────────────────────────────────────────────────────────────────

impl OkxFeed {
    async fn run_primary(
        &self,
        symbols: Vec<Symbol>,
        emit: Emitter,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        // intern pool prewarm + instId → canonical `Symbol` 사전계산.
        self.cache.prewarm(symbols.iter().map(|s| s.as_str()));
        let (inst_ids, inst_map) = build_inst_map(&symbols, &self.cache);

        let mut backoff = Backoff::new(self.cfg.backoff_base_ms, self.cfg.backoff_cap_ms);
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let attempt = backoff.attempts();
            info!(attempt, "okx connecting");

            let outcome = tokio::select! {
                _ = cancel.cancelled() => {
                    info!("okx cancelled during connect");
                    return Ok(());
                }
                r = self.run_primary_once(&inst_ids, &inst_map, &emit, &cancel) => r,
            };

            match outcome {
                Ok(()) => {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    warn!("okx disconnected cleanly, reconnecting");
                    backoff.reset();
                }
                Err(e) => {
                    warn!(error = %e, "okx error; backoff");
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
        info!(status = ?resp.status(), "okx connected");
        let (mut write, mut read) = ws.split();

        // 구독: bbo-tbt + trades 두 채널. 심볼 × 2 = `args`.
        //
        // 단일 chunk 안에 동일 instId 의 두 채널을 넣어야 순서 상 제멋대로
        // 흐트러지지 않음. `MAX_SUBSCRIBE_ARGS` 는 인자 수 기준이므로
        // symbols chunk 를 ceil(MAX / 2) 크기로 자름.
        let per_symbol: usize = 2;
        let chunk_sz = (MAX_SUBSCRIBE_ARGS / per_symbol).max(1);
        for chunk in inst_ids.chunks(chunk_sz) {
            let mut args: Vec<Value> = Vec::with_capacity(chunk.len() * per_symbol);
            for iid in chunk {
                args.push(serde_json::json!({
                    "channel": "bbo-tbt",
                    "instId": iid,
                }));
                args.push(serde_json::json!({
                    "channel": "trades",
                    "instId": iid,
                }));
            }
            let sub = serde_json::json!({
                "op": "subscribe",
                "args": args,
            });
            write.send(Message::Text(sub.to_string())).await?;
        }
        info!(
            symbols = inst_ids.len(),
            "okx subscribed bbo-tbt + trades"
        );

        let read_to = Duration::from_secs(self.cfg.read_timeout_secs);
        let mut ping_iv =
            tokio::time::interval(Duration::from_secs(self.cfg.ping_interval_secs));
        ping_iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 첫 tick 은 즉시 발화 — skip.
        ping_iv.tick().await;

        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => {
                    info!("okx cancel received");
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                _ = ping_iv.tick() => {
                    // OKX spec: text frame "ping" (WS protocol Ping 아님).
                    if let Err(e) = write.send(Message::Text("ping".into())).await {
                        warn!(error = %e, "okx ping send failed");
                        return Err(e.into());
                    }
                    continue;
                }
                res = tokio::time::timeout(read_to, read.next()) => res,
            };

            let msg = match msg {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => {
                    warn!(error = %e, "okx read error");
                    return Err(e.into());
                }
                Ok(None) => {
                    warn!("okx stream ended");
                    return Ok(());
                }
                Err(_) => {
                    warn!(
                        timeout_s = self.cfg.read_timeout_secs,
                        "okx read timeout"
                    );
                    return Err(anyhow::anyhow!("read timeout"));
                }
            };

            match msg {
                Message::Text(text) => {
                    let ws_recv = Stamp::now(&*self.clock);
                    let reply =
                        dispatch_text_bytes(text.as_bytes(), ws_recv, inst_map, &self.cache, emit);
                    if let Some(out) = reply {
                        if let Err(e) = write.send(out).await {
                            warn!(error = %e, "okx reply send failed");
                            return Err(e.into());
                        }
                    }
                }
                // OKX 가 바이너리를 보낼 일은 없지만 방어적 처리.
                Message::Binary(bytes) => {
                    let ws_recv = Stamp::now(&*self.clock);
                    let reply =
                        dispatch_text_bytes(&bytes, ws_recv, inst_map, &self.cache, emit);
                    if let Some(out) = reply {
                        if let Err(e) = write.send(out).await {
                            warn!(error = %e, "okx reply send failed");
                            return Err(e.into());
                        }
                    }
                }
                // 중간 프록시가 WS protocol Ping frame 을 보낼 수 있어 반사.
                Message::Ping(p) => {
                    if let Err(e) = write.send(Message::Pong(p)).await {
                        warn!(error = %e, "okx ws-pong send failed");
                        return Err(e.into());
                    }
                }
                Message::Pong(_) => {}
                Message::Close(frame) => {
                    warn!(?frame, "okx server close");
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

/// 수신 envelope.
///
/// - control event: `{"event":"subscribe"|"error", "code":..., "msg":..., "arg":...}`
/// - data frame: `{"arg":{...}, "data":[{...}]}`
#[derive(Debug, Deserialize)]
struct OkxFrame<'a> {
    #[serde(borrow, default)]
    event: Option<&'a str>,
    #[serde(borrow, default)]
    code: Option<&'a str>,
    #[serde(borrow, default)]
    msg: Option<&'a str>,
    #[serde(borrow, default)]
    arg: Option<OkxArg<'a>>,
    /// 채널별로 스키마가 다르므로 지연 파싱. `bbo-tbt` → `Vec<OkxBboEntry>`,
    /// `trades` → `Vec<OkxTradeEntry>` 로 각각 2차 파싱.
    #[serde(borrow, default)]
    data: Option<&'a RawValue>,
}

#[derive(Debug, Deserialize)]
struct OkxArg<'a> {
    #[serde(borrow, default)]
    channel: Option<&'a str>,
    #[serde(rename = "instId", borrow, default)]
    inst_id: Option<&'a str>,
}

/// `data[0]` — bbo-tbt entry.
#[derive(Debug, Deserialize)]
struct OkxBboEntry<'a> {
    #[serde(default)]
    bids: Vec<BookLevel>,
    #[serde(default)]
    asks: Vec<BookLevel>,
    #[serde(default, deserialize_with = "deserialize_i64_lenient_opt")]
    ts: Option<i64>,
    #[serde(borrow, default)]
    #[allow(dead_code)]
    checksum: Option<&'a str>,
    /// 일부 배포본은 seqId / prevSeqId 를 함께 보냄 — 무시.
    #[serde(default)]
    #[allow(dead_code)]
    #[serde(rename = "seqId")]
    seq_id: Option<u64>,
}

/// `data[i]` — trades channel entry. OKX 공식:
/// `{instId, tradeId, px, sz, side:"buy"|"sell", ts}`.
///
/// `px`/`sz` 는 문자열 — lenient f64 deserializer 로 숫자 fallback 수용.
/// `ts` 는 ms 단위 epoch (문자열) — lenient i64 로 수치형도 허용.
#[derive(Debug, Deserialize)]
struct OkxTradeEntry<'a> {
    #[serde(rename = "instId", borrow, default)]
    inst_id: Option<&'a str>,
    #[serde(rename = "tradeId", borrow, default)]
    trade_id: Option<&'a str>,
    #[serde(deserialize_with = "deserialize_f64_lenient")]
    px: f64,
    #[serde(deserialize_with = "deserialize_f64_lenient")]
    sz: f64,
    #[serde(borrow, default)]
    side: Option<&'a str>,
    #[serde(default, deserialize_with = "deserialize_i64_lenient_opt")]
    ts: Option<i64>,
}

/// `[price, size, lo, co]` — 앞 두 개만 사용.
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
        // f64 lenient newtype — `#[serde(deserialize_with)]` 를 array element
        // position 에 직접 달 수 없어 wrapper 로 우회.
        #[derive(Deserialize)]
        struct F(#[serde(deserialize_with = "deserialize_f64_lenient")] f64);

        struct V;
        impl<'de> de::Visitor<'de> for V {
            type Value = BookLevel;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("[price, size, ...] array")
            }
            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<BookLevel, A::Error> {
                let p: F = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &"2"))?;
                let s: F = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &"2"))?;
                // `lo`, `co` 및 그 외 후속 필드는 skip — forward-compat.
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
// Dispatch (정적 함수) — hot-path
// ────────────────────────────────────────────────────────────────────────────

/// instId (`BTC-USDT-SWAP`) → canonical `Symbol` 매핑. 구독 시 precompute.
type InstMap = HashMap<String, Symbol, ahash::RandomState>;

fn build_inst_map(symbols: &[Symbol], cache: &SymbolCache) -> (Vec<String>, InstMap) {
    let mut ids: Vec<String> = Vec::with_capacity(symbols.len());
    let mut m: InstMap =
        InstMap::with_capacity_and_hasher(symbols.len(), ahash::RandomState::new());
    for s in symbols {
        let interned = cache.intern(s.as_str());
        let iid = to_okx_inst_id(s.as_str());
        ids.push(iid.clone());
        m.insert(iid, interned);
    }
    (ids, m)
}

/// 단일 프레임 dispatch. 반환값이 `Some(Message)` 면 caller 가 write sink 에 송신.
///
/// - text `"ping"` (서버 발신) → `"pong"` 반환.
/// - text `"pong"` → 무시.
/// - JSON `event` 프레임 (`subscribe`/`error`) → 로그만, `None`.
/// - JSON data 프레임 → bookTicker emit, `None`.
fn dispatch_text_bytes(
    bytes: &[u8],
    ws_recv: Stamp,
    inst_map: &InstMap,
    cache: &SymbolCache,
    emit: &Emitter,
) -> Option<Message> {
    // ── Phase 0: text ping/pong 짧은 경로 ──────────────────────────────
    if let Some(reply) = handle_text_control(bytes) {
        return reply;
    }

    // ── Phase 1: envelope 파싱 ─────────────────────────────────────────
    let frame: OkxFrame<'_> = match serde_json::from_slice(bytes) {
        Ok(f) => f,
        Err(e) => {
            let snippet = snippet(bytes, 200);
            debug!(error = %e, snippet = %snippet, "okx non-JSON");
            return None;
        }
    };

    // ── Phase 2: control event 분기 ────────────────────────────────────
    if let Some(event) = frame.event {
        match event {
            "subscribe" => debug!(
                inst_id = ?frame.arg.as_ref().and_then(|a| a.inst_id),
                "okx subscribe confirmed"
            ),
            "error" => {
                error!(
                    code = ?frame.code,
                    msg = ?frame.msg,
                    "okx server error event"
                );
            }
            other => trace!(event = other, "okx control event"),
        }
        return None;
    }

    // ── Phase 3: data frame — channel 별 분기 ─────────────────────────
    let Some(arg) = frame.arg else {
        trace!("okx frame missing arg");
        return None;
    };
    let Some(inst_id) = arg.inst_id else {
        trace!("okx frame missing arg.instId");
        return None;
    };
    let Some(raw_data) = frame.data else {
        trace!(inst_id, "okx frame missing data");
        return None;
    };
    let channel = arg.channel.unwrap_or("");

    match channel {
        "bbo-tbt" => handle_bbo_frame(inst_id, raw_data, ws_recv, inst_map, cache, emit),
        "trades" => handle_trades_frame(inst_id, raw_data, ws_recv, inst_map, cache, emit),
        other => {
            trace!(channel = other, inst_id, "okx unhandled channel — drop");
        }
    }
    None
}

/// bbo-tbt 프레임 처리: `data[0]` 만 활용해 `BookTicker` 1건 emit.
fn handle_bbo_frame(
    inst_id: &str,
    raw_data: &RawValue,
    ws_recv: Stamp,
    inst_map: &InstMap,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    let mut entries: Vec<OkxBboEntry<'_>> = match serde_json::from_str(raw_data.get()) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, inst_id, "okx bbo data parse failed");
            return;
        }
    };
    if entries.is_empty() {
        trace!(inst_id, "okx empty bbo data array");
        return;
    }
    let entry = entries.remove(0);

    if entry.bids.is_empty() || entry.asks.is_empty() {
        trace!(inst_id, "okx empty best level");
        return;
    }

    let bid = &entry.bids[0];
    let ask = &entry.asks[0];
    let sym = resolve_symbol(inst_id, inst_map, cache);
    let server_time_ms = entry.ts.unwrap_or(0);

    let bt = BookTicker {
        exchange: ExchangeId::Okx,
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

/// trades 프레임 처리: `data[]` 원소 하나 당 `Trade` 1건 emit.
/// `side="sell"` 인 경우 size 를 음수로 부호 변환 — 레거시 규약과 동일.
fn handle_trades_frame(
    inst_id: &str,
    raw_data: &RawValue,
    ws_recv: Stamp,
    inst_map: &InstMap,
    cache: &SymbolCache,
    emit: &Emitter,
) {
    let entries: Vec<OkxTradeEntry<'_>> = match serde_json::from_str(raw_data.get()) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, inst_id, "okx trades data parse failed");
            return;
        }
    };
    if entries.is_empty() {
        trace!(inst_id, "okx empty trades data array");
        return;
    }

    // 공통 컨텍스트 — 대부분의 batch frame 은 같은 instId.
    let default_sym = resolve_symbol(inst_id, inst_map, cache);

    for e in entries {
        // entry 의 instId 가 비어있으면 arg.instId 로 fallback.
        let sym = match e.inst_id {
            Some(iid) if !iid.is_empty() && iid != inst_id => {
                resolve_symbol(iid, inst_map, cache)
            }
            _ => default_sym.clone(),
        };

        let signed_size = match e.side {
            Some("sell" | "SELL" | "Sell") => -e.sz,
            Some(_) | None => e.sz,
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
            exchange: ExchangeId::Okx,
            symbol: sym,
            price: Price(e.px),
            size: Size(signed_size),
            trade_id,
            create_time_s,
            create_time_ms,
            server_time_ms: create_time_ms,
            // OKX public trades 는 internal/external 구분 없음 → false.
            is_internal: false,
        };
        let ls = stamps_with_ws(ws_recv, create_time_ms);
        emit(MarketEvent::Trade(t), ls);
    }
}

/// OKX `tradeId` 문자열을 i64 로 변환.
///
/// 대부분의 경우 `tradeId` 는 순수 10 진수. `i64::from_str` 가 성공하면
/// 양수 보장 위해 MSB mask. 실패 시 fixed-seed ahash 로 i63 양수 해시.
#[inline]
fn parse_or_hash_trade_id(s: &str) -> i64 {
    if let Ok(n) = s.parse::<i64>() {
        return n & 0x7FFF_FFFF_FFFF_FFFF;
    }
    if let Ok(u) = s.parse::<u64>() {
        return (u & 0x7FFF_FFFF_FFFF_FFFF) as i64;
    }
    // Non-numeric fallback — fixed-seed ahash → stable across process runs.
    const K0: u64 = 0xDEAD_BEEF_CAFE_BABE;
    const K1: u64 = 0x0123_4567_89AB_CDEF;
    const K2: u64 = 0xFEDC_BA98_7654_3210;
    const K3: u64 = 0x0BAD_F00D_0C0F_FEE0;
    let state = ahash::RandomState::with_seeds(K0, K1, K2, K3);
    (state.hash_one(s) & 0x7FFF_FFFF_FFFF_FFFF) as i64
}

/// text `"ping"`/`"pong"` 프레임을 빠르게 분기. 매칭되면 reply 결정을 포함한
/// `Some(...)` 반환, 아니면 `None` (envelope 파싱으로 진행).
///
/// 반환 구조:
/// - `Some(Some(Message))`: 매칭됐고 caller 가 응답 송신 필요.
/// - `Some(None)`: 매칭됐고 응답 불필요 (예: pong 수신).
/// - `None`: 매칭되지 않음.
fn handle_text_control(bytes: &[u8]) -> Option<Option<Message>> {
    // trim ASCII whitespace.
    let mut start = 0usize;
    let mut end = bytes.len();
    while start < end && (bytes[start] as char).is_ascii_whitespace() {
        start += 1;
    }
    while end > start && (bytes[end - 1] as char).is_ascii_whitespace() {
        end -= 1;
    }
    let body = &bytes[start..end];
    if body.eq_ignore_ascii_case(b"ping") {
        // OKX 서버가 text ping 을 보내는 경우 text pong 회신.
        return Some(Some(Message::Text("pong".into())));
    }
    if body.eq_ignore_ascii_case(b"pong") {
        trace!("okx pong ack (text)");
        return Some(None);
    }
    None
}

/// instId → canonical `Symbol`. map hit 시 clone 1 회로 끝. miss 시
/// `from_okx_inst_id` + `cache.intern` fallback.
#[inline]
fn resolve_symbol(inst_id: &str, inst_map: &InstMap, cache: &SymbolCache) -> Symbol {
    if let Some(sym) = inst_map.get(inst_id) {
        return sym.clone();
    }
    let canonical = from_okx_inst_id(inst_id);
    cache.intern(&canonical)
}

/// Debug log 용 UTF-8 safe snippet.
fn snippet(bytes: &[u8], max: usize) -> String {
    let end = bytes.len().min(max);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// ────────────────────────────────────────────────────────────────────────────
// 심볼 변환
// ────────────────────────────────────────────────────────────────────────────

/// canonical `BTC_USDT` → OKX `BTC-USDT-SWAP`.
fn to_okx_inst_id(sym: &str) -> String {
    let up = sym.to_uppercase();
    if up.ends_with("-SWAP") || up.contains("-SWAP") {
        return up.replace('_', "-");
    }
    let dashed = up.replace('_', "-");
    format!("{}-SWAP", dashed)
}

/// OKX `BTC-USDT-SWAP` → canonical `BTC_USDT`.
fn from_okx_inst_id(inst_id: &str) -> String {
    let up = inst_id.to_uppercase();
    let trimmed: &str = if let Some(idx) = up.find("-SWAP") {
        &up[..idx]
    } else {
        &up
    };
    let s = trimmed.replace('-', "_");
    if s.is_empty() {
        inst_id.to_string()
    } else {
        s
    }
}

// ────────────────────────────────────────────────────────────────────────────
// type asserts
// ────────────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
fn _type_asserts() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<OkxFeed>();
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

    // ── 공통 헬퍼 ──────────────────────────────────────────────────────

    fn make_symbols() -> Vec<Symbol> {
        vec![Symbol::new("BTC_USDT"), Symbol::new("ETH_USDT")]
    }

    fn make_inst_map(cache: &SymbolCache) -> (Vec<String>, InstMap) {
        build_inst_map(&make_symbols(), cache)
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

    fn sample_bbo_bytes() -> Vec<u8> {
        serde_json::json!({
            "arg": {
                "channel": "bbo-tbt",
                "instId": "BTC-USDT-SWAP"
            },
            "data": [{
                "bids": [["40000.5", "3.2", "0", "2"]],
                "asks": [["40001.0", "1.1", "0", "1"]],
                "ts": "1700000000000"
            }]
        })
        .to_string()
        .into_bytes()
    }

    fn recv_stamp() -> Stamp {
        Stamp {
            wall_ms: 10,
            mono_ns: 10,
        }
    }

    // ── BookLevel 파싱 ────────────────────────────────────────────────

    #[test]
    fn book_level_parses_4tuple_strings() {
        let lv: BookLevel = serde_json::from_str(r#"["40000.5","3.2","0","2"]"#).unwrap();
        assert_eq!(lv.price, 40000.5);
        assert_eq!(lv.size, 3.2);
    }

    #[test]
    fn book_level_parses_numeric_pair() {
        let lv: BookLevel = serde_json::from_str("[40000.5, 3.2]").unwrap();
        assert_eq!(lv.price, 40000.5);
        assert_eq!(lv.size, 3.2);
    }

    #[test]
    fn book_level_rejects_single_element() {
        let r: Result<BookLevel, _> = serde_json::from_str("[\"1\"]");
        assert!(r.is_err());
    }

    // ── 심볼 변환 ─────────────────────────────────────────────────────

    #[test]
    fn to_okx_inst_id_appends_swap() {
        assert_eq!(to_okx_inst_id("BTC_USDT"), "BTC-USDT-SWAP");
        assert_eq!(to_okx_inst_id("sol_usdc"), "SOL-USDC-SWAP");
    }

    #[test]
    fn to_okx_is_idempotent_if_already_swap() {
        assert_eq!(to_okx_inst_id("BTC-USDT-SWAP"), "BTC-USDT-SWAP");
        assert_eq!(to_okx_inst_id("btc-usdt-swap"), "BTC-USDT-SWAP");
    }

    #[test]
    fn from_okx_strips_swap_and_dashes() {
        assert_eq!(from_okx_inst_id("BTC-USDT-SWAP"), "BTC_USDT");
        assert_eq!(from_okx_inst_id("SOL-USDC-SWAP"), "SOL_USDC");
    }

    #[test]
    fn from_okx_without_swap_still_converts() {
        assert_eq!(from_okx_inst_id("BTC-USDT"), "BTC_USDT");
    }

    #[test]
    fn symbol_roundtrip() {
        let internal = "BTC_USDT";
        let okx = to_okx_inst_id(internal);
        let back = from_okx_inst_id(&okx);
        assert_eq!(back, internal);
    }

    // ── dispatch hot-path ─────────────────────────────────────────────

    #[test]
    fn dispatch_bbo_emits_bookticker() {
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, last) = emit_counter();
        let reply = dispatch_text_bytes(&sample_bbo_bytes(), recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 1);
        let bt = last.lock().unwrap()[0].clone();
        assert_eq!(bt.exchange, ExchangeId::Okx);
        assert_eq!(bt.symbol.as_str(), "BTC_USDT");
        assert_eq!(bt.bid_price.0, 40000.5);
        assert_eq!(bt.ask_price.0, 40001.0);
        assert_eq!(bt.bid_size.0, 3.2);
        assert_eq!(bt.ask_size.0, 1.1);
        assert_eq!(bt.server_time_ms, 1_700_000_000_000);
        assert!(bt.is_valid());
    }

    #[test]
    fn dispatch_numeric_ts_and_levels() {
        let bytes = serde_json::json!({
            "arg": {"channel":"bbo-tbt","instId":"BTC-USDT-SWAP"},
            "data": [{
                "bids": [[40000.5, 3.2]],
                "asks": [[40001.0, 1.1]],
                "ts": 1_700_000_000_001_i64
            }]
        })
        .to_string()
        .into_bytes();

        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, last) = emit_counter();
        let reply = dispatch_text_bytes(&bytes, recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 1);
        let bt = &last.lock().unwrap()[0];
        assert_eq!(bt.server_time_ms, 1_700_000_000_001);
        assert_eq!(bt.bid_price.0, 40000.5);
    }

    #[test]
    fn dispatch_unknown_inst_id_falls_back_to_intern() {
        // map 에 없는 inst_id → from_okx_inst_id + intern.
        let bytes = serde_json::json!({
            "arg": {"channel":"bbo-tbt","instId":"ETH-USDT-SWAP"},
            "data": [{
                "bids": [["2000.0","10","0","1"]],
                "asks": [["2001.0","5","0","1"]],
                "ts": "1"
            }]
        })
        .to_string()
        .into_bytes();
        let cache = Arc::new(SymbolCache::new());
        let empty = InstMap::with_hasher(ahash::RandomState::new());
        let (emit, cnt, last) = emit_counter();
        let reply = dispatch_text_bytes(&bytes, recv_stamp(), &empty, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 1);
        assert_eq!(last.lock().unwrap()[0].symbol.as_str(), "ETH_USDT");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn dispatch_reuses_cached_symbol_across_frames() {
        // 동일 inst_id 가 여러 프레임에 걸쳐 나와도 같은 `Arc<str>` 포인터.
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, _, last) = emit_counter();
        dispatch_text_bytes(&sample_bbo_bytes(), recv_stamp(), &map, &cache, &emit);
        dispatch_text_bytes(&sample_bbo_bytes(), recv_stamp(), &map, &cache, &emit);
        let bts = last.lock().unwrap();
        assert_eq!(bts.len(), 2);
        assert!(
            bts[0].symbol.ptr_eq(&bts[1].symbol),
            "symbol Arc must be identical across frames"
        );
    }

    #[test]
    fn dispatch_empty_bids_drops() {
        let bytes = serde_json::json!({
            "arg": {"channel":"bbo-tbt","instId":"BTC-USDT-SWAP"},
            "data": [{
                "bids": [],
                "asks": [["40001.0","1.1","0","1"]],
                "ts": "1"
            }]
        })
        .to_string()
        .into_bytes();
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(&bytes, recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_missing_arg_drops() {
        let bytes = br#"{"data":[{"bids":[["1","1"]],"asks":[["2","1"]],"ts":"1"}]}"#;
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_missing_data_drops() {
        let bytes = br#"{"arg":{"channel":"bbo-tbt","instId":"BTC-USDT-SWAP"}}"#;
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_empty_data_array_drops() {
        let bytes = br#"{"arg":{"channel":"bbo-tbt","instId":"BTC-USDT-SWAP"},"data":[]}"#;
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_malformed_json_drops() {
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(b"not json", recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    // ── control events ────────────────────────────────────────────────

    #[test]
    fn dispatch_subscribe_ack_is_silent() {
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let bytes =
            br#"{"event":"subscribe","arg":{"channel":"bbo-tbt","instId":"BTC-USDT-SWAP"}}"#;
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_error_event_is_silent() {
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let bytes = br#"{"event":"error","code":"60012","msg":"invalid channel"}"#;
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    // ── text ping / pong ───────────────────────────────────────────────

    #[test]
    fn dispatch_text_ping_replies_with_pong() {
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(b"ping", recv_stamp(), &map, &cache, &emit);
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
        match reply {
            Some(Message::Text(s)) => assert_eq!(s, "pong"),
            other => panic!("expected text pong reply, got {:?}", other),
        }
    }

    #[test]
    fn dispatch_text_ping_case_insensitive() {
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(b"  PING  ", recv_stamp(), &map, &cache, &emit);
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
        match reply {
            Some(Message::Text(s)) => assert_eq!(s, "pong"),
            other => panic!("expected text pong reply, got {:?}", other),
        }
    }

    #[test]
    fn dispatch_text_pong_no_reply() {
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_counter();
        let reply = dispatch_text_bytes(b"pong", recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);

        let reply = dispatch_text_bytes(b"PONG", recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
    }

    // ── build_inst_map ────────────────────────────────────────────────

    #[test]
    fn build_inst_map_populates_ids_and_map() {
        let cache = Arc::new(SymbolCache::new());
        let (ids, map) = build_inst_map(
            &[Symbol::new("BTC_USDT"), Symbol::new("SOL_USDC")],
            &cache,
        );
        assert_eq!(ids, vec!["BTC-USDT-SWAP", "SOL-USDC-SWAP"]);
        assert_eq!(
            map.get("BTC-USDT-SWAP").map(|s| s.as_str()),
            Some("BTC_USDT")
        );
        assert_eq!(
            map.get("SOL-USDC-SWAP").map(|s| s.as_str()),
            Some("SOL_USDC")
        );
    }

    // ── OkxFeed wiring ────────────────────────────────────────────────

    #[test]
    fn okx_config_default_sane() {
        let c = OkxConfig::default();
        assert!(c.ws_url.starts_with("wss://"));
        assert_eq!(c.inst_type, "SWAP");
        assert_eq!(c.settle_ccy, "USDT");
        assert!(c.ping_interval_secs >= 10);
        assert!(c.backoff_base_ms < c.backoff_cap_ms);
        assert!(c.scratch_capacity >= 512);
    }

    #[test]
    fn okx_feed_id_and_role() {
        let f = OkxFeed::new(OkxConfig::default());
        assert_eq!(f.id(), ExchangeId::Okx);
        assert_eq!(f.role(), DataRole::Primary);
        assert!(f.label().to_lowercase().contains("okx"));
    }

    #[test]
    fn okx_feed_is_trait_object() {
        let _boxed: Arc<dyn ExchangeFeed> = Arc::new(OkxFeed::new(OkxConfig::default()));
    }

    #[test]
    fn okx_feed_with_clock_uses_injected_clock() {
        let mock = MockClock::new(42, 7);
        let clock: Arc<dyn Clock> = mock.clone();
        let f = OkxFeed::with_clock(OkxConfig::default(), clock.clone());
        let s = Stamp::now(f.clock().as_ref());
        assert_eq!(s.wall_ms, 42);
    }

    #[test]
    fn okx_feed_with_cache_shares_pool() {
        let shared = Arc::new(SymbolCache::new());
        let f1 = OkxFeed::with_cache(OkxConfig::default(), shared.clone());
        let f2 = OkxFeed::with_cache(OkxConfig::default(), shared.clone());
        let a = f1.symbol_cache().intern("BTC_USDT");
        let b = f2.symbol_cache().intern("BTC_USDT");
        assert!(a.ptr_eq(&b), "cache must be shared between feed instances");
    }

    #[test]
    fn okx_feed_with_clock_and_cache_uses_both() {
        let mock = MockClock::new(100, 3);
        let clock: Arc<dyn Clock> = mock.clone();
        let shared = Arc::new(SymbolCache::new());
        let f = OkxFeed::with_clock_and_cache(
            OkxConfig::default(),
            clock.clone(),
            shared.clone(),
        );
        let s = Stamp::now(f.clock().as_ref());
        assert_eq!(s.wall_ms, 100);
        assert!(Arc::ptr_eq(f.symbol_cache(), &shared));
    }

    #[tokio::test]
    async fn stream_empty_symbols_is_no_op() {
        let feed = OkxFeed::new(OkxConfig::default());
        let cancel = CancellationToken::new();
        let emit = hft_exchange_api::noop_emitter();
        feed.stream(&[], emit, cancel).await.expect("stream no-op");
    }

    #[tokio::test]
    async fn stream_all_empty_string_symbols_is_no_op() {
        let feed = OkxFeed::new(OkxConfig::default());
        let cancel = CancellationToken::new();
        let emit = hft_exchange_api::noop_emitter();
        feed.stream(&[Symbol::new(""), Symbol::new("")], emit, cancel)
            .await
            .expect("stream no-op");
    }

    // ── latency stamps ────────────────────────────────────────────────

    #[test]
    fn stamps_captures_both_recv_and_server() {
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let stamps = Arc::new(Mutex::new(Option::<hft_time::LatencyStamps>::None));
        let stamps_c = stamps.clone();
        let emit: Emitter = Arc::new(move |_ev, ls| {
            *stamps_c.lock().unwrap() = Some(ls);
        });
        let ws = Stamp {
            wall_ms: 500,
            mono_ns: 12345,
        };
        let _ = dispatch_text_bytes(&sample_bbo_bytes(), ws, &map, &cache, &emit);
        let got = stamps.lock().unwrap().unwrap();
        assert_eq!(got.ws_received.wall_ms, 500);
        assert_eq!(got.exchange_server.wall_ms, 1_700_000_000_000);
        assert_eq!(got.serialized.mono_ns, 0);
    }

    // ── trades channel ────────────────────────────────────────────────

    fn emit_trade_counter() -> (Emitter, Arc<AtomicUsize>, Arc<Mutex<Vec<Trade>>>) {
        let cnt = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(Mutex::new(Vec::<Trade>::new()));
        let cnt_c = cnt.clone();
        let last_c = last.clone();
        let emit: Emitter = Arc::new(move |ev, _ls| {
            if let MarketEvent::Trade(t) = ev {
                cnt_c.fetch_add(1, Ordering::SeqCst);
                last_c.lock().unwrap().push(t);
            }
        });
        (emit, cnt, last)
    }

    fn sample_trade_bytes_buy() -> Vec<u8> {
        serde_json::json!({
            "arg": {"channel":"trades","instId":"BTC-USDT-SWAP"},
            "data": [{
                "instId":"BTC-USDT-SWAP",
                "tradeId":"130639474",
                "px":"42219.9",
                "sz":"0.12",
                "side":"buy",
                "ts":"1629386781174"
            }]
        })
        .to_string()
        .into_bytes()
    }

    fn sample_trade_bytes_sell() -> Vec<u8> {
        serde_json::json!({
            "arg": {"channel":"trades","instId":"BTC-USDT-SWAP"},
            "data": [{
                "instId":"BTC-USDT-SWAP",
                "tradeId":"130639475",
                "px":"42219.9",
                "sz":"0.5",
                "side":"sell",
                "ts":"1629386781175"
            }]
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn dispatch_trade_buy_emits_positive_size() {
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, last) = emit_trade_counter();
        let reply = dispatch_text_bytes(&sample_trade_bytes_buy(), recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 1);
        let t = &last.lock().unwrap()[0];
        assert_eq!(t.exchange, ExchangeId::Okx);
        assert_eq!(t.symbol.as_str(), "BTC_USDT");
        assert_eq!(t.price.0, 42219.9);
        assert_eq!(t.size.0, 0.12);
        assert_eq!(t.trade_id, 130_639_474_i64);
        assert_eq!(t.create_time_ms, 1_629_386_781_174);
        assert_eq!(t.create_time_s, 1_629_386_781);
        assert_eq!(t.server_time_ms, 1_629_386_781_174);
        assert!(!t.is_internal);
    }

    #[test]
    fn dispatch_trade_sell_negates_size() {
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, last) = emit_trade_counter();
        let reply = dispatch_text_bytes(&sample_trade_bytes_sell(), recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 1);
        let t = &last.lock().unwrap()[0];
        assert_eq!(t.size.0, -0.5);
    }

    #[test]
    fn dispatch_trade_batch_emits_each_entry() {
        // 배열 여러 원소 → 각각 Trade emit.
        let bytes = serde_json::json!({
            "arg": {"channel":"trades","instId":"BTC-USDT-SWAP"},
            "data": [
                {"instId":"BTC-USDT-SWAP","tradeId":"1","px":"100","sz":"1","side":"buy","ts":"100"},
                {"instId":"BTC-USDT-SWAP","tradeId":"2","px":"101","sz":"2","side":"sell","ts":"101"},
                {"instId":"BTC-USDT-SWAP","tradeId":"3","px":"102","sz":"3","side":"buy","ts":"102"}
            ]
        })
        .to_string()
        .into_bytes();

        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, last) = emit_trade_counter();
        let reply = dispatch_text_bytes(&bytes, recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 3);
        let ts = last.lock().unwrap();
        assert_eq!(ts[0].trade_id, 1);
        assert_eq!(ts[0].size.0, 1.0);
        assert_eq!(ts[1].trade_id, 2);
        assert_eq!(ts[1].size.0, -2.0);
        assert_eq!(ts[2].trade_id, 3);
        assert_eq!(ts[2].size.0, 3.0);
    }

    #[test]
    fn dispatch_trade_empty_data_is_silent() {
        let bytes = br#"{"arg":{"channel":"trades","instId":"BTC-USDT-SWAP"},"data":[]}"#;
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_trade_counter();
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_trade_missing_side_defaults_positive() {
        let bytes = serde_json::json!({
            "arg": {"channel":"trades","instId":"BTC-USDT-SWAP"},
            "data": [{
                "instId":"BTC-USDT-SWAP","tradeId":"7",
                "px":"100","sz":"2","ts":"1"
            }]
        })
        .to_string()
        .into_bytes();
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, _, last) = emit_trade_counter();
        dispatch_text_bytes(&bytes, recv_stamp(), &map, &cache, &emit);
        let t = &last.lock().unwrap()[0];
        assert_eq!(t.size.0, 2.0);
    }

    #[test]
    fn dispatch_trade_unknown_side_defaults_positive() {
        let bytes = serde_json::json!({
            "arg": {"channel":"trades","instId":"BTC-USDT-SWAP"},
            "data": [{
                "instId":"BTC-USDT-SWAP","tradeId":"7",
                "px":"100","sz":"2","side":"xxx","ts":"1"
            }]
        })
        .to_string()
        .into_bytes();
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, _, last) = emit_trade_counter();
        dispatch_text_bytes(&bytes, recv_stamp(), &map, &cache, &emit);
        let t = &last.lock().unwrap()[0];
        assert_eq!(t.size.0, 2.0);
    }

    #[test]
    fn dispatch_trade_numeric_px_sz_accepted() {
        // 숫자형 px/sz (문자열 아님) — lenient f64 로 수용.
        let bytes = serde_json::json!({
            "arg": {"channel":"trades","instId":"BTC-USDT-SWAP"},
            "data": [{
                "instId":"BTC-USDT-SWAP","tradeId":"8",
                "px": 42000.0,"sz": 1.5,"side":"buy","ts": 10_i64
            }]
        })
        .to_string()
        .into_bytes();
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, _, last) = emit_trade_counter();
        dispatch_text_bytes(&bytes, recv_stamp(), &map, &cache, &emit);
        let t = &last.lock().unwrap()[0];
        assert_eq!(t.price.0, 42000.0);
        assert_eq!(t.size.0, 1.5);
        assert_eq!(t.create_time_ms, 10);
    }

    #[test]
    fn dispatch_trade_non_numeric_trade_id_hashed_stable() {
        // 숫자가 아닌 tradeId — 해시 fallback. 같은 문자열 → 같은 값.
        let a = parse_or_hash_trade_id("abc-uuid-1");
        let b = parse_or_hash_trade_id("abc-uuid-1");
        assert_eq!(a, b);
        assert!(a >= 0);

        let c = parse_or_hash_trade_id("different");
        assert_ne!(a, c);
    }

    #[test]
    fn dispatch_trade_empty_trade_id_defaults_zero() {
        let bytes = serde_json::json!({
            "arg": {"channel":"trades","instId":"BTC-USDT-SWAP"},
            "data": [{
                "instId":"BTC-USDT-SWAP","tradeId":"",
                "px":"1","sz":"1","side":"buy","ts":"1"
            }]
        })
        .to_string()
        .into_bytes();
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, _, last) = emit_trade_counter();
        dispatch_text_bytes(&bytes, recv_stamp(), &map, &cache, &emit);
        let t = &last.lock().unwrap()[0];
        assert_eq!(t.trade_id, 0);
    }

    #[test]
    fn dispatch_trade_unknown_inst_id_falls_back_to_intern() {
        // arg.instId 가 map 에 없으면 from_okx_inst_id + intern.
        let bytes = serde_json::json!({
            "arg": {"channel":"trades","instId":"SOL-USDC-SWAP"},
            "data": [{
                "instId":"SOL-USDC-SWAP","tradeId":"10",
                "px":"50","sz":"1","side":"buy","ts":"1"
            }]
        })
        .to_string()
        .into_bytes();
        let cache = Arc::new(SymbolCache::new());
        let empty = InstMap::with_hasher(ahash::RandomState::new());
        let (emit, cnt, last) = emit_trade_counter();
        let reply = dispatch_text_bytes(&bytes, recv_stamp(), &empty, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 1);
        assert_eq!(last.lock().unwrap()[0].symbol.as_str(), "SOL_USDC");
    }

    #[test]
    fn dispatch_unknown_channel_drops_silently() {
        // 미지원 채널 (예: 'books') → 무시, emit 없음.
        let bytes = br#"{"arg":{"channel":"books","instId":"BTC-USDT-SWAP"},"data":[{"bids":[],"asks":[]}]}"#;
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_trade_counter();
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dispatch_trade_malformed_entry_skips_batch_safely() {
        // 배열 원소 하나가 schema 위반이면 serde 가 전체 배열 파싱 실패 →
        // handle_trades_frame 이 debug log 만 남기고 return. emit 0 건.
        let bytes = br#"{"arg":{"channel":"trades","instId":"BTC-USDT-SWAP"},"data":[{"oops":true}]}"#;
        let cache = Arc::new(SymbolCache::new());
        let (_, map) = make_inst_map(&cache);
        let (emit, cnt, _) = emit_trade_counter();
        let reply = dispatch_text_bytes(bytes, recv_stamp(), &map, &cache, &emit);
        assert!(reply.is_none());
        assert_eq!(cnt.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn parse_or_hash_trade_id_overflow_masks_positive() {
        // u64::MAX 는 i64 파싱은 실패하지만 u64 파싱 후 msb mask 로 양수화.
        let v = parse_or_hash_trade_id("18446744073709551615"); // u64::MAX
        assert!(v >= 0);
        assert_eq!(v, i64::MAX);
    }
}

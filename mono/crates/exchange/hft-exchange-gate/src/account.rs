//! Gate.io Futures (USDT) 계정 조회 REST 클라이언트.
//!
//! [`GateExecutor`](crate::executor::GateExecutor) 이 주문 발행(POST/DELETE)을 담당한다면,
//! 본 모듈은 전략 runtime 이 요구하는 **정적 메타/포지션 스냅샷 조회** 를 담당한다.
//!
//! ## 구현 엔드포인트
//! - `GET /api/v4/futures/{settle}/contracts`         — public. 심볼별 quanto_multiplier /
//!   order_price_round / order_size_min / risk_limit 등.
//! - `GET /api/v4/futures/{settle}/contracts/{name}`  — 단일 심볼 refresh (옵션).
//! - `GET /api/v4/futures/{settle}/positions`         — **private (signed)**. 현재 계정
//!   모든 비영 포지션.
//! - `GET /api/v4/futures/{settle}/accounts`          — **private (signed)**. 지갑 잔고
//!   (total / unrealized_pnl) 조회. 전략 `set_account_balance` 에 피드.
//!
//! ## 서명
//! `executor::GateExecutor::canonical_sign_string` 와 동일한 v4 스펙을 **재사용** 하기 위해
//! 여기도 동일한 helper 를 copy 하지 않고 [`hft_exchange_rest`] 의 HMAC/HASH 프리미티브를
//! 직접 쓴다 (public endpoint 는 no-sign, private 은 copy-by-value 최소화).
//!
//! ## 성능 / 안전 원칙
//! 1. 모든 public 함수는 **async** 이며 `reqwest::Client` (hft-exchange-rest 의
//!    `RestClient`) 를 재사용 — connection pool 로 TCP/TLS warmup cost 분산.
//! 2. 파싱은 Gate 의 string/number 혼재를 흡수하기 위해 `StringOrF64` / `StringOrI64`
//!    visitor 헬퍼 사용 — 레거시 `gate_hft_rust` 의 "`f64::from_str(field.as_str())`" 패턴을
//!    serde 레벨로 올려 매 호출마다 String alloc 제거.
//! 3. `AccountPoller` 는 단일 tokio task 로 **contracts → positions → accounts** 순서로
//!    refresh, 실패 시 직전 캐시를 유지 (`PositionCache::refresh` / `SymbolMetaCache::refresh`
//!    의 Err 경로 정책과 동일).
//! 4. 심볼 카디널리티가 큰 경우 (Gate USDT futures 현재 ~500+) 에도 contracts 덤프는
//!    ~200KB 로 작아 5s 주기 폴링 cost 무시 가능. 포지션 엔드포인트는 active 포지션만
//!    반환되므로 보통 수십 row 이하.
//!
//! ## 예시
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use std::time::Duration;
//! # use hft_exchange_rest::{Credentials, RestClient};
//! # use hft_strategy_runtime::{PositionCache, SymbolMetaCache, AccountMembership};
//! # async fn demo() -> anyhow::Result<()> {
//! let http = RestClient::new()?;
//! let creds = Credentials::new("key", "secret");
//! let account = hft_exchange_gate::account::GateAccountClient::new(creds, http);
//!
//! // 캐시 구성
//! let meta = Arc::new(SymbolMetaCache::new());
//! let positions = Arc::new(PositionCache::new());
//!
//! // 5s 주기 폴러 spawn — drop 시 cancel.
//! let poller = hft_exchange_gate::account::AccountPoller::builder(account)
//!     .meta(meta.clone())
//!     .positions(positions.clone())
//!     .meta_period(Duration::from_secs(60))
//!     .positions_period(Duration::from_secs(1))
//!     .accounts_period(Duration::from_secs(5))
//!     .spawn();
//! # Ok(()) }
//! ```

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ahash::AHashMap;
use anyhow::anyhow;
use async_trait::async_trait;
use hft_exchange_api::ApiError;
use hft_exchange_rest::{
    headers_from_pairs, hmac_sha512_hex, now_epoch_ms, now_epoch_s, sha512_hex, Credentials,
    RestClient,
};
use hft_strategy_runtime::{
    ContractMeta, PositionCache, PositionProvider, PositionSnapshot, SymbolMetaCache,
    SymbolMetaProvider, SymbolPosition,
};
use hft_telemetry::{counter_inc, CounterKey};
use hft_types::Symbol;
use reqwest::Method;
use serde::{Deserialize, Deserializer};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

// ────────────────────────────────────────────────────────────────────────────
// Config
// ────────────────────────────────────────────────────────────────────────────

/// 계정 클라이언트 구성. 기본은 USDT perpetual + api.gateio.ws.
#[derive(Debug, Clone)]
pub struct GateAccountConfig {
    /// REST base (e.g. `https://api.gateio.ws`). trailing slash 없음.
    pub rest_base: String,
    /// settle currency — `usdt` 또는 `btc`. 기본 `usdt`.
    pub settle: String,
    /// private endpoint 호출 시 서명용 timestamp(초) 과 서버 허용 편차. Gate 는 60s.
    /// 시스템 시계가 ±30s 이상 드리프트 되어있으면 네트워크에서 거부되므로 observability 용.
    pub timestamp_tolerance_s: i64,
}

impl Default for GateAccountConfig {
    fn default() -> Self {
        Self {
            rest_base: "https://api.gateio.ws".into(),
            settle: "usdt".into(),
            timestamp_tolerance_s: 60,
        }
    }
}

impl GateAccountConfig {
    /// `/api/v4/futures/{settle}` prefix 를 조립.
    #[inline]
    fn futures_prefix(&self) -> String {
        format!("/api/v4/futures/{}", self.settle)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 클라이언트
// ────────────────────────────────────────────────────────────────────────────

/// Gate Futures 계정/메타 조회 클라이언트.
///
/// `GateExecutor` 와 동일한 `RestClient` 를 **공유** 하는 것을 권장 (connection pool
/// warmup 재사용). 내부 모두 cheap-clone 이라 여러 Poller 로 복제해도 비용 미미.
#[derive(Debug, Clone)]
pub struct GateAccountClient {
    creds: Credentials,
    http: RestClient,
    cfg: GateAccountConfig,
}

impl GateAccountClient {
    /// 기본 구성 (USDT). `Credentials::validate` 는 private endpoint 호출 시 inline 검사.
    pub fn new(creds: Credentials, http: RestClient) -> Self {
        Self {
            creds,
            http,
            cfg: GateAccountConfig::default(),
        }
    }

    pub fn with_config(mut self, cfg: GateAccountConfig) -> Self {
        self.cfg = cfg;
        self
    }

    pub fn config(&self) -> &GateAccountConfig {
        &self.cfg
    }

    /// 공개 (unsigned) GET.
    async fn get_public<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, ApiError> {
        let url = format!("{}{}", self.cfg.rest_base, path);
        let headers = reqwest::header::HeaderMap::new();
        let resp = self.http.send(Method::GET, &url, &headers, None).await?;
        let body = resp.into_ok_body()?;
        serde_json::from_str::<T>(&body).map_err(|e| {
            ApiError::Decode(format!("{path} body: {e} / raw={}", truncate(&body, 256)))
        })
    }

    /// 서명 GET. Gate v4 canonical string == executor 와 동일.
    async fn get_private<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &str,
    ) -> Result<T, ApiError> {
        self.creds.validate()?;
        let ts = now_epoch_s();
        let body_hash = sha512_hex(&[]);
        // canonical: METHOD\nPATH\nQUERY\nBODY_HASH\nTS
        let sign_str = format!("GET\n{path}\n{query}\n{body_hash}\n{ts}");
        let sign = hmac_sha512_hex(self.creds.api_secret.as_bytes(), sign_str.as_bytes());

        let url = if query.is_empty() {
            format!("{}{}", self.cfg.rest_base, path)
        } else {
            format!("{}{}?{}", self.cfg.rest_base, path, query)
        };
        let headers = headers_from_pairs([
            ("KEY", self.creds.api_key.as_ref()),
            ("SIGN", sign.as_str()),
            ("Timestamp", ts.to_string().as_str()),
        ])?;

        let resp = self.http.send(Method::GET, &url, &headers, None).await?;
        let body = resp.into_ok_body()?;
        serde_json::from_str::<T>(&body).map_err(|e| {
            ApiError::Decode(format!("{path} body: {e} / raw={}", truncate(&body, 256)))
        })
    }

    // ── contracts ─────────────────────────────────────────────────────────

    /// 모든 심볼 contract meta 를 한번에 fetch.
    pub async fn fetch_contracts(&self) -> Result<Vec<(Symbol, ContractMeta)>, ApiError> {
        let path = format!("{}/contracts", self.cfg.futures_prefix());
        let rows: Vec<GateContractRow> = self.get_public(&path).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            if r.name.is_empty() {
                continue;
            }
            out.push((Symbol::new(&r.name), r.into_meta()));
        }
        debug!(target: "gate::account", contracts = out.len(), "contracts refreshed");
        Ok(out)
    }

    /// 한 심볼의 contract meta.
    pub async fn fetch_contract(&self, symbol: &str) -> Result<ContractMeta, ApiError> {
        let path = format!("{}/contracts/{}", self.cfg.futures_prefix(), symbol);
        let row: GateContractRow = self.get_public(&path).await?;
        Ok(row.into_meta())
    }

    // ── positions ─────────────────────────────────────────────────────────

    /// 계정의 모든 active 포지션.
    pub async fn fetch_positions(&self) -> Result<PositionSnapshot, ApiError> {
        let path = format!("{}/positions", self.cfg.futures_prefix());
        let rows: Vec<GatePositionRow> = self.get_private(&path, "").await?;
        let snap = positions_to_snapshot(rows);
        debug!(
            target: "gate::account",
            long = snap.total_long_usdt,
            short = snap.total_short_usdt,
            symbols = snap.by_symbol.len(),
            "positions refreshed"
        );
        Ok(snap)
    }

    // ── accounts (wallet) ─────────────────────────────────────────────────

    /// 계정 잔고 (total USDT 환산 + unrealized PnL) 조회.
    pub async fn fetch_accounts(&self) -> Result<AccountBalance, ApiError> {
        let path = format!("{}/accounts", self.cfg.futures_prefix());
        let row: GateAccountsRow = self.get_private(&path, "").await?;
        Ok(row.into_balance())
    }
}

/// 지갑 balance 스냅샷 — 전략 `set_account_balance` 에 연결.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AccountBalance {
    /// 전체 지갑 USDT 환산 (Gate `total`).
    pub total_usdt: f64,
    /// Unrealized PnL (Gate `unrealised_pnl`).
    pub unrealized_pnl_usdt: f64,
    /// available margin (참고용).
    pub available_usdt: f64,
}

// ────────────────────────────────────────────────────────────────────────────
// provider trait impls
// ────────────────────────────────────────────────────────────────────────────

#[async_trait]
impl SymbolMetaProvider for GateAccountClient {
    async fn fetch_all(&self) -> anyhow::Result<Vec<(Symbol, ContractMeta)>> {
        self.fetch_contracts()
            .await
            .map_err(|e| anyhow!("gate contracts fetch: {e}"))
    }
}

#[async_trait]
impl PositionProvider for GateAccountClient {
    async fn fetch(&self) -> anyhow::Result<PositionSnapshot> {
        self.fetch_positions()
            .await
            .map_err(|e| anyhow!("gate positions fetch: {e}"))
    }
}

// ────────────────────────────────────────────────────────────────────────────
// AccountPoller — 3개 캐시를 주기적으로 refresh 하는 단일 tokio task
// ────────────────────────────────────────────────────────────────────────────

/// 실행 중 `AccountPoller` 에 대한 핸들 — drop 시 task 를 graceful shutdown.
///
/// task 는 tokio `CancellationToken` 으로 중단된다. `PollerHandle::shutdown().await` 는
/// 즉시 cancel 을 요청하고 task join 까지 기다린다.
pub struct PollerHandle {
    /// `Option` 으로 감싸 `shutdown(self)` 에서 `.take()` 로 ownership 이전 가능하게 함
    /// (`Drop` 구현 때문에 field move-out 이 금지됨 — E0509 회피).
    task: Option<JoinHandle<()>>,
    cancel: hft_exchange_api::CancellationToken,
    stats: Arc<PollerStats>,
}

impl PollerHandle {
    /// cancel 신호 + join 대기.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    /// 외부에서 cancel 만 요청 (task 는 계속 live — drop 시 abort).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// 통계 핸들 (주로 telemetry / e2e 테스트).
    pub fn stats(&self) -> Arc<PollerStats> {
        self.stats.clone()
    }
}

impl Drop for PollerHandle {
    fn drop(&mut self) {
        if !self.cancel.is_cancelled() {
            self.cancel.cancel();
        }
        // shutdown(self) 이 이미 join 했다면 `task` 는 None. 그 외에는 abort.
        if let Some(task) = self.task.as_ref() {
            task.abort();
        }
    }
}

/// observability counter — refresh 성공/실패 수, 마지막 업데이트 epoch ms.
#[derive(Debug, Default)]
pub struct PollerStats {
    pub meta_refreshes_ok: AtomicU64,
    pub meta_refreshes_err: AtomicU64,
    pub position_refreshes_ok: AtomicU64,
    pub position_refreshes_err: AtomicU64,
    pub account_refreshes_ok: AtomicU64,
    pub account_refreshes_err: AtomicU64,
    /// 마지막 meta refresh 성공 시각 (epoch ms). 0 = 미성공.
    pub last_meta_ok_ms: AtomicI64,
    /// 마지막 position refresh 성공 시각 (epoch ms). 0 = 미성공.
    pub last_position_ok_ms: AtomicI64,
    /// 마지막 account refresh 성공 시각 (epoch ms). 0 = 미성공.
    pub last_account_ok_ms: AtomicI64,
}

/// `AccountBalance` 를 외부 (주로 strategy) 가 구독할 수 있는 ArcSwap 슬롯.
///
/// strategy hot path 는 1 tick 당 1회 로드 후 `set_account_balance` 로 반영 → 락 경합 없음.
pub type BalanceSlot = arc_swap::ArcSwap<AccountBalance>;

/// 폴러 빌더.
pub struct AccountPollerBuilder {
    client: GateAccountClient,
    meta: Option<Arc<SymbolMetaCache>>,
    positions: Option<Arc<PositionCache>>,
    balance: Option<Arc<BalanceSlot>>,
    meta_period: Duration,
    positions_period: Duration,
    accounts_period: Duration,
    /// 처음 실행 시점에 한번 blocking refresh 를 기다릴지 여부 (startup warmup).
    warm_start: bool,
}

impl AccountPollerBuilder {
    /// meta (contract) 캐시 주입. 없으면 폴링 스킵.
    pub fn meta(mut self, c: Arc<SymbolMetaCache>) -> Self {
        self.meta = Some(c);
        self
    }
    /// position 캐시 주입. 없으면 폴링 스킵.
    pub fn positions(mut self, c: Arc<PositionCache>) -> Self {
        self.positions = Some(c);
        self
    }
    /// 지갑 balance 슬롯 주입. 없으면 accounts 엔드포인트 skip.
    pub fn balance_slot(mut self, slot: Arc<BalanceSlot>) -> Self {
        self.balance = Some(slot);
        self
    }
    /// contracts refresh 간격. 기본 60s (정적 메타는 변경 빈도 낮음).
    pub fn meta_period(mut self, d: Duration) -> Self {
        self.meta_period = d;
        self
    }
    /// positions refresh 간격. 기본 1s.
    pub fn positions_period(mut self, d: Duration) -> Self {
        self.positions_period = d;
        self
    }
    /// accounts (wallet) refresh 간격. 기본 5s.
    pub fn accounts_period(mut self, d: Duration) -> Self {
        self.accounts_period = d;
        self
    }
    /// startup 시 contracts + positions 를 한번 blocking 으로 호출해 warmup.
    pub fn warm_start(mut self, on: bool) -> Self {
        self.warm_start = on;
        self
    }

    /// 구성 검증. 최소 하나의 캐시/슬롯은 연결돼야 함.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.meta.is_none() && self.positions.is_none() && self.balance.is_none() {
            return Err(anyhow!(
                "AccountPoller: at least one of meta/positions/balance must be attached"
            ));
        }
        if self.meta_period.is_zero()
            || self.positions_period.is_zero()
            || self.accounts_period.is_zero()
        {
            return Err(anyhow!("AccountPoller: all periods must be > 0"));
        }
        Ok(())
    }

    /// tokio task spawn → `PollerHandle` 반환. 호출자가 drop 하면 task 도 취소.
    pub fn spawn(self) -> anyhow::Result<PollerHandle> {
        self.validate()?;
        let cancel = hft_exchange_api::CancellationToken::new();
        let task_cancel = cancel.child_token();
        let stats = Arc::new(PollerStats::default());
        let stats_for_task = stats.clone();

        let runner = PollerRunner {
            client: self.client,
            meta: self.meta,
            positions: self.positions,
            balance: self.balance,
            meta_period: self.meta_period,
            positions_period: self.positions_period,
            accounts_period: self.accounts_period,
            warm_start: self.warm_start,
            stats: stats_for_task,
        };

        let task = tokio::spawn(async move { runner.run(task_cancel).await });
        Ok(PollerHandle {
            task: Some(task),
            cancel,
            stats,
        })
    }
}

/// 빌더 entry — 주 진입점.
pub struct AccountPoller;

impl AccountPoller {
    /// 새 빌더. client 외에는 모두 default + None.
    pub fn builder(client: GateAccountClient) -> AccountPollerBuilder {
        AccountPollerBuilder {
            client,
            meta: None,
            positions: None,
            balance: None,
            meta_period: Duration::from_secs(60),
            positions_period: Duration::from_secs(1),
            accounts_period: Duration::from_secs(5),
            warm_start: true,
        }
    }
}

/// task 내부 구성. spawn 후엔 외부에서 볼 수 없음.
struct PollerRunner {
    client: GateAccountClient,
    meta: Option<Arc<SymbolMetaCache>>,
    positions: Option<Arc<PositionCache>>,
    balance: Option<Arc<BalanceSlot>>,
    meta_period: Duration,
    positions_period: Duration,
    accounts_period: Duration,
    warm_start: bool,
    stats: Arc<PollerStats>,
}

impl PollerRunner {
    async fn run(self, cancel: hft_exchange_api::CancellationToken) {
        // ── warmup: 켜져 있으면 cancel 이전에 첫 샘플을 시도 ──
        if self.warm_start {
            if let Some(m) = &self.meta {
                self.refresh_meta_once(m).await;
            }
            if let Some(p) = &self.positions {
                self.refresh_positions_once(p).await;
            }
            if let Some(b) = &self.balance {
                self.refresh_balance_once(b).await;
            }
        }

        // ── main loop: 3 개 interval 을 select 로 편성 ──
        // tokio::time::interval 은 첫 tick 을 즉시 발사. 우린 warm_start 에서 이미
        // 한번 호출했으므로 reset 으로 다음 tick 을 period 이후로 밀어둔다.
        let mut meta_tick = tokio::time::interval(self.meta_period);
        let mut pos_tick = tokio::time::interval(self.positions_period);
        let mut acc_tick = tokio::time::interval(self.accounts_period);
        if self.warm_start {
            meta_tick.reset();
            pos_tick.reset();
            acc_tick.reset();
        }
        meta_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        pos_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        acc_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    debug!(target: "gate::account::poller", "cancel received — exiting");
                    break;
                }
                _ = meta_tick.tick() => {
                    if let Some(c) = &self.meta {
                        self.refresh_meta_once(c).await;
                    }
                }
                _ = pos_tick.tick() => {
                    if let Some(c) = &self.positions {
                        self.refresh_positions_once(c).await;
                    }
                }
                _ = acc_tick.tick() => {
                    if let Some(b) = &self.balance {
                        self.refresh_balance_once(b).await;
                    }
                }
            }
        }
    }

    async fn refresh_meta_once(&self, cache: &SymbolMetaCache) {
        match cache.refresh(&self.client).await {
            Ok(_) => {
                self.stats.meta_refreshes_ok.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .last_meta_ok_ms
                    .store(now_epoch_ms(), Ordering::Relaxed);
            }
            Err(e) => {
                self.stats
                    .meta_refreshes_err
                    .fetch_add(1, Ordering::Relaxed);
                counter_inc(CounterKey::StrategyAccountPollErr);
                warn!(target: "gate::account::poller", error = %e, "contracts refresh failed");
            }
        }
    }

    async fn refresh_positions_once(&self, cache: &PositionCache) {
        match cache.refresh(&self.client).await {
            Ok(()) => {
                self.stats
                    .position_refreshes_ok
                    .fetch_add(1, Ordering::Relaxed);
                self.stats
                    .last_position_ok_ms
                    .store(now_epoch_ms(), Ordering::Relaxed);
            }
            Err(e) => {
                self.stats
                    .position_refreshes_err
                    .fetch_add(1, Ordering::Relaxed);
                counter_inc(CounterKey::StrategyAccountPollErr);
                warn!(target: "gate::account::poller", error = %e, "positions refresh failed");
            }
        }
    }

    async fn refresh_balance_once(&self, slot: &BalanceSlot) {
        match self.client.fetch_accounts().await {
            Ok(b) => {
                self.stats
                    .account_refreshes_ok
                    .fetch_add(1, Ordering::Relaxed);
                self.stats
                    .last_account_ok_ms
                    .store(now_epoch_ms(), Ordering::Relaxed);
                slot.store(Arc::new(b));
            }
            Err(e) => {
                self.stats
                    .account_refreshes_err
                    .fetch_add(1, Ordering::Relaxed);
                counter_inc(CounterKey::StrategyAccountPollErr);
                warn!(target: "gate::account::poller", error = %e, "accounts refresh failed");
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Wire schemas — Gate v4 응답 구조체. 모두 internal.
// ────────────────────────────────────────────────────────────────────────────

/// `/contracts` 응답 row. Gate 는 필드 값이 string/number 혼재라 custom deserializer.
#[derive(Debug, Deserialize, Default)]
struct GateContractRow {
    #[serde(default)]
    name: String,
    /// "0.0001" 같은 문자열로 올 수 있음.
    #[serde(default, deserialize_with = "de_f64_string_or_num")]
    quanto_multiplier: f64,
    #[serde(default, deserialize_with = "de_f64_string_or_num")]
    order_price_round: f64,
    #[serde(default, deserialize_with = "de_f64_string_or_num")]
    order_size_round: f64,
    #[serde(default, deserialize_with = "de_i64_string_or_num")]
    order_size_min: i64,
    /// Gate 는 `risk_limit_max`, 또는 `risk_limit_base` 를 돌려주기도 함 — 전자 우선.
    #[serde(default, deserialize_with = "de_f64_string_or_num")]
    risk_limit_max: f64,
}

impl GateContractRow {
    fn into_meta(self) -> ContractMeta {
        ContractMeta {
            order_price_round: if self.order_price_round > 0.0 {
                self.order_price_round
            } else {
                0.0001
            },
            order_size_round: if self.order_size_round > 0.0 {
                self.order_size_round
            } else {
                1.0
            },
            quanto_multiplier: if self.quanto_multiplier > 0.0 {
                self.quanto_multiplier
            } else {
                1.0
            },
            risk_limit: self.risk_limit_max.max(0.0),
            min_order_size: self.order_size_min.max(1),
        }
    }
}

/// `/positions` row. `value` / `size` 가 부호 포함된 signed 수량. single vs dual 모드 흡수.
#[derive(Debug, Deserialize, Default)]
struct GatePositionRow {
    #[serde(default)]
    contract: String,
    /// 계약 수량 (signed; + long, - short).
    #[serde(default, deserialize_with = "de_i64_string_or_num")]
    size: i64,
    /// USDT notional value (문자열). 양수.
    #[serde(default, deserialize_with = "de_f64_string_or_num")]
    value: f64,
    /// 업데이트 시각 (초). Gate v4 는 float 로 내려보냄.
    #[serde(default, deserialize_with = "de_f64_string_or_num")]
    update_time: f64,
    /// "single" / "dual_long" / "dual_short". dual 모드는 size 부호가 mode 로 대체.
    #[serde(default)]
    mode: String,
}

impl GatePositionRow {
    /// signed USDT notional 환산.
    fn signed_notional(&self) -> f64 {
        // single mode: size 의 sign 이 곧 포지션 방향. value 는 항상 절댓값.
        // dual mode: mode 가 "dual_long" / "dual_short" — value 도 양수.
        let mag = self.value.abs();
        match self.mode.as_str() {
            "dual_short" => -mag,
            "dual_long" => mag,
            _ => {
                if self.size < 0 {
                    -mag
                } else {
                    mag
                }
            }
        }
    }

    fn update_time_sec(&self) -> i64 {
        if self.update_time.is_finite() && self.update_time > 0.0 {
            self.update_time as i64
        } else {
            0
        }
    }
}

fn positions_to_snapshot(rows: Vec<GatePositionRow>) -> PositionSnapshot {
    // Dual-side hedge 모드에서는 같은 심볼이 long/short 2행으로 올 수 있음. `by_symbol` 은
    // 단일 net notional 을 요구하므로 부호 있는 값 합산.
    let mut by_symbol: AHashMap<Symbol, SymbolPosition> = AHashMap::with_capacity(rows.len());
    let mut total_long = 0.0f64;
    let mut total_short = 0.0f64;

    for r in rows {
        if r.contract.is_empty() {
            continue;
        }
        let signed = r.signed_notional();
        if signed.abs() < f64::EPSILON {
            continue;
        }
        if signed > 0.0 {
            total_long += signed;
        } else {
            total_short += signed.abs();
        }
        let sym = Symbol::new(&r.contract);
        let update_sec = r.update_time_sec();
        by_symbol
            .entry(sym)
            .and_modify(|p| {
                p.notional_usdt += signed;
                if update_sec > p.update_time_sec {
                    p.update_time_sec = update_sec;
                }
            })
            .or_insert(SymbolPosition {
                notional_usdt: signed,
                update_time_sec: update_sec,
            });
    }

    PositionSnapshot {
        total_long_usdt: total_long,
        total_short_usdt: total_short,
        by_symbol,
        taken_at_ms: now_epoch_ms(),
    }
}

/// `/accounts` 응답 — Gate 는 single object 반환 (array 아님).
#[derive(Debug, Deserialize, Default)]
struct GateAccountsRow {
    #[serde(default, deserialize_with = "de_f64_string_or_num")]
    total: f64,
    #[serde(default, deserialize_with = "de_f64_string_or_num")]
    unrealised_pnl: f64,
    #[serde(default, deserialize_with = "de_f64_string_or_num")]
    available: f64,
}

impl GateAccountsRow {
    fn into_balance(self) -> AccountBalance {
        AccountBalance {
            total_usdt: self.total,
            unrealized_pnl_usdt: self.unrealised_pnl,
            available_usdt: self.available,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// serde helpers — Gate 가 번호 / 문자열 혼용을 흡수.
// ────────────────────────────────────────────────────────────────────────────

fn de_f64_string_or_num<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    use serde::de::{self, Visitor};
    use std::fmt;

    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = f64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("number or numeric string")
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<f64, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<f64, E> {
            Ok(v as f64)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<f64, E> {
            Ok(v as f64)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<f64, E> {
            let t = v.trim();
            if t.is_empty() {
                return Ok(0.0);
            }
            t.parse::<f64>().map_err(de::Error::custom)
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<f64, E> {
            self.visit_str(&v)
        }
        fn visit_none<E: de::Error>(self) -> Result<f64, E> {
            Ok(0.0)
        }
        fn visit_unit<E: de::Error>(self) -> Result<f64, E> {
            Ok(0.0)
        }
        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<f64, D::Error> {
            d.deserialize_any(self)
        }
    }
    d.deserialize_any(V)
}

fn de_i64_string_or_num<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    use serde::de::{self, Visitor};
    use std::fmt;

    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = i64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("integer or numeric string")
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<i64, E> {
            Ok(v)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<i64, E> {
            Ok(v as i64)
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<i64, E> {
            Ok(v as i64)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<i64, E> {
            let t = v.trim();
            if t.is_empty() {
                return Ok(0);
            }
            if let Ok(i) = t.parse::<i64>() {
                return Ok(i);
            }
            // "1e3" / "3.0" 같이 섞여 들어오면 float → truncate.
            t.parse::<f64>()
                .map(|f| f as i64)
                .map_err(de::Error::custom)
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<i64, E> {
            self.visit_str(&v)
        }
        fn visit_none<E: de::Error>(self) -> Result<i64, E> {
            Ok(0)
        }
        fn visit_unit<E: de::Error>(self) -> Result<i64, E> {
            Ok(0)
        }
        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<i64, D::Error> {
            d.deserialize_any(self)
        }
    }
    d.deserialize_any(V)
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &s[..end]
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 테스트 (offline — wire parsing + snapshot 조립만)
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // 실제 Gate API 응답 포맷 (v4 문서 기준) 을 mini fixture 로.
    const CONTRACTS_JSON: &str = r#"[
      {
        "name":"BTC_USDT",
        "quanto_multiplier":"0.0001",
        "order_price_round":"0.1",
        "order_size_round":"1",
        "order_size_min":1,
        "risk_limit_max":"1000000"
      },
      {
        "name":"ETH_USDT",
        "quanto_multiplier":"0.01",
        "order_price_round":"0.01",
        "order_size_round":"1",
        "order_size_min":2,
        "risk_limit_max":"500000"
      }
    ]"#;

    const POSITIONS_JSON_SINGLE: &str = r#"[
      {
        "contract":"BTC_USDT",
        "size": 5,
        "value":"1234.5",
        "update_time":1713100000.5,
        "mode":"single"
      },
      {
        "contract":"ETH_USDT",
        "size": -10,
        "value":"980",
        "update_time":1713100010,
        "mode":"single"
      }
    ]"#;

    const POSITIONS_JSON_DUAL: &str = r#"[
      {"contract":"BTC_USDT","size":3,"value":"1000","update_time":1713,"mode":"dual_long"},
      {"contract":"BTC_USDT","size":0,"value":"400","update_time":1712,"mode":"dual_short"}
    ]"#;

    const ACCOUNTS_JSON: &str = r#"{
      "total":"12345.67",
      "unrealised_pnl":"-42.1",
      "available":"9999",
      "currency":"USDT"
    }"#;

    #[test]
    fn contracts_parse_and_defaults() {
        let rows: Vec<GateContractRow> = serde_json::from_str(CONTRACTS_JSON).unwrap();
        assert_eq!(rows.len(), 2);
        let meta = rows[0].clone_meta();
        assert_eq!(meta.min_order_size, 1);
        assert_eq!(meta.risk_limit, 1_000_000.0);
        assert_eq!(meta.order_price_round, 0.1);
        assert_eq!(meta.quanto_multiplier, 0.0001);
    }

    #[test]
    fn positions_single_mode_signs_via_size() {
        let rows: Vec<GatePositionRow> = serde_json::from_str(POSITIONS_JSON_SINGLE).unwrap();
        let snap = positions_to_snapshot(rows);
        assert_eq!(snap.total_long_usdt, 1234.5);
        assert_eq!(snap.total_short_usdt, 980.0);
        assert_eq!(snap.by_symbol.len(), 2);
        let btc = snap.by_symbol.get(&Symbol::new("BTC_USDT")).expect("btc");
        assert_eq!(btc.notional_usdt, 1234.5);
        assert_eq!(btc.update_time_sec, 1_713_100_000);
        let eth = snap.by_symbol.get(&Symbol::new("ETH_USDT")).unwrap();
        assert_eq!(eth.notional_usdt, -980.0);
    }

    #[test]
    fn positions_dual_mode_accumulates_net() {
        let rows: Vec<GatePositionRow> = serde_json::from_str(POSITIONS_JSON_DUAL).unwrap();
        let snap = positions_to_snapshot(rows);
        // long 1000 + short -400 → net +600
        let btc = snap.by_symbol.get(&Symbol::new("BTC_USDT")).unwrap();
        assert_eq!(btc.notional_usdt, 600.0);
        assert_eq!(snap.total_long_usdt, 1000.0);
        assert_eq!(snap.total_short_usdt, 400.0);
        // 가장 최신 update_time_sec (= 1713) 유지
        assert_eq!(btc.update_time_sec, 1713);
    }

    #[test]
    fn accounts_parse_mixed_types() {
        let row: GateAccountsRow = serde_json::from_str(ACCOUNTS_JSON).unwrap();
        let b = row.into_balance();
        assert_eq!(b.total_usdt, 12345.67);
        assert_eq!(b.unrealized_pnl_usdt, -42.1);
        assert_eq!(b.available_usdt, 9999.0);
    }

    #[test]
    fn empty_positions_yields_empty_snapshot() {
        let snap = positions_to_snapshot(Vec::new());
        assert!(snap.by_symbol.is_empty());
        assert_eq!(snap.total_long_usdt, 0.0);
        assert_eq!(snap.total_short_usdt, 0.0);
    }

    #[test]
    fn zero_notional_rows_dropped() {
        let rows = vec![GatePositionRow {
            contract: "X".into(),
            size: 0,
            value: 0.0,
            update_time: 0.0,
            mode: "single".into(),
        }];
        let snap = positions_to_snapshot(rows);
        assert!(snap.by_symbol.is_empty());
    }

    #[test]
    fn contract_defaults_when_fields_missing() {
        // quanto_multiplier / price_round 이 0 이나 없는 경우 합리적 fallback.
        let j = r#"{"name":"X_USDT","order_size_min":0}"#;
        let r: GateContractRow = serde_json::from_str(j).unwrap();
        let m = r.clone_meta();
        assert_eq!(m.quanto_multiplier, 1.0);
        assert_eq!(m.order_price_round, 0.0001);
        assert_eq!(m.min_order_size, 1);
    }

    #[test]
    fn builder_requires_at_least_one_target() {
        let client = GateAccountClient::new(Credentials::new("k", "s"), RestClient::new().unwrap());
        let err = AccountPoller::builder(client)
            .meta_period(Duration::from_secs(1))
            .positions_period(Duration::from_secs(1))
            .accounts_period(Duration::from_secs(1))
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least one"), "{err}");
    }

    #[test]
    fn builder_rejects_zero_period() {
        let client = GateAccountClient::new(Credentials::new("k", "s"), RestClient::new().unwrap());
        let err = AccountPoller::builder(client)
            .meta(Arc::new(SymbolMetaCache::new()))
            .meta_period(Duration::from_secs(0))
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("> 0"), "{err}");
    }

    // clone_meta helper just for tests (consumes &self).
    impl GateContractRow {
        fn clone_meta(&self) -> ContractMeta {
            // Clone + into_meta 로 owned 변환.
            Self {
                name: self.name.clone(),
                ..*self
            }
            .into_meta()
        }
    }

    // GateContractRow Clone for test helper
    impl Clone for GateContractRow {
        fn clone(&self) -> Self {
            Self {
                name: self.name.clone(),
                quanto_multiplier: self.quanto_multiplier,
                order_price_round: self.order_price_round,
                order_size_round: self.order_size_round,
                order_size_min: self.order_size_min,
                risk_limit_max: self.risk_limit_max,
            }
        }
    }
}

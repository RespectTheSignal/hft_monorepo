//! strategy 바이너리 엔트리포인트.
//!
//! # Phase 2 D 동작
//!
//! 본 바이너리는 다음 3 가지 실행 모드를 지원한다. 모드는 환경변수
//! `HFT_STRATEGY_VARIANT` 로 선택한다 (기본 `noop`).
//!
//! 1. `noop`  — 기존 idle-loop. subscriber 없이 빈 이벤트 채널만 붙여 프로세스
//!    수명 관리/배포 스모크용. 실 주문 경로 없음.
//! 2. `v6` / `v7` / `v8` — **풀 스택**. 다음 순서로 구성된다.
//!    `hft_config::load_all()` → `AppConfig`
//!    `subscriber::start(cfg, Arc<InprocQueue>)` 로 SUB 소켓 구독 + 채널 세팅
//!    `hft_exchange_gate::GateAccountClient` + `AccountPoller` spawn — 계정 메타 /
//!    포지션 / 잔고를 `SymbolMetaCache` · `PositionCache` · `BalanceSlot` 에 펌프
//!    `PositionOracleImpl::new(meta, positions, last_orders, membership)` 로 oracle 조립
//!    `V?Strategy::with_runtime(oracle, rate_tracker)` 로 러너 주입
//!    주기 tokio task — `BalanceSlot` → `StrategyControl::SetAccountBalance` 를
//!    `StrategyHandle::push_control` 로 내보내 `on_control` 경로로 반영
//!    rate decay task — 매 1s `OrderRateTracker::decay_before_now` 호출
//!
//! # 환경변수
//! - `HFT_STRATEGY_VARIANT`       : `noop` | `v6` | `v7` | `v8` (default `noop`)
//! - `HFT_STRATEGY_LOGIN_NAME`    : 전략 식별자 (= 계정 login_name). default `default`
//! - `HFT_STRATEGY_SYMBOLS`       : 쉼표 구분 심볼 리스트. 비어있으면 AppConfig.exchanges
//!   에서 `ExchangeId::Gate` primary 심볼을 그대로 사용.
//! - `GATE_API_KEY` / `GATE_API_SECRET` : Gate 계정 creds. 미설정이면 poller 비활성화
//!   (전략은 여전히 돌지만 포지션/잔고는 0 유지).
//! - `HFT_STRATEGY_ACCOUNT_MODE` : `shared` | `isolated` — Phase 2 D 는 `shared` 만 사용.
//! - `HFT_BALANCE_PUMP_MS`       : 잔고 pump 주기 ms (default 500).
//! - `HFT_RATE_DECAY_MS`         : rate tracker decay 호출 주기 ms (default 1_000).
//!
//! # Order gateway 연결
//! Phase 2 D 단계에선 order-gateway 가 별도 프로세스로 분리돼 있으나, 실주문 파이프라인
//! 구현은 Phase 2 E 에서. 여기서는 `OrderSender::bounded(1024)` 의 수신단을 동일
//! 프로세스 내 "drain-to-tracing" 태스크가 소비해 발행 흔적을 구조화 로그로 남긴다.
//! ZMQ PUSH 로 교체하는 것은 Phase 2 E 에서.

#![deny(rust_2018_idioms)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Receiver;
use hft_exchange_api::{CancellationToken, OrderRequest};
use hft_exchange_gate::{AccountPoller, BalanceSlot, GateAccountClient, PollerHandle};
use hft_exchange_rest::{Credentials, RestClient};
use hft_strategy_config::StrategyConfig;
use hft_strategy_core::risk::RiskConfig;
use hft_strategy_runtime::{
    AccountMembership, LastOrderStore, OrderRateTracker, PositionCache, PositionOracleImpl,
    SymbolMetaCache,
};
use hft_types::ExchangeId;
use subscriber::InprocQueue;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use hft_config::{AppConfig, ExchangeConfig};
use strategy::{
    start, v6::V6Strategy, v7::V7Strategy, v8::V8Strategy, NoopStrategy, OrderSender,
    StrategyControl, StrategyHandle,
};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ─────────────────────────────────────────────────────────────────────────────
// Variant enum
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    Noop,
    V6,
    V7,
    V8,
}

impl Variant {
    fn from_env() -> Self {
        match std::env::var("HFT_STRATEGY_VARIANT")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "noop" => Variant::Noop,
            "v6" => Variant::V6,
            "v7" => Variant::V7,
            "v8" => Variant::V8,
            other => {
                warn!(
                    target: "strategy::main",
                    variant = %other,
                    "unknown HFT_STRATEGY_VARIANT — falling back to noop"
                );
                Variant::Noop
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Telemetry
// ─────────────────────────────────────────────────────────────────────────────

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
            eprintln!("[strategy] second signal — aborting");
            std::process::exit(130);
        }
        eprintln!("[strategy] shutdown signal received");
        cancel.cancel();
    });
    if let Err(e) = install_result {
        warn!(error = %e, "failed to install ctrlc handler");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Strategy config helper
// ─────────────────────────────────────────────────────────────────────────────

fn gate_exchange_symbols(cfg: &AppConfig) -> Vec<String> {
    cfg.exchanges
        .iter()
        .filter(|e: &&ExchangeConfig| e.id == ExchangeId::Gate)
        .flat_map(|e| e.symbols.iter().map(|s| s.as_str().to_string()))
        .collect()
}

fn load_strategy_symbols(cfg: &AppConfig) -> Vec<String> {
    match std::env::var("HFT_STRATEGY_SYMBOLS") {
        Ok(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => gate_exchange_symbols(cfg),
    }
}

fn build_strategy_config(cfg: &AppConfig) -> Arc<StrategyConfig> {
    let login = std::env::var("HFT_STRATEGY_LOGIN_NAME").unwrap_or_else(|_| "default".to_string());
    let symbols = load_strategy_symbols(cfg);
    Arc::new(StrategyConfig::new(
        login,
        symbols,
        Default::default(), // TradeSettings default — 실 튜닝은 hot-reload loader 가 담당.
    ))
}

fn build_account_membership(strategy_cfg: &StrategyConfig) -> AccountMembership {
    AccountMembership::fixed(strategy_cfg.symbols.clone())
}

// ─────────────────────────────────────────────────────────────────────────────
// Gate account poller spawn
// ─────────────────────────────────────────────────────────────────────────────

/// `GATE_API_KEY` + `GATE_API_SECRET` 가 있으면 Gate REST poller 를 세팅, 없으면 `None`.
fn maybe_spawn_gate_poller(
    meta: Arc<SymbolMetaCache>,
    positions: Arc<PositionCache>,
    balance: Arc<BalanceSlot>,
) -> Option<PollerHandle> {
    let key = match std::env::var("GATE_API_KEY") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            warn!(
                target: "strategy::main",
                "GATE_API_KEY not set — skipping Gate account poller (positions & balance stay 0)"
            );
            return None;
        }
    };
    let secret = match std::env::var("GATE_API_SECRET") {
        Ok(v) if !v.is_empty() => v,
        _ => {
            warn!(
                target: "strategy::main",
                "GATE_API_SECRET not set — skipping Gate account poller"
            );
            return None;
        }
    };

    let http = match RestClient::new() {
        Ok(c) => c,
        Err(e) => {
            error!(target: "strategy::main", error = %e, "RestClient init failed");
            return None;
        }
    };

    let creds = Credentials::new(key, secret);
    let client = GateAccountClient::new(creds, http);

    match AccountPoller::builder(client)
        .meta(meta)
        .positions(positions)
        .balance_slot(balance)
        .meta_period(Duration::from_secs(60))
        .positions_period(Duration::from_secs(1))
        .accounts_period(Duration::from_secs(2))
        .warm_start(true)
        .spawn()
    {
        Ok(h) => {
            info!(target: "strategy::main", "Gate account poller spawned");
            Some(h)
        }
        Err(e) => {
            error!(
                target: "strategy::main",
                error = %e,
                "Gate account poller spawn failed — continuing without account data"
            );
            None
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Balance pump + rate decay tasks
// ─────────────────────────────────────────────────────────────────────────────

/// `BalanceSlot` → `StrategyHandle::control_tx` 를 주기적으로 펌프.
/// slot 값이 변했을 때만 전송 (epsilon 비교) 해 불필요 control 트래픽 절감.
fn spawn_balance_pump(
    slot: Arc<BalanceSlot>,
    handle_ctrl: crossbeam_channel::Sender<StrategyControl>,
    cancel: CancellationToken,
    period: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_sent: Option<(f64, f64)> = None;
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let b = slot.load();
                    let cur = (b.total_usdt, b.unrealized_pnl_usdt);
                    let changed = match last_sent {
                        None => true,
                        Some(prev) => (prev.0 - cur.0).abs() > 1e-9
                            || (prev.1 - cur.1).abs() > 1e-9,
                    };
                    if !changed {
                        continue;
                    }
                    let ctrl = StrategyControl::SetAccountBalance {
                        total_usdt: cur.0,
                        unrealized_pnl_usdt: cur.1,
                    };
                    // try_send — full 이면 다음 tick 에 재시도 (수렴).
                    if let Err(e) = handle_ctrl.try_send(ctrl) {
                        warn!(
                            target: "strategy::main",
                            error = ?e,
                            "balance pump: control channel full/closed"
                        );
                        continue;
                    }
                    last_sent = Some(cur);
                }
            }
        }
        info!(target: "strategy::main", "balance pump task exiting");
    })
}

/// `OrderRateTracker` 의 time-window 밖 엔트리 정리를 주기적으로 수행.
///
/// window 은 `TradeSettings::too_many_orders_time_gap_ms` 를 쓰는게 맞지만, 여기서는
/// 1시간 상한으로 적용 (디폴트 안전 값). 전략별 정교화는 후속.
fn spawn_rate_decay(
    rate: Arc<OrderRateTracker>,
    cancel: CancellationToken,
    period: Duration,
    window_ms: i64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let now_ms = chrono_now_ms();
                    let window = Duration::from_millis(window_ms.max(0) as u64);
                    let dropped = rate.decay_before_now(now_ms, window);
                    if dropped > 0 {
                        tracing::debug!(
                            target: "strategy::main",
                            dropped,
                            "rate tracker decay"
                        );
                    }
                }
            }
        }
        info!(target: "strategy::main", "rate decay task exiting");
    })
}

fn chrono_now_ms() -> i64 {
    // SystemTime epoch ms — 음수 불가능, overflow 실용상 없음.
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Order drain (Phase 2 D — Phase 2 E 에서 ZMQ PUSH 로 교체)
// ─────────────────────────────────────────────────────────────────────────────

/// `OrderSender::bounded` 의 수신단을 소비하며 tracing 으로 기록.
///
/// Phase 2 E 에서 이 함수는 ZMQ PUSH 로 실 order-gateway 에 전달하는 구현으로 교체된다.
/// 현재는 crate 경계를 정리하기 위한 **흡수 태스크**.
fn spawn_order_drain(rx: Receiver<OrderRequest>, cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if cancel.is_cancelled() {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(req) => {
                    info!(
                        target: "strategy::orders",
                        exchange = ?req.exchange,
                        symbol = %req.symbol.as_str(),
                        side = ?req.side,
                        qty = req.qty,
                        price = ?req.price,
                        client_id = %req.client_id,
                        "order emitted (drain)"
                    );
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    tokio::task::yield_now().await;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        info!(target: "strategy::main", "order drain task exiting");
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Full-stack bring-up (v6/v7/v8 공통)
// ─────────────────────────────────────────────────────────────────────────────

struct StackedHandles {
    strategy: StrategyHandle,
    subscriber: subscriber::SubscriberHandle,
    poller: Option<PollerHandle>,
    balance_pump: JoinHandle<()>,
    rate_decay: JoinHandle<()>,
    order_drain: JoinHandle<()>,
    /// sub-task 들의 shutdown token — 주 cancel 과 분리.
    aux_cancel: CancellationToken,
}

impl StackedHandles {
    async fn join_all(self) {
        // order 1: subscriber 먼저 — 이벤트 유입을 끊어야 strategy 가 drain 가능.
        self.subscriber.shutdown();
        // 2: strategy shutdown — 채널 closed 로도 루프 빠져나가지만 token 이 명시적.
        self.strategy.shutdown();
        // 3: aux task cancel.
        self.aux_cancel.cancel();
        if let Some(p) = self.poller {
            p.shutdown().await;
        }
        let _ = self.balance_pump.await;
        let _ = self.rate_decay.await;
        let _ = self.order_drain.await;
        self.subscriber.join().await;
        self.strategy.join().await;
    }
}

async fn bring_up_full(
    cfg: Arc<AppConfig>,
    variant: Variant,
    main_cancel: CancellationToken,
) -> Result<StackedHandles> {
    let strategy_cfg = build_strategy_config(&cfg);
    info!(
        target: "strategy::main",
        variant = ?variant,
        login = %strategy_cfg.login_name,
        symbols = strategy_cfg.symbols.len(),
        "bringing up full strategy stack"
    );

    // ── 캐시 / oracle 공통 구성.
    let meta = Arc::new(SymbolMetaCache::new());
    let positions = Arc::new(PositionCache::new());
    let last_orders = Arc::new(LastOrderStore::new());
    let balance = Arc::new(BalanceSlot::from_pointee(
        hft_exchange_gate::AccountBalance::default(),
    ));
    let rate = Arc::new(OrderRateTracker::new());

    let membership = build_account_membership(&strategy_cfg);
    let oracle = Arc::new(PositionOracleImpl::new(
        meta.clone(),
        positions.clone(),
        last_orders,
        membership,
    ));

    // ── Gate account poller (optional, creds 없으면 skip).
    let poller = maybe_spawn_gate_poller(meta.clone(), positions.clone(), balance.clone());

    // ── channels.
    let (queue, ev_rx) = InprocQueue::bounded(cfg.zmq.hwm.max(1024) as usize);
    let (orders_tx, orders_rx) = OrderSender::bounded(1024);

    // ── strategy spawn (variant-specific `with_runtime` 분기).
    let risk = RiskConfig::default();
    let strategy_handle: StrategyHandle = match variant {
        Variant::V6 => {
            let s = V6Strategy::new(strategy_cfg.clone(), risk).with_runtime(oracle, rate.clone());
            start(s, ev_rx, orders_tx).context("v6 start")?
        }
        Variant::V7 => {
            let s = V7Strategy::new(strategy_cfg.clone(), risk)
                .with_runtime(oracle)
                .with_rate(rate.clone());
            start(s, ev_rx, orders_tx).context("v7 start")?
        }
        Variant::V8 => {
            let s = V8Strategy::new(strategy_cfg.clone(), risk).with_runtime(oracle, rate.clone());
            start(s, ev_rx, orders_tx).context("v8 start")?
        }
        Variant::Noop => unreachable!("bring_up_full invoked with Noop"),
    };

    // ── subscriber.
    let sub_handle = subscriber::start(cfg.clone(), Arc::new(queue))
        .await
        .context("subscriber start")?;

    // ── aux tasks. main_cancel 에 묶이되, 각자 상위 cancel 도 체인.
    let aux_cancel = main_cancel.child_token();

    let pump_period = env_duration_ms("HFT_BALANCE_PUMP_MS", 500);
    let balance_pump = spawn_balance_pump(
        balance.clone(),
        strategy_handle.control_tx.clone(),
        aux_cancel.clone(),
        pump_period,
    );

    let decay_period = env_duration_ms("HFT_RATE_DECAY_MS", 1_000);
    let rate_window_ms: i64 = 3_600_000; // 1h 상한 — rate tracker 는 시간창 밖만 버린다.
    let rate_decay = spawn_rate_decay(rate, aux_cancel.clone(), decay_period, rate_window_ms);

    let order_drain = spawn_order_drain(orders_rx, aux_cancel.clone());

    Ok(StackedHandles {
        strategy: strategy_handle,
        subscriber: sub_handle,
        poller,
        balance_pump,
        rate_decay,
        order_drain,
        aux_cancel,
    })
}

fn env_duration_ms(key: &str, default_ms: u64) -> Duration {
    match std::env::var(key) {
        Ok(v) => v.parse::<u64>().map(Duration::from_millis).unwrap_or_else(|_| {
            warn!(target: "strategy::main", %key, %v, "invalid duration; using default");
            Duration::from_millis(default_ms)
        }),
        Err(_) => Duration::from_millis(default_ms),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Idle (noop) mode
// ─────────────────────────────────────────────────────────────────────────────

async fn run_noop_idle(main_cancel: CancellationToken) -> Result<()> {
    // 독립 실행: event 채널엔 아무도 send 하지 않는다 → runner 는 10ms 폴링 루프만.
    let (_ev_tx, ev_rx) = crossbeam_channel::bounded(64);
    let (orders_tx, _orders_rx) = OrderSender::bounded(64);
    let handle = start(NoopStrategy::default(), ev_rx, orders_tx).context("noop start")?;

    // 신호 대기.
    main_cancel.cancelled().await;
    handle.shutdown();
    handle.join().await;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry
// ─────────────────────────────────────────────────────────────────────────────

async fn run() -> Result<()> {
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;
    let _tele = init_telemetry(&cfg)?;

    let variant = Variant::from_env();
    info!(
        target: "strategy::main",
        service = %cfg.service_name,
        variant = ?variant,
        "strategy starting"
    );

    let main_cancel = CancellationToken::new();
    install_signal_handler(main_cancel.clone());

    match variant {
        Variant::Noop => run_noop_idle(main_cancel).await?,
        v @ (Variant::V6 | Variant::V7 | Variant::V8) => {
            let stacked = bring_up_full(cfg, v, main_cancel.clone()).await?;
            main_cancel.cancelled().await;
            stacked.join_all().await;
        }
    }

    info!(target: "strategy::main", "strategy exited cleanly");
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[strategy] fatal: {e:#}");
            error!(error = %format!("{e:#}"), "strategy fatal");
            ExitCode::from(1)
        }
    }
}

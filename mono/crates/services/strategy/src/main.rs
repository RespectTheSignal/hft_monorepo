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
//! Phase 2 E Step 4c 부터 `OrderSender` 의 수신단은 실제 egress drain 이 소비한다.
//! strategy hot path 는 `(OrderRequest, OrderEgressMetaSeed)` 를 crossbeam 채널에 넣고,
//! drain 태스크가 mode 별 `PolicyOrderEgress` 로 SHM/ZMQ 전송을 수행한다.

#![deny(rust_2018_idioms)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Receiver;
use hft_exchange_api::{CancellationToken, OrderRequest};
use hft_exchange_gate::{AccountPoller, BalanceSlot, GateAccountClient, PollerHandle};
use hft_order_egress::{
    PolicyOrderEgress, ShmOrderEgress, SubmitError, SubmitOutcome, ZmqOrderEgress,
};
use hft_exchange_rest::{Credentials, RestClient};
use hft_shm::{Backing, LayoutSpec, Role, SharedRegion, SubKind, SymbolTable};
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

use hft_config::{order_egress::OrderEgressMode, AppConfig, ExchangeConfig, ShmBackendKind};
use strategy::{
    start, v6::V6Strategy, v7::V7Strategy, v8::V8Strategy, NoopStrategy, OrderEgressMetaSeed,
    OrderEnvelope, OrderSender, StrategyControl, StrategyHandle,
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

/// strategy drain 이 보유하는 mode-specific egress 핸들.
enum DrainEgress {
    /// SHM 정상 경로.
    Shm(PolicyOrderEgress<ShmOrderEgress>),
    /// ZMQ fallback 경로 + symbol intern 용 symtab.
    Zmq {
        inner: PolicyOrderEgress<ZmqOrderEgress>,
        symtab: Arc<SymbolTable>,
    },
}

impl DrainEgress {
    fn try_submit(
        &self,
        req: &OrderRequest,
        seed: &OrderEgressMetaSeed,
        origin_ts_ns: u64,
    ) -> Result<SubmitOutcome, SubmitError> {
        match self {
            Self::Shm(inner) => inner.submit(req, &seed.promote(origin_ts_ns, None, None)),
            Self::Zmq { inner, symtab } => {
                let symbol_id = symtab
                    .get_or_intern(req.exchange, req.symbol.as_str())
                    .map_err(|e| SubmitError::Transport(format!("symtab intern failed: {e}")))?;
                inner.submit(req, &seed.promote(origin_ts_ns, Some(symbol_id), None))
            }
        }
    }
}

/// `strategy_ring_id == 0` 이면 `shm.vm_id` 를 그대로 쓴다.
fn effective_ring_id(cfg: &AppConfig) -> u32 {
    if cfg.order_egress.shm.strategy_ring_id == 0 {
        cfg.shm.vm_id
    } else {
        cfg.order_egress.shm.strategy_ring_id
    }
}

fn build_v2_backing(cfg: &AppConfig) -> Result<Backing> {
    match cfg.shm.backend {
        ShmBackendKind::DevShm => Ok(Backing::DevShm {
            path: cfg.shm.shared_path.clone(),
        }),
        ShmBackendKind::Hugetlbfs => Ok(Backing::Hugetlbfs {
            path: cfg.shm.shared_path.clone(),
        }),
        ShmBackendKind::PciBar => Ok(Backing::PciBar {
            path: cfg.shm.shared_path.clone(),
        }),
        ShmBackendKind::LegacyMultiFile => {
            anyhow::bail!("legacy_multi_file backend does not use shared-region backing")
        }
    }
}

fn build_shared_layout_spec(cfg: &AppConfig) -> Result<LayoutSpec> {
    if cfg.order_egress.shm.ring_capacity as u64 != cfg.shm.order_ring_capacity {
        anyhow::bail!(
            "order_egress.shm.ring_capacity ({}) must match shm.order_ring_capacity ({}) until publisher/order-gateway also migrate",
            cfg.order_egress.shm.ring_capacity,
            cfg.shm.order_ring_capacity
        );
    }
    Ok(LayoutSpec {
        quote_slot_count: cfg.shm.quote_slot_count,
        trade_ring_capacity: cfg.shm.trade_ring_capacity,
        symtab_capacity: cfg.shm.symbol_table_capacity,
        order_ring_capacity: cfg.shm.order_ring_capacity,
        n_max: cfg.shm.n_max,
    })
}

fn open_drain_symtab(cfg: &AppConfig, ring_id: u32) -> Result<Arc<SymbolTable>> {
    match cfg.shm.backend {
        ShmBackendKind::LegacyMultiFile => {
            let symtab = SymbolTable::open_or_create(&cfg.shm.symtab_path, cfg.shm.symbol_table_capacity)
                .with_context(|| format!("SymbolTable::open_or_create({})", cfg.shm.symtab_path.display()))?;
            Ok(Arc::new(symtab))
        }
        ShmBackendKind::DevShm | ShmBackendKind::Hugetlbfs | ShmBackendKind::PciBar => {
            let backing = build_v2_backing(cfg)?;
            let spec = build_shared_layout_spec(cfg)?;
            let shared = SharedRegion::open_view(backing, spec, Role::Strategy { vm_id: ring_id })
                .context("SharedRegion::open_view(strategy symtab)")?;
            let sub = shared
                .sub_region(SubKind::Symtab)
                .context("sub_region(Symtab)")?;
            let symtab = SymbolTable::open_from_region(sub)
                .map_err(|e| anyhow!("SymbolTable::open_from_region: {e}"))?;
            Ok(Arc::new(symtab))
        }
    }
}

fn build_drain_egress(cfg: &AppConfig) -> Result<DrainEgress> {
    let ring_id = effective_ring_id(cfg);
    let policy = cfg.order_egress.backpressure.clone();
    match cfg.order_egress.mode {
        OrderEgressMode::Shm => {
            if matches!(cfg.shm.backend, ShmBackendKind::LegacyMultiFile) {
                anyhow::bail!(
                    "strategy SHM order egress requires shared-region backend; legacy_multi_file is not supported in Step 4c"
                );
            }
            let backing = build_v2_backing(cfg)?;
            let spec = build_shared_layout_spec(cfg)?;
            let client = Arc::new(
                hft_strategy_shm::StrategyShmClient::attach(backing, spec, ring_id)
                    .context("StrategyShmClient::attach(order drain)")?,
            );
            Ok(DrainEgress::Shm(PolicyOrderEgress::new(
                ShmOrderEgress::new(client),
                policy,
            )))
        }
        OrderEgressMode::Zmq => {
            let zmq_cfg = cfg
                .order_egress
                .zmq
                .as_ref()
                .context("order_egress.zmq missing for mode=zmq")?;
            let inner = ZmqOrderEgress::connect(zmq_cfg).context("ZmqOrderEgress::connect")?;
            let symtab = open_drain_symtab(cfg, ring_id)?;
            Ok(DrainEgress::Zmq {
                inner: PolicyOrderEgress::new(inner, policy),
                symtab,
            })
        }
    }
}

fn wall_clock_epoch_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn spawn_order_drain(rx: Receiver<OrderEnvelope>, egress: DrainEgress, cancel: CancellationToken) -> JoinHandle<()> {
    spawn_order_drain_with_now(rx, egress, cancel, wall_clock_epoch_ns)
}

fn spawn_order_drain_with_now<F>(
    rx: Receiver<OrderEnvelope>,
    egress: DrainEgress,
    cancel: CancellationToken,
    now_ns: F,
) -> JoinHandle<()>
where
    F: Fn() -> u64 + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            if cancel.is_cancelled() {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok((req, seed)) => {
                    let origin_ts_ns = now_ns();
                    let mut req = req;
                    req.reduce_only = seed.reduce_only;
                    req.client_seq = seed.client_seq;
                    req.origin_ts_ns = origin_ts_ns;
                    match egress.try_submit(&req, &seed, origin_ts_ns) {
                        Ok(SubmitOutcome::Sent | SubmitOutcome::WouldBlock) => {}
                        Err(e) => {
                            error!(
                                target: "strategy::orders",
                                exchange = ?req.exchange,
                                symbol = %req.symbol.as_str(),
                                side = ?req.side,
                                qty = req.qty,
                                price = ?req.price,
                                client_id = %req.client_id,
                                error = %e,
                                "order drain submit failed"
                            );
                        }
                    }
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
    let drain_egress = build_drain_egress(&cfg).context("build drain egress")?;

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

    let order_drain = spawn_order_drain(orders_rx, drain_egress, aux_cancel.clone());

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

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::sync::Arc;

    use hft_config::order_egress::{BackpressurePolicy, ZmqOrderEgressConfig};
    use hft_exchange_api::{OrderSide, OrderType, TimeInForce};
    use hft_protocol::{order_wire::{OrderRequestWire, ORDER_REQUEST_WIRE_SIZE}, WireLevel};
    use hft_shm::{OrderKind, OrderRingReader, OrderRingWriter, QuoteSlotWriter, TradeRingWriter};
    use hft_types::Symbol;
    use tempfile::tempdir;
    use zmq::Socket;

    fn sample_req() -> OrderRequest {
        OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new("BTC_USDT"),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            qty: 1.0,
            price: Some(100.0),
            reduce_only: false,
            tif: TimeInForce::Gtc,
            client_seq: 7,
            origin_ts_ns: 0,
            client_id: Arc::from("v8-7"),
        }
    }

    fn sample_seed() -> OrderEgressMetaSeed {
        OrderEgressMetaSeed {
            client_seq: 7,
            level: WireLevel::Open,
            reduce_only: false,
            strategy_tag: "v8",
        }
    }

    fn sample_spec() -> LayoutSpec {
        LayoutSpec {
            quote_slot_count: 16,
            trade_ring_capacity: 32,
            symtab_capacity: 16,
            order_ring_capacity: 16,
            n_max: 1,
        }
    }

    fn boot_publisher(path: &Path, spec: LayoutSpec) -> SharedRegion {
        let shared = SharedRegion::create_or_attach(
            Backing::DevShm {
                path: path.to_path_buf(),
            },
            spec,
            Role::Publisher,
        )
        .expect("publisher shared region");
        let _ = QuoteSlotWriter::from_region(shared.sub_region(SubKind::Quote).unwrap(), spec.quote_slot_count)
            .expect("quote writer");
        let _ = TradeRingWriter::from_region(shared.sub_region(SubKind::Trade).unwrap(), spec.trade_ring_capacity)
            .expect("trade writer");
        let _ = SymbolTable::from_region(shared.sub_region(SubKind::Symtab).unwrap(), spec.symtab_capacity)
            .expect("symtab");
        let _ = OrderRingWriter::from_region(
            shared.sub_region(SubKind::OrderRing { vm_id: 0 }).unwrap(),
            spec.order_ring_capacity,
        )
        .expect("order writer");
        shared
    }

    fn base_cfg() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.order_egress.backpressure = BackpressurePolicy::Drop;
        cfg
    }

    fn bind_pull(ctx: &hft_zmq::Context, endpoint: &str) -> Socket {
        let pull = ctx.raw().socket(zmq::PULL).expect("pull socket");
        pull.set_rcvtimeo(1_000).expect("set rcvtimeo");
        pull.bind(endpoint).expect("bind pull");
        pull
    }

    fn free_tcp_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("tcp listener");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        format!("tcp://127.0.0.1:{port}")
    }

    #[test]
    fn effective_ring_id_uses_vm_id_on_zero_sentinel() {
        let mut cfg = base_cfg();
        cfg.shm.vm_id = 3;
        cfg.order_egress.shm.strategy_ring_id = 0;
        assert_eq!(effective_ring_id(&cfg), 3);
        cfg.order_egress.shm.strategy_ring_id = 7;
        assert_eq!(effective_ring_id(&cfg), 7);
    }

    #[tokio::test]
    async fn shm_mode_drain_publishes_frame() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("strategy-drain-shm.bin");
        let spec = sample_spec();
        let shared = boot_publisher(&path, spec);
        let mut reader =
            OrderRingReader::from_region(shared.sub_region(SubKind::OrderRing { vm_id: 0 }).unwrap())
                .unwrap();

        let mut cfg = base_cfg();
        cfg.shm.backend = ShmBackendKind::DevShm;
        cfg.shm.shared_path = path;
        cfg.shm.vm_id = 0;
        cfg.shm.n_max = spec.n_max;
        cfg.shm.quote_slot_count = spec.quote_slot_count;
        cfg.shm.trade_ring_capacity = spec.trade_ring_capacity;
        cfg.shm.symbol_table_capacity = spec.symtab_capacity;
        cfg.shm.order_ring_capacity = spec.order_ring_capacity;
        cfg.order_egress.mode = OrderEgressMode::Shm;
        cfg.order_egress.shm.ring_capacity = spec.order_ring_capacity as usize;

        let egress = build_drain_egress(&cfg).expect("build shm drain egress");
        let cancel = CancellationToken::new();
        let (tx, rx) = crossbeam_channel::bounded(4);
        let handle = spawn_order_drain_with_now(rx, egress, cancel.clone(), || 123);

        tx.send((sample_req(), sample_seed())).unwrap();
        drop(tx);
        handle.await.unwrap();

        let frame = reader.try_consume().expect("shm frame");
        assert_eq!(frame.kind, OrderKind::Place as u8);
        assert_eq!(frame.client_id, 7);
        assert_eq!(frame.ts_ns, 123);
        assert_eq!(frame.exchange_id, hft_shm::exchange_to_u8(ExchangeId::Gate));
        let symtab = SymbolTable::open_from_region(shared.sub_region(SubKind::Symtab).unwrap()).unwrap();
        let symbol_idx = symtab.lookup(ExchangeId::Gate, "BTC_USDT").expect("symbol idx");
        assert_eq!(frame.symbol_idx, symbol_idx);
        assert_eq!(frame.price, 100);
        assert_eq!(frame.size, 1);
    }

    #[tokio::test]
    async fn zmq_mode_drain_sends_wire_with_symbol_id() {
        let dir = tempdir().unwrap();
        let symtab_path = dir.path().join("symtab.bin");
        let endpoint = free_tcp_endpoint();
        let ctx = hft_zmq::Context::new();
        let pull = bind_pull(&ctx, &endpoint);

        let mut cfg = base_cfg();
        cfg.shm.backend = ShmBackendKind::LegacyMultiFile;
        cfg.shm.symtab_path = symtab_path;
        cfg.shm.symbol_table_capacity = 16;
        cfg.order_egress.mode = OrderEgressMode::Zmq;
        cfg.order_egress.backpressure = BackpressurePolicy::RetryWithTimeout {
            max_retries: 64,
            backoff_ns: 1_000_000,
            total_timeout_ns: 500_000_000,
        };
        cfg.order_egress.zmq = Some(ZmqOrderEgressConfig {
            endpoint: endpoint.clone(),
            send_hwm: 8,
            linger_ms: 0,
            reconnect_interval_ms: 10,
            reconnect_interval_max_ms: 10,
        });

        let egress = build_drain_egress(&cfg).expect("build zmq drain egress");
        let cancel = CancellationToken::new();
        let (tx, rx) = crossbeam_channel::bounded(4);
        let handle = spawn_order_drain_with_now(rx, egress, cancel.clone(), || 456);

        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send((sample_req(), sample_seed())).unwrap();
        drop(tx);
        handle.await.unwrap();
        let bytes = pull.recv_bytes(0).expect("recv zmq bytes");

        let buf: [u8; ORDER_REQUEST_WIRE_SIZE] = bytes.try_into().expect("128B wire");
        let wire = OrderRequestWire::decode(&buf).expect("decode order wire");
        assert_eq!(wire.client_seq, 7);
        assert_eq!(wire.origin_ts_ns, 456);
        assert_eq!(&wire.text_tag[..2], b"v8");
        let symtab = SymbolTable::open(&cfg.shm.symtab_path).unwrap();
        let symbol_id = symtab.lookup(ExchangeId::Gate, "BTC_USDT").expect("symbol id");
        assert_eq!(wire.symbol_id, symbol_id);
    }

    #[tokio::test]
    async fn drain_loop_exits_on_closed_channel() {
        let dir = tempdir().unwrap();
        let symtab_path = dir.path().join("symtab-close.bin");
        let endpoint = free_tcp_endpoint();
        let ctx = hft_zmq::Context::new();
        let _pull = bind_pull(&ctx, &endpoint);

        let mut cfg = base_cfg();
        cfg.shm.backend = ShmBackendKind::LegacyMultiFile;
        cfg.shm.symtab_path = symtab_path;
        cfg.shm.symbol_table_capacity = 16;
        cfg.order_egress.mode = OrderEgressMode::Zmq;
        cfg.order_egress.zmq = Some(ZmqOrderEgressConfig {
            endpoint,
            send_hwm: 8,
            linger_ms: 0,
            reconnect_interval_ms: 10,
            reconnect_interval_max_ms: 10,
        });

        let egress = build_drain_egress(&cfg).expect("build zmq drain egress");
        let cancel = CancellationToken::new();
        let (tx, rx) = crossbeam_channel::bounded(1);
        drop(tx);
        let handle = spawn_order_drain_with_now(rx, egress, cancel, || 999);
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("drain exit timeout")
            .unwrap();
    }
}

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
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use hft_common::health_server::{start_health_server, HealthServerConfig};
use hft_common::supervisor::{run_with_restart, RestartConfig, SupervisorExit};
use hft_exchange_api::{CancellationToken, OrderRequest};
use hft_exchange_gate::{
    AccountBalance, AccountPoller, BalanceSlot, BalanceUpdatePayload, GateAccountClient,
    GateUserStream, PollerHandle, PositionUpdatePayload, UserStreamCallback, UserStreamEvent,
};
use hft_exchange_rest::{Credentials, RestClient};
use hft_order_egress::{
    PolicyOrderEgress, ShmOrderEgress, SubmitError, SubmitOutcome, ZmqOrderEgress,
};
use hft_protocol::order_wire::{
    is_heartbeat_wire, OrderResultWire, WireError, ORDER_RESULT_WIRE_SIZE, STATUS_ACCEPTED,
    STATUS_HEARTBEAT, STATUS_REJECTED,
};
use hft_shm::{Backing, LayoutSpec, Role, SharedRegion, SubKind, SymbolTable};
use hft_strategy_config::StrategyConfig;
use hft_strategy_core::risk::RiskConfig;
use hft_strategy_runtime::{
    AccountMembership, LastOrderStore, OrderRateTracker, PositionCache, PositionOracleImpl,
    PositionSnapshot, SymbolMetaCache, SymbolPosition,
};
use hft_time::{Clock, SystemClock};
use hft_types::{ExchangeId, Symbol};
use hft_zmq::Context as ZmqContext;
use subscriber::InprocQueue;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use hft_config::{order_egress::OrderEgressMode, AppConfig, ExchangeConfig, ShmBackendKind};
use hft_telemetry::{counter_inc, gauge_set, CounterKey, GaugeKey};
use strategy::{
    close_unhealthy::CloseUnhealthyStrategy, close_v1::CloseV1Strategy, mm_close::MmCloseStrategy,
    spawn_order_drain_loop_with_now, start, v6::V6Strategy, v7::V7Strategy, v8::V8Strategy,
    GatewayLiveness, NoopStrategy, OrderEgressMetaSeed, OrderEnvelope, OrderResultInfo,
    OrderSender, ResultStatus, StrategyControl, StrategyHandle,
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
    CloseV1,
    MmClose,
    CloseUnhealthy,
}

impl Variant {
    fn from_name(raw: &str) -> Self {
        match raw {
            "" | "noop" => Variant::Noop,
            "v6" => Variant::V6,
            "v7" => Variant::V7,
            "v8" => Variant::V8,
            "close_v1" | "close" => Variant::CloseV1,
            "mm_close" => Variant::MmClose,
            "close_unhealthy" => Variant::CloseUnhealthy,
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

    fn from_env() -> Self {
        let raw = std::env::var("HFT_STRATEGY_VARIANT")
            .unwrap_or_default()
            .to_ascii_lowercase();
        Self::from_name(raw.as_str())
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

fn spawn_uptime_gauge_updater(cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let boot_instant = std::time::Instant::now();
        loop {
            gauge_set(
                GaugeKey::UptimeSeconds,
                boot_instant.elapsed().as_secs() as i64,
            );
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    })
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

/// AppConfig 에서 전략 risk 설정을 조립한다.
///
/// 현재 runtime override 대상은 leverage 하나뿐이다. 값이 없으면
/// `RiskConfig::default()` 의 기본값(50.0)을 그대로 사용한다.
fn build_risk_config(cfg: &AppConfig) -> RiskConfig {
    let mut risk = RiskConfig::default();
    if let Some(leverage) = cfg.leverage {
        risk.leverage = leverage;
    }
    risk
}

fn build_account_membership(strategy_cfg: &StrategyConfig) -> AccountMembership {
    AccountMembership::fixed(strategy_cfg.symbols.clone())
}

// ─────────────────────────────────────────────────────────────────────────────
// Gate account poller spawn
// ─────────────────────────────────────────────────────────────────────────────

/// `GATE_API_KEY` + `GATE_API_SECRET` 가 있으면 Gate REST poller 를 세팅, 없으면 `None`.
fn gate_api_credentials() -> Option<(String, String)> {
    let api_key = std::env::var("GATE_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())?;
    let api_secret = std::env::var("GATE_API_SECRET")
        .ok()
        .filter(|v| !v.is_empty())?;
    Some((api_key, api_secret))
}

/// `GATE_API_KEY` + `GATE_API_SECRET` 가 있으면 Gate REST poller 를 세팅, 없으면 `None`.
fn maybe_spawn_gate_poller(
    meta: Arc<SymbolMetaCache>,
    positions: Arc<PositionCache>,
    balance: Arc<BalanceSlot>,
) -> Option<PollerHandle> {
    let (key, secret) = match gate_api_credentials() {
        Some(creds) => creds,
        None => {
            warn!(
                target: "strategy::main",
                "Gate API credentials missing — skipping Gate account poller (positions & balance stay 0)"
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

fn parse_ws_decimal(field: &str, raw: &str) -> Option<f64> {
    match raw.parse::<f64>() {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(
                target: "strategy::user_stream",
                field,
                raw,
                error = %e,
                "failed to parse Gate WS decimal field"
            );
            None
        }
    }
}

fn push_strategy_control(
    control_tx: &Sender<StrategyControl>,
    ctrl: StrategyControl,
    source: &str,
) {
    if let Err(e) = control_tx.try_send(ctrl) {
        match e {
            crossbeam_channel::TrySendError::Full(_) => {
                counter_inc(CounterKey::StrategyControlDropped);
                warn!(
                    target: "strategy::user_stream",
                    source,
                    "user stream control channel full — dropping control message"
                );
            }
            crossbeam_channel::TrySendError::Disconnected(_) => {
                warn!(
                    target: "strategy::user_stream",
                    source,
                    "user stream control channel disconnected"
                );
            }
        }
    }
}

fn recompute_position_totals(snapshot: &mut PositionSnapshot) {
    let mut total_long = 0.0;
    let mut total_short = 0.0;
    for pos in snapshot.by_symbol.values() {
        if pos.notional_usdt > 0.0 {
            total_long += pos.notional_usdt;
        } else {
            total_short += pos.notional_usdt.abs();
        }
    }
    snapshot.total_long_usdt = total_long;
    snapshot.total_short_usdt = total_short;
}

fn apply_ws_balance_update(slot: &BalanceSlot, payload: &BalanceUpdatePayload) {
    let Some(total_usdt) = parse_ws_decimal("balance", &payload.balance) else {
        return;
    };
    let current = slot.load();
    slot.store(Arc::new(AccountBalance {
        total_usdt,
        unrealized_pnl_usdt: current.unrealized_pnl_usdt,
        available_usdt: total_usdt,
    }));
}

fn apply_ws_position_update(
    positions: &PositionCache,
    meta: &SymbolMetaCache,
    payload: &PositionUpdatePayload,
) {
    let symbol = Symbol::new(payload.contract.as_str());
    let mark_price = parse_ws_decimal("mark_price", &payload.mark_price)
        .or_else(|| parse_ws_decimal("entry_price", &payload.entry_price))
        .unwrap_or(0.0);
    let quanto_multiplier = meta
        .get(&symbol)
        .map(|m| m.quanto_multiplier)
        .unwrap_or(1.0);
    let mut snapshot = (*positions.snapshot()).clone();

    if payload.size == 0 {
        snapshot.by_symbol.remove(&symbol);
    } else {
        let update_time_sec = if payload.update_time > 0 {
            payload.update_time
        } else {
            chrono_now_ms() / 1000
        };
        snapshot.by_symbol.insert(
            symbol,
            SymbolPosition {
                notional_usdt: payload.size as f64 * mark_price * quanto_multiplier,
                update_time_sec,
            },
        );
    }

    snapshot.taken_at_ms = chrono_now_ms();
    recompute_position_totals(&mut snapshot);
    positions.store(snapshot);
}

fn spawn_gate_user_stream(
    meta: Arc<SymbolMetaCache>,
    positions: Arc<PositionCache>,
    balance: Arc<BalanceSlot>,
    control_tx: Sender<StrategyControl>,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    let (api_key, api_secret) = match gate_api_credentials() {
        Some(creds) => creds,
        None => {
            info!(
                target: "strategy::main",
                "Gate API credentials missing — WS user stream disabled, REST polling only"
            );
            return None;
        }
    };

    Some(tokio::spawn(async move {
        let user_stream = GateUserStream::new(api_key, api_secret);
        let callback: UserStreamCallback = Arc::new(move |ev| match ev {
            UserStreamEvent::PositionUpdate(payload) => {
                apply_ws_position_update(&positions, &meta, &payload);
                let entry_price =
                    parse_ws_decimal("entry_price", &payload.entry_price).unwrap_or(0.0);
                let unrealised_pnl =
                    parse_ws_decimal("unrealised_pnl", &payload.unrealised_pnl).unwrap_or(0.0);
                let mark_price = parse_ws_decimal("mark_price", &payload.mark_price).unwrap_or(0.0);
                push_strategy_control(
                    &control_tx,
                    StrategyControl::WsPositionUpdate {
                        contract: payload.contract,
                        size: payload.size,
                        entry_price,
                        unrealised_pnl,
                        mark_price,
                    },
                    "position_update",
                );
            }
            UserStreamEvent::BalanceUpdate(payload) => {
                apply_ws_balance_update(&balance, &payload);
                let balance_value = parse_ws_decimal("balance", &payload.balance).unwrap_or(0.0);
                let change = parse_ws_decimal("change", &payload.change).unwrap_or(0.0);
                push_strategy_control(
                    &control_tx,
                    StrategyControl::WsBalanceUpdate {
                        balance: balance_value,
                        change,
                    },
                    "balance_update",
                );
            }
            UserStreamEvent::OrderUpdate(payload) => {
                let fill_price = parse_ws_decimal("fill_price", &payload.fill_price).unwrap_or(0.0);
                push_strategy_control(
                    &control_tx,
                    StrategyControl::WsOrderUpdate {
                        order_id: payload.id,
                        contract: payload.contract,
                        status: payload.status,
                        left: payload.left,
                        fill_price,
                    },
                    "order_update",
                );
            }
            UserStreamEvent::UserTrade(_) => {}
        });

        if let Err(e) = user_stream.run(callback, cancel).await {
            warn!(error = %e, "Gate WS user stream stopped");
        }
    }))
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
                        match e {
                            crossbeam_channel::TrySendError::Full(_) => {
                                counter_inc(CounterKey::StrategyControlDropped);
                                warn!(
                                    target: "strategy::main",
                                    "balance pump: control channel full — dropping control message"
                                );
                            }
                            crossbeam_channel::TrySendError::Disconnected(_) => {
                                warn!(
                                    target: "strategy::main",
                                    "balance pump: control channel disconnected"
                                );
                            }
                        }
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
    SystemClock::default().now_ms()
}

fn spawn_strategy_metrics_updater(
    liveness: Option<GatewayLiveness>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let clock = SystemClock::default();
        loop {
            gauge_set(GaugeKey::ActiveStrategies, 1);
            let heartbeat_age_ms = match liveness.as_ref() {
                Some(lv) => {
                    let last = lv.last_heartbeat_ns();
                    if last == 0 {
                        0
                    } else {
                        clock.epoch_ns().saturating_sub(last) / 1_000_000
                    }
                }
                None => 0,
            };
            gauge_set(GaugeKey::LastHeartbeatAgeMs, heartbeat_age_ms as i64);
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
        gauge_set(GaugeKey::ActiveStrategies, 0);
        gauge_set(GaugeKey::LastHeartbeatAgeMs, 0);
    })
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
            let symtab =
                SymbolTable::open_or_create(&cfg.shm.symtab_path, cfg.shm.symbol_table_capacity)
                    .with_context(|| {
                        format!(
                            "SymbolTable::open_or_create({})",
                            cfg.shm.symtab_path.display()
                        )
                    })?;
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

fn decode_zero_padded_string(bytes: &[u8]) -> String {
    let len = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

enum ResultOrHeartbeat {
    Result(OrderResultInfo),
    Heartbeat(u64),
}

fn decode_order_result_info_or_heartbeat(buf: &[u8]) -> Result<ResultOrHeartbeat> {
    if buf.len() != ORDER_RESULT_WIRE_SIZE {
        anyhow::bail!("unexpected order result wire size: {}", buf.len());
    }

    let mut raw = [0u8; ORDER_RESULT_WIRE_SIZE];
    raw.copy_from_slice(buf);

    if is_heartbeat_wire(&raw) {
        let wire = OrderResultWire::decode(&raw).map_err(|e: WireError| anyhow!(e))?;
        if wire.status != STATUS_HEARTBEAT {
            anyhow::bail!("heartbeat fast-path disagrees with decoded status");
        }
        return Ok(ResultOrHeartbeat::Heartbeat(wire.gateway_ts_ns));
    }

    let wire = OrderResultWire::decode(&raw).map_err(|e: WireError| anyhow!(e))?;
    let status = match wire.status {
        STATUS_ACCEPTED => ResultStatus::Accepted,
        STATUS_REJECTED => ResultStatus::Rejected,
        other => anyhow::bail!("unsupported result status={other} for strategy listener"),
    };

    Ok(ResultOrHeartbeat::Result(OrderResultInfo {
        client_seq: wire.client_seq,
        status,
        exchange_order_id: decode_zero_padded_string(&wire.exchange_order_id),
        gateway_ts_ns: wire.gateway_ts_ns,
        text_tag: decode_zero_padded_string(&wire.text_tag),
    }))
}

fn spawn_result_listener(
    endpoint: &str,
    zmq_cfg: &hft_config::ZmqConfig,
    control_tx: Sender<StrategyControl>,
    cancel: CancellationToken,
    liveness: Option<GatewayLiveness>,
) -> Result<JoinHandle<()>> {
    spawn_result_listener_with_context(
        ZmqContext::new(),
        endpoint,
        zmq_cfg.clone(),
        control_tx,
        cancel,
        liveness,
    )
}

fn spawn_result_listener_with_context(
    ctx: ZmqContext,
    endpoint: &str,
    zmq_cfg: hft_config::ZmqConfig,
    control_tx: Sender<StrategyControl>,
    cancel: CancellationToken,
    liveness: Option<GatewayLiveness>,
) -> Result<JoinHandle<()>> {
    let endpoint = endpoint.to_string();
    Ok(tokio::task::spawn_blocking(move || {
        let mut pull = match ctx.pull_connect(&endpoint, &zmq_cfg) {
            Ok(sock) => sock,
            Err(e) => {
                error!(
                    target: "strategy::result_listener",
                    endpoint = %endpoint,
                    error = %e,
                    "failed to connect result PULL socket"
                );
                return;
            }
        };
        info!(
            target: "strategy::result_listener",
            endpoint = %endpoint,
            "strategy result listener started"
        );

        loop {
            if cancel.is_cancelled() {
                break;
            }

            match pull.recv_bytes_timeout(50) {
                Ok(Some(buf)) => match decode_order_result_info_or_heartbeat(&buf) {
                    Ok(ResultOrHeartbeat::Heartbeat(ts_ns)) => {
                        if let Some(ref lv) = liveness {
                            lv.touch(ts_ns);
                        }
                    }
                    Ok(ResultOrHeartbeat::Result(info)) => {
                        if let Err(e) = control_tx.try_send(StrategyControl::OrderResult(info)) {
                            match e {
                                crossbeam_channel::TrySendError::Full(_) => {
                                    counter_inc(CounterKey::StrategyControlDropped);
                                    warn!(
                                        target: "strategy::result_listener",
                                        "result control channel full — dropping order result"
                                    );
                                }
                                crossbeam_channel::TrySendError::Disconnected(_) => {
                                    warn!(
                                        target: "strategy::result_listener",
                                        "result control channel disconnected"
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => warn!(
                        target: "strategy::result_listener",
                        endpoint = %endpoint,
                        error = %format!("{e:#}"),
                        "failed to decode order result wire"
                    ),
                },
                Ok(None) => continue,
                Err(e) => warn!(
                    target: "strategy::result_listener",
                    endpoint = %endpoint,
                    error = %e,
                    "result listener recv failed"
                ),
            }
        }

        info!(target: "strategy::result_listener", "strategy result listener stopped");
    }))
}

fn spawn_order_drain(
    rx: Receiver<OrderEnvelope>,
    egress: DrainEgress,
    cancel: CancellationToken,
    liveness: Option<GatewayLiveness>,
) -> JoinHandle<()> {
    let clock = SystemClock::default();
    spawn_order_drain_with_now(rx, egress, cancel, move || clock.epoch_ns(), liveness)
}

fn spawn_order_drain_with_now<F>(
    rx: Receiver<OrderEnvelope>,
    egress: DrainEgress,
    cancel: CancellationToken,
    now_ns: F,
    liveness: Option<GatewayLiveness>,
) -> JoinHandle<()>
where
    F: Fn() -> u64 + Send + 'static,
{
    spawn_order_drain_loop_with_now(
        rx,
        cancel,
        now_ns,
        liveness,
        move |req, seed, origin_ts_ns| match egress.try_submit(&req, &seed, origin_ts_ns)? {
            SubmitOutcome::Sent | SubmitOutcome::WouldBlock => Ok::<(), SubmitError>(()),
        },
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Full-stack bring-up (v6/v7/v8 공통)
// ─────────────────────────────────────────────────────────────────────────────

struct StackedHandles {
    strategy: StrategyHandle,
    subscriber: subscriber::SubscriberHandle,
    poller: Option<PollerHandle>,
    user_stream: Option<JoinHandle<()>>,
    balance_pump: JoinHandle<()>,
    rate_decay: JoinHandle<()>,
    order_drain: JoinHandle<()>,
    result_listener: Option<JoinHandle<()>>,
    strategy_metrics: JoinHandle<()>,
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
        if let Some(h) = self.user_stream {
            let _ = h.await;
        }
        let _ = self.balance_pump.await;
        let _ = self.rate_decay.await;
        let _ = self.order_drain.await;
        if let Some(h) = self.result_listener {
            let _ = h.await;
        }
        let _ = self.strategy_metrics.await;
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
    let risk = build_risk_config(&cfg);
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
        Variant::CloseV1 => {
            let s = CloseV1Strategy::new(strategy_cfg.clone(), risk)
                .with_runtime(oracle)
                .with_rate(rate.clone());
            start(s, ev_rx, orders_tx).context("close_v1 start")?
        }
        Variant::MmClose => {
            let s = MmCloseStrategy::new(strategy_cfg.clone(), risk)
                .with_runtime(oracle)
                .with_rate(rate.clone());
            start(s, ev_rx, orders_tx).context("mm_close start")?
        }
        Variant::CloseUnhealthy => {
            let s = CloseUnhealthyStrategy::new(strategy_cfg.clone(), risk)
                .with_runtime(oracle)
                .with_rate(rate.clone());
            start(s, ev_rx, orders_tx).context("close_unhealthy start")?
        }
        Variant::Noop => unreachable!("bring_up_full invoked with Noop"),
    };

    // ── aux tasks. main_cancel 에 묶이되, 각자 상위 cancel 도 체인.
    let aux_cancel = main_cancel.child_token();

    let user_stream = spawn_gate_user_stream(
        meta.clone(),
        positions.clone(),
        balance.clone(),
        strategy_handle.control_tx.clone(),
        aux_cancel.clone(),
    );

    // ── subscriber.
    let sub_handle = subscriber::start(cfg.clone(), Arc::new(queue))
        .await
        .context("subscriber start")?;

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

    let liveness = if cfg.order_egress.heartbeat_timeout_ms > 0
        && cfg.order_egress.result_zmq_connect.is_some()
    {
        Some(GatewayLiveness::new(cfg.order_egress.heartbeat_timeout_ms))
    } else {
        None
    };

    let order_drain = spawn_order_drain(
        orders_rx,
        drain_egress,
        aux_cancel.clone(),
        liveness.clone(),
    );
    let result_listener = match cfg.order_egress.result_zmq_connect.as_deref() {
        Some(endpoint) => Some(
            spawn_result_listener(
                endpoint,
                &cfg.zmq,
                strategy_handle.control_tx.clone(),
                aux_cancel.clone(),
                liveness.clone(),
            )
            .context("spawn result listener")?,
        ),
        None => None,
    };
    let strategy_metrics = spawn_strategy_metrics_updater(liveness, aux_cancel.clone());

    Ok(StackedHandles {
        strategy: strategy_handle,
        subscriber: sub_handle,
        poller,
        user_stream,
        balance_pump,
        rate_decay,
        order_drain,
        result_listener,
        strategy_metrics,
        aux_cancel,
    })
}

fn env_duration_ms(key: &str, default_ms: u64) -> Duration {
    match std::env::var(key) {
        Ok(v) => v
            .parse::<u64>()
            .map(Duration::from_millis)
            .unwrap_or_else(|_| {
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

async fn run_inner(
    cfg: Arc<AppConfig>,
    variant: Variant,
    main_cancel: CancellationToken,
) -> Result<()> {
    info!(
        target: "strategy::main",
        service = %cfg.service_name,
        variant = ?variant,
        "strategy starting"
    );

    match variant {
        Variant::Noop => run_noop_idle(main_cancel).await?,
        v @ (Variant::V6
        | Variant::V7
        | Variant::V8
        | Variant::CloseV1
        | Variant::MmClose
        | Variant::CloseUnhealthy) => {
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
    let cfg = match hft_config::load_all() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("[strategy] config load failed: {e}");
            return ExitCode::from(1);
        }
    };

    let _tele = match init_telemetry(&cfg) {
        Ok(tele) => tele,
        Err(e) => {
            eprintln!("[strategy] telemetry init failed: {e:#}");
            return ExitCode::from(1);
        }
    };

    let variant = Variant::from_env();
    let main_cancel = CancellationToken::new();
    install_signal_handler(main_cancel.clone());
    let _uptime = spawn_uptime_gauge_updater(main_cancel.clone());
    let health_port = cfg.telemetry.prom_port.unwrap_or(9100);
    let _health = match start_health_server(
        HealthServerConfig {
            port: health_port,
            service_name: "strategy".into(),
        },
        main_cancel.clone(),
    )
    .await
    {
        Ok(handle) => {
            info!(addr = %handle.local_addr, "health server started");
            handle
        }
        Err(e) => {
            eprintln!("[strategy] health server start failed: {e:#}");
            return ExitCode::from(1);
        }
    };

    match run_with_restart("strategy", RestartConfig::default(), main_cancel.clone(), {
        let cfg = cfg.clone();
        move || {
            let cfg = cfg.clone();
            let cancel = main_cancel.clone();
            async move { run_inner(cfg, variant, cancel).await }
        }
    })
    .await
    {
        Ok(SupervisorExit::Clean) => ExitCode::SUCCESS,
        Ok(SupervisorExit::MaxRetriesExceeded {
            attempts,
            last_error,
        }) => {
            eprintln!("[strategy] gave up after {attempts} failures: {last_error}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("[strategy] supervisor fatal: {e:#}");
            error!(error = %format!("{e:#}"), "strategy supervisor fatal");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::sync::Arc;

    use hft_config::{
        order_egress::{BackpressurePolicy, ZmqOrderEgressConfig},
        ZmqConfig,
    };
    use hft_exchange_api::{OrderSide, OrderType, TimeInForce};
    use hft_protocol::{
        order_wire::{
            OrderRequestWire, OrderResultWire, ORDER_REQUEST_WIRE_SIZE, STATUS_ACCEPTED,
            STATUS_REJECTED,
        },
        WireLevel,
    };
    use hft_shm::{OrderKind, OrderRingReader, OrderRingWriter, QuoteSlotWriter, TradeRingWriter};
    use hft_strategy_runtime::ContractMeta;
    use hft_types::Symbol;
    use order_gateway::{
        start_with_arc, zmq_ingress::start_zmq_order_ingress, IngressEnvelope, NoopExecutor,
        RetryPolicy, Route, RoutingTable, DEDUP_CACHE_CAP_DEFAULT,
    };
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
        let _ = QuoteSlotWriter::from_region(
            shared.sub_region(SubKind::Quote).unwrap(),
            spec.quote_slot_count,
        )
        .expect("quote writer");
        let _ = TradeRingWriter::from_region(
            shared.sub_region(SubKind::Trade).unwrap(),
            spec.trade_ring_capacity,
        )
        .expect("trade writer");
        let _ = SymbolTable::from_region(
            shared.sub_region(SubKind::Symtab).unwrap(),
            spec.symtab_capacity,
        )
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

    #[test]
    fn leverage_override_from_config() {
        let mut cfg = base_cfg();
        cfg.leverage = Some(20.0);
        let risk = build_risk_config(&cfg);
        assert_eq!(risk.leverage, 20.0);
    }

    #[test]
    fn leverage_default_when_none() {
        let cfg = base_cfg();
        let risk = build_risk_config(&cfg);
        assert_eq!(risk.leverage, 50.0);
    }

    #[test]
    fn ws_position_update_updates_cache() {
        let meta = SymbolMetaCache::seeded([(
            Symbol::new("BTC_USDT"),
            ContractMeta {
                quanto_multiplier: 1.0,
                ..Default::default()
            },
        )]);
        let positions = PositionCache::new();
        let payload = PositionUpdatePayload {
            contract: "BTC_USDT".to_string(),
            size: 2,
            entry_price: "65000.0".to_string(),
            leverage: 20,
            margin: "100".to_string(),
            mode: "single".to_string(),
            realised_pnl: "0".to_string(),
            unrealised_pnl: "3.5".to_string(),
            liq_price: "50000".to_string(),
            mark_price: "65000.5".to_string(),
            update_id: 7,
            update_time: 1_710_000_100,
        };

        apply_ws_position_update(&positions, &meta, &payload);
        let snapshot = positions.snapshot();
        let symbol = Symbol::new("BTC_USDT");
        let pos = snapshot.symbol_position(&symbol).expect("symbol position");
        assert_eq!(pos.update_time_sec, 1_710_000_100);
        assert!((pos.notional_usdt - 130_001.0).abs() < 1e-9);
        assert!((snapshot.total_long_usdt - 130_001.0).abs() < 1e-9);
        assert_eq!(snapshot.total_short_usdt, 0.0);
    }

    #[test]
    fn ws_balance_update_updates_slot() {
        let slot = BalanceSlot::from_pointee(AccountBalance {
            total_usdt: 100.0,
            unrealized_pnl_usdt: 7.5,
            available_usdt: 90.0,
        });
        let payload = BalanceUpdatePayload {
            balance: "123.5".to_string(),
            change: "-1.0".to_string(),
            fund_type: "fee".to_string(),
            time_ms: 1_710_000_000_123,
        };

        apply_ws_balance_update(&slot, &payload);
        let current = slot.load();
        assert_eq!(current.total_usdt, 123.5);
        assert_eq!(current.unrealized_pnl_usdt, 7.5);
        assert_eq!(current.available_usdt, 123.5);
    }

    #[test]
    fn variant_from_name_maps_close_variants() {
        assert_eq!(Variant::from_name("close_v1"), Variant::CloseV1);
        assert_eq!(Variant::from_name("close"), Variant::CloseV1);
        assert_eq!(Variant::from_name("mm_close"), Variant::MmClose);
        assert_eq!(
            Variant::from_name("close_unhealthy"),
            Variant::CloseUnhealthy
        );
    }

    fn bind_pull(ctx: &hft_zmq::Context, endpoint: &str) -> Socket {
        let pull = ctx.raw().socket(zmq::PULL).expect("pull socket");
        pull.set_rcvtimeo(1_000).expect("set rcvtimeo");
        pull.bind(endpoint).expect("bind pull");
        pull
    }

    fn bind_push(ctx: &hft_zmq::Context, endpoint: &str) -> Socket {
        let push = ctx.raw().socket(zmq::PUSH).expect("push socket");
        push.set_sndtimeo(1_000).expect("set sndtimeo");
        push.bind(endpoint).expect("bind push");
        push
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
        let mut reader = OrderRingReader::from_region(
            shared.sub_region(SubKind::OrderRing { vm_id: 0 }).unwrap(),
        )
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

        let egress = build_drain_egress(&cfg).expect("build shm drain egress");
        let cancel = CancellationToken::new();
        let (tx, rx) = crossbeam_channel::bounded(4);
        let handle = spawn_order_drain_with_now(rx, egress, cancel.clone(), || 123, None);

        tx.send((sample_req(), sample_seed())).unwrap();
        drop(tx);
        handle.await.unwrap();

        let frame = reader.try_consume().expect("shm frame");
        assert_eq!(frame.kind, OrderKind::Place as u8);
        assert_eq!(frame.client_id, 7);
        assert_eq!(frame.ts_ns, 123);
        assert_eq!(frame.exchange_id, hft_shm::exchange_to_u8(ExchangeId::Gate));
        let symtab =
            SymbolTable::open_from_region(shared.sub_region(SubKind::Symtab).unwrap()).unwrap();
        let symbol_idx = symtab
            .lookup(ExchangeId::Gate, "BTC_USDT")
            .expect("symbol idx");
        assert_eq!(frame.symbol_idx, symbol_idx);
        assert_eq!(frame.price, 10_000_000_000);
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
        let handle = spawn_order_drain_with_now(rx, egress, cancel.clone(), || 456, None);

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
        let symbol_id = symtab
            .lookup(ExchangeId::Gate, "BTC_USDT")
            .expect("symbol id");
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
        let handle = spawn_order_drain_with_now(rx, egress, cancel, || 999, None);
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("drain exit timeout")
            .unwrap();
    }

    #[test]
    fn gateway_liveness_startup_grace() {
        let lv = GatewayLiveness::new(5000);
        assert!(lv.is_alive(1_000_000_000));
    }

    #[test]
    fn gateway_liveness_alive_within_timeout() {
        let lv = GatewayLiveness::new(5000);
        lv.touch(100_000_000_000);
        assert!(lv.is_alive(104_000_000_000));
    }

    #[test]
    fn gateway_liveness_stale_after_timeout() {
        let lv = GatewayLiveness::new(5000);
        lv.touch(100_000_000_000);
        assert!(!lv.is_alive(106_000_000_000));
    }

    #[test]
    fn gateway_liveness_disabled_when_zero() {
        let lv = GatewayLiveness::new(0);
        assert!(lv.is_alive(999_000_000_000));
    }

    #[tokio::test]
    async fn result_listener_heartbeat_updates_liveness() {
        let ctx = hft_zmq::Context::new();
        let endpoint = free_tcp_endpoint();
        let push = bind_push(&ctx, &endpoint);
        let (control_tx, control_rx) = crossbeam_channel::bounded(8);
        let cancel = CancellationToken::new();
        let lv = GatewayLiveness::new(5000);

        let handle = spawn_result_listener_with_context(
            ctx.clone(),
            &endpoint,
            ZmqConfig::default(),
            control_tx,
            cancel.clone(),
            Some(lv.clone()),
        )
        .expect("spawn result listener");

        tokio::time::sleep(Duration::from_millis(50)).await;

        let wire = hft_protocol::order_wire::build_heartbeat_wire(42_000_000_000);
        let mut buf = [0u8; ORDER_RESULT_WIRE_SIZE];
        wire.encode(&mut buf);
        push.send(&buf[..], 0).expect("push heartbeat");

        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(lv.last_heartbeat_ns(), 42_000_000_000);
        assert!(control_rx.try_recv().is_err());

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn result_listener_decodes_wire_into_strategy_control() {
        let ctx = hft_zmq::Context::new();
        let endpoint = free_tcp_endpoint();
        let push = bind_push(&ctx, &endpoint);
        let (control_tx, control_rx) = crossbeam_channel::bounded(8);
        let cancel = CancellationToken::new();
        let handle = spawn_result_listener_with_context(
            ctx.clone(),
            &endpoint,
            ZmqConfig::default(),
            control_tx,
            cancel.clone(),
            None,
        )
        .expect("spawn result listener");

        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut wire = OrderResultWire {
            client_seq: 77,
            gateway_ts_ns: 123456,
            filled_size: 0,
            reject_code: 0,
            status: STATUS_ACCEPTED,
            _pad0: [0; 3],
            exchange_order_id: [0; 48],
            text_tag: [0; 32],
            _reserved: [0; 16],
        };
        wire.exchange_order_id[..6].copy_from_slice(b"ord-77");
        wire.text_tag[..2].copy_from_slice(b"v8");
        let mut buf = [0u8; hft_protocol::order_wire::ORDER_RESULT_WIRE_SIZE];
        wire.encode(&mut buf);
        push.send(&buf[..], 0).expect("push result wire");

        let ctrl = control_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("result control");
        match ctrl {
            StrategyControl::OrderResult(info) => {
                assert_eq!(info.client_seq, 77);
                assert_eq!(info.status, ResultStatus::Accepted);
                assert_eq!(info.exchange_order_id, "ord-77");
                assert_eq!(info.gateway_ts_ns, 123456);
                assert_eq!(info.text_tag, "v8");
            }
            other => panic!("unexpected control: {other:?}"),
        }

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn result_listener_rejected_wire_maps_to_rejected_status() {
        let ctx = hft_zmq::Context::new();
        let endpoint = free_tcp_endpoint();
        let push = bind_push(&ctx, &endpoint);
        let (control_tx, control_rx) = crossbeam_channel::bounded(8);
        let cancel = CancellationToken::new();
        let handle = spawn_result_listener_with_context(
            ctx.clone(),
            &endpoint,
            ZmqConfig::default(),
            control_tx,
            cancel.clone(),
            None,
        )
        .expect("spawn result listener");

        tokio::time::sleep(Duration::from_millis(50)).await;
        let wire = OrderResultWire {
            client_seq: 88,
            gateway_ts_ns: 999,
            filled_size: 0,
            reject_code: 0,
            status: STATUS_REJECTED,
            _pad0: [0; 3],
            exchange_order_id: [0; 48],
            text_tag: [0; 32],
            _reserved: [0; 16],
        };
        let mut buf = [0u8; hft_protocol::order_wire::ORDER_RESULT_WIRE_SIZE];
        wire.encode(&mut buf);
        push.send(&buf[..], 0).expect("push rejected result");

        let ctrl = control_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("rejected result control");
        match ctrl {
            StrategyControl::OrderResult(info) => {
                assert_eq!(info.client_seq, 88);
                assert_eq!(info.status, ResultStatus::Rejected);
            }
            other => panic!("unexpected control: {other:?}"),
        }

        cancel.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn end_to_end_zmq_request_and_result_roundtrip_accepted() {
        let dir = tempdir().unwrap();
        let symtab_path = dir.path().join("symtab-result-ok.bin");
        let order_endpoint = free_tcp_endpoint();
        let result_endpoint = free_tcp_endpoint();

        let symtab = Arc::new(SymbolTable::open_or_create(&symtab_path, 32).unwrap());
        let result_ctx = hft_zmq::Context::new();
        let result_push = bind_push(&result_ctx, &result_endpoint);
        let (req_tx, req_rx) = crossbeam_channel::bounded::<IngressEnvelope>(8);
        let (result_tx, result_rx) = crossbeam_channel::bounded::<OrderResultWire>(8);

        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(NoopExecutor::new(ExchangeId::Gate))),
        );
        let gateway = start_with_arc(
            Arc::new(routing),
            req_rx,
            None,
            Some(result_tx),
            DEDUP_CACHE_CAP_DEFAULT,
            RetryPolicy::default(),
        )
        .unwrap();

        let ingress = start_zmq_order_ingress(
            &order_endpoint,
            &ZmqConfig {
                order_ingress_bind: Some(order_endpoint.clone()),
                ..ZmqConfig::default()
            },
            symtab.clone(),
            req_tx,
            gateway.cancel.clone(),
        )
        .unwrap();

        let (control_tx, control_rx) = crossbeam_channel::bounded(8);
        let listener = spawn_result_listener(
            &result_endpoint,
            &ZmqConfig::default(),
            control_tx,
            gateway.cancel.clone(),
            None,
        )
        .unwrap();

        let mut cfg = base_cfg();
        cfg.shm.backend = ShmBackendKind::LegacyMultiFile;
        cfg.shm.symtab_path = symtab_path;
        cfg.shm.symbol_table_capacity = 32;
        cfg.order_egress.mode = OrderEgressMode::Zmq;
        cfg.order_egress.backpressure = BackpressurePolicy::RetryWithTimeout {
            max_retries: 64,
            backoff_ns: 1_000_000,
            total_timeout_ns: 500_000_000,
        };
        cfg.order_egress.result_zmq_connect = Some(result_endpoint);
        cfg.order_egress.zmq = Some(ZmqOrderEgressConfig {
            endpoint: order_endpoint,
            send_hwm: 8,
            linger_ms: 0,
            reconnect_interval_ms: 10,
            reconnect_interval_max_ms: 10,
        });

        let egress = build_drain_egress(&cfg).expect("build zmq drain egress");
        let (tx, rx) = crossbeam_channel::bounded(4);
        let drain = spawn_order_drain_with_now(rx, egress, gateway.cancel.clone(), || 456, None);

        tokio::time::sleep(Duration::from_millis(100)).await;
        tx.send((sample_req(), sample_seed())).unwrap();
        drop(tx);

        let wire = tokio::task::spawn_blocking(move || {
            result_rx.recv_timeout(Duration::from_millis(1500))
        })
        .await
        .unwrap()
        .expect("accepted result wire");
        assert_eq!(wire.client_seq, 7);
        assert_eq!(wire.status, STATUS_ACCEPTED);
        assert_eq!(&wire.text_tag[..2], b"v8");
        let mut buf = [0u8; hft_protocol::order_wire::ORDER_RESULT_WIRE_SIZE];
        wire.encode(&mut buf);
        result_push.send(&buf[..], 0).expect("send accepted result");

        let ctrl = control_rx
            .recv_timeout(Duration::from_millis(1500))
            .expect("accepted order result");
        match ctrl {
            StrategyControl::OrderResult(info) => {
                assert_eq!(info.client_seq, 7);
                assert_eq!(info.status, ResultStatus::Accepted);
                assert_eq!(info.text_tag, "v8");
                assert!(info.exchange_order_id.starts_with("noop-"));
            }
            other => panic!("unexpected control: {other:?}"),
        }

        gateway.shutdown();
        let _ = drain.await;
        let _ = ingress.await;
        let _ = listener.await;
        gateway.join().await;
    }

    #[tokio::test]
    async fn end_to_end_zmq_request_and_result_roundtrip_rejected() {
        let dir = tempdir().unwrap();
        let symtab_path = dir.path().join("symtab-result-reject.bin");
        let order_endpoint = free_tcp_endpoint();
        let result_endpoint = free_tcp_endpoint();

        let symtab = Arc::new(SymbolTable::open_or_create(&symtab_path, 32).unwrap());
        let result_ctx = hft_zmq::Context::new();
        let result_push = bind_push(&result_ctx, &result_endpoint);
        let (req_tx, req_rx) = crossbeam_channel::bounded::<IngressEnvelope>(8);
        let (result_tx, result_rx) = crossbeam_channel::bounded::<OrderResultWire>(8);

        let mut routing = RoutingTable::new();
        routing.insert(
            ExchangeId::Gate,
            Route::Rust(Arc::new(NoopExecutor::new(ExchangeId::Gate))),
        );
        let gateway = start_with_arc(
            Arc::new(routing),
            req_rx,
            None,
            Some(result_tx),
            DEDUP_CACHE_CAP_DEFAULT,
            RetryPolicy::default(),
        )
        .unwrap();

        let ingress = start_zmq_order_ingress(
            &order_endpoint,
            &ZmqConfig {
                order_ingress_bind: Some(order_endpoint.clone()),
                ..ZmqConfig::default()
            },
            symtab.clone(),
            req_tx,
            gateway.cancel.clone(),
        )
        .unwrap();

        let (control_tx, control_rx) = crossbeam_channel::bounded(8);
        let listener = spawn_result_listener(
            &result_endpoint,
            &ZmqConfig::default(),
            control_tx,
            gateway.cancel.clone(),
            None,
        )
        .unwrap();

        let mut cfg = base_cfg();
        cfg.shm.backend = ShmBackendKind::LegacyMultiFile;
        cfg.shm.symtab_path = symtab_path;
        cfg.shm.symbol_table_capacity = 32;
        cfg.order_egress.mode = OrderEgressMode::Zmq;
        cfg.order_egress.backpressure = BackpressurePolicy::RetryWithTimeout {
            max_retries: 64,
            backoff_ns: 1_000_000,
            total_timeout_ns: 500_000_000,
        };
        cfg.order_egress.result_zmq_connect = Some(result_endpoint);
        cfg.order_egress.zmq = Some(ZmqOrderEgressConfig {
            endpoint: order_endpoint,
            send_hwm: 8,
            linger_ms: 0,
            reconnect_interval_ms: 10,
            reconnect_interval_max_ms: 10,
        });

        let egress = build_drain_egress(&cfg).expect("build zmq drain egress");
        let (tx, rx) = crossbeam_channel::bounded(4);
        let drain = spawn_order_drain_with_now(rx, egress, gateway.cancel.clone(), || 789, None);

        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut req = sample_req();
        req.exchange = ExchangeId::Binance;
        tx.send((req, sample_seed())).unwrap();
        drop(tx);

        let wire = tokio::task::spawn_blocking(move || {
            result_rx.recv_timeout(Duration::from_millis(1500))
        })
        .await
        .unwrap()
        .expect("rejected result wire");
        assert_eq!(wire.client_seq, 7);
        assert_eq!(wire.status, STATUS_REJECTED);
        assert_eq!(&wire.text_tag[..2], b"v8");
        let mut buf = [0u8; hft_protocol::order_wire::ORDER_RESULT_WIRE_SIZE];
        wire.encode(&mut buf);
        result_push.send(&buf[..], 0).expect("send rejected result");

        let ctrl = control_rx
            .recv_timeout(Duration::from_millis(1500))
            .expect("rejected order result");
        match ctrl {
            StrategyControl::OrderResult(info) => {
                assert_eq!(info.client_seq, 7);
                assert_eq!(info.status, ResultStatus::Rejected);
                assert_eq!(info.text_tag, "v8");
                assert!(info.exchange_order_id.is_empty());
            }
            other => panic!("unexpected control: {other:?}"),
        }

        gateway.shutdown();
        let _ = drain.await;
        let _ = ingress.await;
        let _ = listener.await;
        gateway.join().await;
    }
}

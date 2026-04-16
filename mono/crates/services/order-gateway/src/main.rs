//! order-gateway 바이너리 엔트리포인트.
//!
//! # Phase 1 동작
//! 이 바이너리는 **standalone 운영용 thin wrapper** 다. 실제 E2E 에선 strategy 가
//! 같은 프로세스 안에 embed 되어 `crossbeam_channel::Sender<OrderRequest>` 를 들고
//! 주문을 밀어넣는다. 단독 실행 시에는 request 채널에 아무도 send 하지 않으므로
//! router 는 `recv_timeout` 만 돌며 cancel 을 기다린다 — 프로세스 supervisor /
//! ops 스크립트 배포 검증용.
//!
//! # 기동 순서 (SPEC.md §Startup 와 동일)
//! 1. `hft_config::load_all()` — defaults → TOML → env.
//! 2. `hft_telemetry::init()` — tracing subscriber 조립, handle 은 main scope 유지.
//! 3. 설정의 `cfg.exchanges` 를 순회하며 `ExchangeId` 집합을 만들어 각각에 대해
//!    env var (`HFT_{EX}_API_KEY` / `_API_SECRET` / `_PASSPHRASE`) 를 읽어
//!    실 executor (`GateExecutor` / `BinanceExecutor` / `BybitExecutor` /
//!    `BitgetExecutor` / `OkxExecutor`) 를 구성. 자격 없으면 `NoopExecutor` 로
//!    fallback (dev/dry-run 모드).
//! 4. 주문 채널 `crossbeam_channel::bounded::<IngressEnvelope>(hwm)` — sender 는
//!    embed 환경에서만 실제로 쓰이지만 standalone 에서도 disconnect 를 막기 위해
//!    `_req_tx` 를 main 의 스코프 끝까지 살려둔다.
//! 5. `order_gateway::start()` 호출 → `OrderGatewayHandle`.
//! 6. `cfg.shm.enabled` 면 SHM subscriber 기동, `cfg.zmq.order_ingress_bind=Some(..)` 면
//!    ZMQ PULL ingress task 기동. 둘 다 같은 request 채널로 fan-in.
//! 7. `cfg.zmq.result_egress_bind=Some(..)` 면 gateway 결과를 ZMQ PUSH(bind) 로 송신.
//! 7. ctrlc 핸들러 설치 — SIGINT/SIGTERM 수신 시 `handle.cancel.cancel()`.
//! 8. `handle.join().await`.
//!
//! # 안정성 메모
//! - mimalloc 을 global allocator 로 지정 (hot path 가 아니어도 지연 균일화).
//! - tokio multi-thread runtime. order-gateway 는 async place_order + retry sleep
//!   이 존재하므로 current-thread 로는 돌리지 않는다.
//! - 동일 `ExchangeId` 가 `cfg.exchanges` 에 중복으로 등장해도 HashSet 으로 한 번만
//!   등록. RoutingTable::insert 는 replace 시 warn 하지만 여기서는 그 이전에 skip.

#![deny(rust_2018_idioms)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use hft_exchange_api::{CancellationToken, ExchangeExecutor};
use hft_protocol::order_wire::{OrderResultWire, ORDER_RESULT_WIRE_SIZE};
use hft_exchange_rest::Credentials;
use hft_types::ExchangeId;
use hft_zmq::{Context as ZmqContext, SendOutcome};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use hft_config::AppConfig;
use order_gateway::shm_sub::{
    start_shm_order_subscriber, ShmOrderSubscriberHandle, ShmSubscriberConfig,
};
use order_gateway::zmq_ingress::{open_order_ingress_symtab, start_zmq_order_ingress};
use order_gateway::{
    start_with_arc, IngressEnvelope, NoopExecutor, Route, RoutingTable, RetryPolicy,
    DEDUP_CACHE_CAP_DEFAULT,
};

/// 전역 allocator: mimalloc (hot path 외에서도 지연 균일화).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// 설정 → telemetry 초기화.
///
/// hft-config 의 `TelemetryConfig` 를 hft-telemetry 의 구조체로 1:1 매핑.
fn init_telemetry(cfg: &AppConfig) -> Result<hft_telemetry::TelemetryHandle> {
    let tcfg = hft_telemetry::TelemetryConfig {
        otlp_endpoint: cfg.telemetry.otlp_endpoint.clone(),
        default_level: cfg.telemetry.default_level.clone(),
        json_logs: cfg.telemetry.stdout_json,
    };
    hft_telemetry::init(&tcfg, &cfg.service_name).context("telemetry init failed")
}

/// SIGINT/SIGTERM 수신 시 `cancel.cancel()` 호출.
///
/// 2번째 신호에서는 즉시 `exit(130)` — drain 이 멈춘 상황에서 사용자가 강제 종료를
/// 원한다고 판단.
fn install_signal_handler(cancel: CancellationToken) {
    let fired = Arc::new(AtomicBool::new(false));
    let install_result = ctrlc::set_handler(move || {
        if fired.swap(true, Ordering::SeqCst) {
            eprintln!("[order-gateway] second signal received — aborting");
            std::process::exit(130);
        }
        eprintln!("[order-gateway] shutdown signal received — draining");
        cancel.cancel();
    });
    if let Err(e) = install_result {
        warn!(error = %e, "failed to install ctrlc handler (already installed?)");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Credential loading (Phase 2)
// ─────────────────────────────────────────────────────────────────────────────

/// 환경변수 prefix. ops 가 shell/secret store 에서 같은 naming 으로 주입한다.
///
/// - `HFT_{EX}_API_KEY`      — 필수
/// - `HFT_{EX}_API_SECRET`   — 필수
/// - `HFT_{EX}_PASSPHRASE`   — OKX / Bitget 만
///
/// `{EX}` 는 `ExchangeId::as_str()` 의 대문자 (e.g. GATE, BINANCE, BYBIT, BITGET, OKX).
fn env_prefix(id: ExchangeId) -> String {
    format!("HFT_{}", id.as_str().to_ascii_uppercase())
}

/// non-empty env var 읽기. 빈 문자열은 `None` 으로 간주해 실수로 placeholder 를 쓰는
/// 사고를 방지.
fn read_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

/// 거래소별 `Credentials` 를 env 에서 로드. key 또는 secret 이 비어있으면 `None`.
/// passphrase 는 OKX/Bitget 에만 필수이며, 그 외는 존재해도 무시.
fn load_credentials(id: ExchangeId) -> Option<Credentials> {
    let prefix = env_prefix(id);
    let api_key = read_env(&format!("{prefix}_API_KEY"))?;
    let api_secret = read_env(&format!("{prefix}_API_SECRET"))?;
    let mut creds = Credentials::new(api_key, api_secret);
    if let Some(pp) = read_env(&format!("{prefix}_PASSPHRASE")) {
        creds = creds.with_passphrase(pp);
    }
    Some(creds)
}

/// 거래소별 실 executor 를 생성. 자격이 없거나 생성에 실패하면 `None` 반환 →
/// caller 가 NoopExecutor 로 fallback.
///
/// 정상 경로 이외에서는 반드시 `warn!` 으로 경고 남긴다. 주문을 실제로 보내지 못할
/// 거래소에 대해서는 ops 가 즉시 알아차려야 하기 때문.
fn load_executor(id: ExchangeId) -> Option<Arc<dyn ExchangeExecutor>> {
    let creds = match load_credentials(id) {
        Some(c) => c,
        None => {
            warn!(
                exchange = ?id,
                "credentials missing — falling back to NoopExecutor (dev/dry-run mode)"
            );
            return None;
        }
    };

    // 각 executor 의 new() 는 Credentials::validate() + passphrase 체크를 수행한다.
    // 명시적 trait object coercion: `Arc<ConcreteExec>` → `Arc<dyn ExchangeExecutor>`.
    let result: Result<Arc<dyn ExchangeExecutor>, hft_exchange_api::ApiError> = match id {
        ExchangeId::Gate => hft_exchange_gate::GateExecutor::new(creds)
            .map(|e| Arc::new(e) as Arc<dyn ExchangeExecutor>),
        ExchangeId::Binance => hft_exchange_binance::BinanceExecutor::new(creds)
            .map(|e| Arc::new(e) as Arc<dyn ExchangeExecutor>),
        ExchangeId::Bybit => hft_exchange_bybit::BybitExecutor::new(creds)
            .map(|e| Arc::new(e) as Arc<dyn ExchangeExecutor>),
        ExchangeId::Bitget => hft_exchange_bitget::BitgetExecutor::new(creds)
            .map(|e| Arc::new(e) as Arc<dyn ExchangeExecutor>),
        ExchangeId::Okx => hft_exchange_okx::OkxExecutor::new(creds)
            .map(|e| Arc::new(e) as Arc<dyn ExchangeExecutor>),
    };

    match result {
        Ok(e) => Some(e),
        Err(e) => {
            warn!(
                exchange = ?id,
                error = %e,
                "executor build failed — falling back to NoopExecutor"
            );
            None
        }
    }
}

/// `cfg.exchanges` 에서 등장하는 모든 고유 `ExchangeId` 에 대해 실 executor 를
/// 등록한 `RoutingTable` 을 만든다. 자격이 없는 거래소는 `NoopExecutor` 로 fallback.
///
/// 동일 ID 가 여러 role (Primary / WebOrderBook) 로 등장하는 케이스는 한 번만 등록.
fn build_routing_table(cfg: &AppConfig) -> RoutingTable {
    let mut routing = RoutingTable::new();
    let mut seen: std::collections::HashSet<ExchangeId> = std::collections::HashSet::new();
    for ex in &cfg.exchanges {
        if !seen.insert(ex.id) {
            continue;
        }
        match load_executor(ex.id) {
            Some(exec) => {
                let label = exec.label();
                routing.insert(ex.id, Route::Rust(exec));
                info!(
                    exchange = ?ex.id,
                    executor = %label,
                    "routing: registered real ExchangeExecutor"
                );
            }
            None => {
                routing.insert(ex.id, Route::Rust(Arc::new(NoopExecutor::new(ex.id))));
                info!(
                    exchange = ?ex.id,
                    "routing: registered NoopExecutor (no credentials)"
                );
            }
        }
    }
    routing
}

/// `cfg.shm.enabled` 면 Python strategy → Rust 주문 수신을 위한 SHM subscriber 를 기동.
///
/// 실패는 `warn` 로 남기고 `None` 반환 — order-gateway 는 ZMQ 경로만으로도 동작해야
/// 하기 때문. 상위 caller 가 반환값의 `Some/None` 을 보고 handle 을 scope 에 유지할지
/// 결정한다.
fn build_shm_subscriber(
    cfg: &AppConfig,
    routing: Arc<RoutingTable>,
    req_tx: crossbeam_channel::Sender<IngressEnvelope>,
    rt: tokio::runtime::Handle,
    cancel: hft_exchange_api::CancellationToken,
) -> Option<ShmOrderSubscriberHandle> {
    if !cfg.shm.enabled {
        info!("SHM order ingress disabled by config");
        return None;
    }

    // v1 legacy vs v2 shared region 분기 — hft-config 의 backend 필드로 결정.
    let sub_cfg = match cfg.shm.backend {
        hft_config::ShmBackendKind::LegacyMultiFile => {
            info!(
                order_path = ?cfg.shm.order_path,
                symtab_path = ?cfg.shm.symtab_path,
                "SHM subscriber: v1 legacy single-ring mode"
            );
            ShmSubscriberConfig::legacy(
                cfg.shm.order_path.clone(),
                cfg.shm.symtab_path.clone(),
            )
        }
        backend @ (hft_config::ShmBackendKind::DevShm
        | hft_config::ShmBackendKind::Hugetlbfs
        | hft_config::ShmBackendKind::PciBar) => {
            let backing = match backend {
                hft_config::ShmBackendKind::DevShm => hft_shm::Backing::DevShm {
                    path: cfg.shm.shared_path.clone(),
                },
                hft_config::ShmBackendKind::Hugetlbfs => hft_shm::Backing::Hugetlbfs {
                    path: cfg.shm.shared_path.clone(),
                },
                hft_config::ShmBackendKind::PciBar => hft_shm::Backing::PciBar {
                    path: cfg.shm.shared_path.clone(),
                },
                hft_config::ShmBackendKind::LegacyMultiFile => unreachable!(),
            };
            let spec = hft_shm::LayoutSpec {
                quote_slot_count: cfg.shm.quote_slot_count,
                trade_ring_capacity: cfg.shm.trade_ring_capacity,
                symtab_capacity: cfg.shm.symbol_table_capacity,
                order_ring_capacity: cfg.shm.order_ring_capacity,
                n_max: cfg.shm.n_max,
            };
            info!(
                shared_path = ?cfg.shm.shared_path,
                backend = ?backend,
                n_max = cfg.shm.n_max,
                "SHM subscriber: v2 multi-vm fan-in mode"
            );
            ShmSubscriberConfig::shared_region(backing, spec)
        }
    };

    match start_shm_order_subscriber(&sub_cfg, routing, req_tx, rt, cancel) {
        Ok(h) => {
            info!("SHM order subscriber started (Python → Rust ingress)");
            Some(h)
        }
        Err(e) => {
            warn!(
                error = %format!("{e:#}"),
                "SHM order subscriber failed to start — ZMQ path only"
            );
            None
        }
    }
}

/// `cfg.zmq.order_ingress_bind` 가 설정되면 ZMQ PULL ingress 를 시작한다.
fn build_zmq_ingress(
    cfg: &AppConfig,
    req_tx: crossbeam_channel::Sender<IngressEnvelope>,
    cancel: hft_exchange_api::CancellationToken,
) -> Option<JoinHandle<()>> {
    let Some(endpoint) = cfg.zmq.order_ingress_bind.as_deref() else {
        info!("ZMQ order ingress disabled by config");
        return None;
    };

    let symtab = match open_order_ingress_symtab(cfg) {
        Ok(symtab) => symtab,
        Err(e) => {
            warn!(
                error = %format!("{e:#}"),
                "ZMQ order ingress symtab open failed — SHM path only"
            );
            return None;
        }
    };

    match start_zmq_order_ingress(endpoint, &cfg.zmq, symtab, req_tx, cancel) {
        Ok(handle) => {
            info!(endpoint, "ZMQ order ingress started");
            Some(handle)
        }
        Err(e) => {
            warn!(
                endpoint,
                error = %format!("{e:#}"),
                "ZMQ order ingress failed to start — SHM path only"
            );
            None
        }
    }
}

fn build_zmq_result_egress(
    cfg: &AppConfig,
    result_rx: crossbeam_channel::Receiver<OrderResultWire>,
    cancel: hft_exchange_api::CancellationToken,
) -> Option<JoinHandle<()>> {
    let Some(endpoint) = cfg.zmq.result_egress_bind.as_deref() else {
        info!("ZMQ order result egress disabled by config");
        return None;
    };

    let endpoint = endpoint.to_string();
    let zmq_cfg = cfg.zmq.clone();
    Some(tokio::task::spawn_blocking(move || {
        fn send_result_with_retry(
            push: &hft_zmq::PushSocket,
            payload: &[u8],
            client_seq: u64,
            cancel: &hft_exchange_api::CancellationToken,
        ) {
            for attempt in 0..100 {
                if cancel.is_cancelled() {
                    return;
                }
                match push.send_bytes_dontwait(payload) {
                    SendOutcome::Sent => return,
                    SendOutcome::WouldBlock => {
                        if attempt == 99 {
                            warn!(
                                target: "order_gateway::result_egress",
                                client_seq,
                                "result PUSH socket still not writable after retry budget — dropping result wire"
                            );
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    SendOutcome::Error(e) => {
                        warn!(
                            target: "order_gateway::result_egress",
                            client_seq,
                            error = ?e,
                            "result PUSH socket send failed"
                        );
                        return;
                    }
                }
            }
        }

        let ctx = ZmqContext::new();
        let push = match ctx.push_bind(&endpoint, &zmq_cfg) {
            Ok(sock) => sock,
            Err(e) => {
                warn!(
                    endpoint,
                    error = %format!("{e:#}"),
                    "ZMQ order result egress failed to bind"
                );
                return;
            }
        };
        info!(endpoint, "ZMQ order result egress started");

        loop {
            if cancel.is_cancelled() {
                break;
            }

            match result_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(wire) => {
                    let mut buf = [0u8; ORDER_RESULT_WIRE_SIZE];
                    wire.encode(&mut buf);
                    send_result_with_retry(&push, &buf, wire.client_seq, &cancel);
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }

        info!(target: "order_gateway::result_egress", "ZMQ order result egress stopped");
    }))
}

/// 실제 run 로직. 에러는 main 에서 exitcode 로 변환.
async fn run() -> Result<()> {
    // 1) config
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;

    // 2) telemetry — main scope 까지 살아있어야 OTel flush 정상 동작.
    let _tele = init_telemetry(&cfg)?;

    info!(
        service = %cfg.service_name,
        exchanges = cfg.exchanges.len(),
        hwm = cfg.zmq.hwm,
        shm_enabled = cfg.shm.enabled,
        "order-gateway starting (Phase 2: real executors + SHM ingress)"
    );

    // 3) routing — Arc 로 감싸 Router/SHM subscriber 가 공유.
    let routing = Arc::new(build_routing_table(&cfg));
    if routing.is_empty() {
        return Err(anyhow!(
            "order-gateway requires at least one exchange in cfg.exchanges"
        ));
    }

    // 4) request 채널. hwm 은 i32 → usize (음수 방어), 0 도 1 로 승격해 bounded(0) 회피.
    //    standalone 에서는 SHM subscriber 만 producer 이므로 `_req_tx` 를 main 스코프
    //    끝까지 보관해 router 가 Disconnected 로 조기 종료되는 상황을 막는다. SHM 가
    //    off 라면 단순히 idle 대기.
    let cap = cfg.zmq.hwm.max(1) as usize;
    let (req_tx, req_rx) = crossbeam_channel::bounded::<IngressEnvelope>(cap);
    let (result_tx, result_rx) = match cfg.zmq.result_egress_bind.as_deref() {
        Some(_) => {
            let (tx, rx) = crossbeam_channel::bounded::<OrderResultWire>(cap);
            (Some(tx), Some(rx))
        }
        None => (None, None),
    };

    // 5) router start — ack 소비자는 없으므로 None.
    let handle = start_with_arc(
        routing.clone(),
        req_rx,
        None,
        result_tx,
        DEDUP_CACHE_CAP_DEFAULT,
        RetryPolicy::default(),
    )
    .context("order_gateway::start_with_arc failed")?;

    // 6) SHM subscriber — 성공/실패 모두 non-fatal. handle 은 run 스코프 끝까지 유지.
    let shm_handle = build_shm_subscriber(
        &cfg,
        routing.clone(),
        req_tx.clone(),
        tokio::runtime::Handle::current(),
        handle.cancel.clone(),
    );
    let zmq_handle = build_zmq_ingress(&cfg, req_tx.clone(), handle.cancel.clone());
    let result_handle = result_rx
        .map(|rx| build_zmq_result_egress(&cfg, rx, handle.cancel.clone()))
        .flatten();

    // 7) signal handler (CancellationToken 만 clone 해서 전달 — handle 은 join 으로 소비).
    install_signal_handler(handle.cancel.clone());

    // 8) router task 종료까지 blocking.
    handle.join().await;

    // 9) SHM subscriber 정리.
    if let Some(h) = shm_handle {
        h.join();
    }
    if let Some(h) = zmq_handle {
        let _ = h.await;
    }
    if let Some(h) = result_handle {
        let _ = h.await;
    }

    info!("order-gateway exited cleanly");
    // req_tx 는 여기서 drop — 이미 router 종료된 이후라 영향 없음.
    drop(req_tx);
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[order-gateway] fatal: {e:#}");
            error!(error = %format!("{e:#}"), "order-gateway fatal error");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn free_tcp_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("tcp listener");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        format!("tcp://127.0.0.1:{port}")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zmq_result_egress_sends_order_result_wire() {
        let endpoint = free_tcp_endpoint();
        let mut cfg = AppConfig::default();
        cfg.zmq.result_egress_bind = Some(endpoint.clone());

        let ctx = ZmqContext::new();
        let mut pull = ctx
            .pull_connect(&endpoint, &cfg.zmq)
            .expect("pull connect result endpoint");
        let (tx, rx) = crossbeam_channel::bounded::<OrderResultWire>(4);
        let cancel = CancellationToken::new();
        let handle = build_zmq_result_egress(&cfg, rx, cancel.clone()).expect("result egress");

        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut wire = OrderResultWire {
            client_seq: 42,
            gateway_ts_ns: 1234,
            filled_size: 0,
            reject_code: 0,
            status: hft_protocol::order_wire::STATUS_ACCEPTED,
            _pad0: [0; 3],
            exchange_order_id: [0; 48],
            text_tag: [0; 32],
            _reserved: [0; 16],
        };
        wire.exchange_order_id[..6].copy_from_slice(b"ord-42");
        wire.text_tag[..2].copy_from_slice(b"v8");
        tx.send(wire).expect("send result wire");

        let mut bytes = None;
        let deadline = std::time::Instant::now() + Duration::from_millis(1000);
        while bytes.is_none() && std::time::Instant::now() < deadline {
            bytes = pull.recv_bytes_timeout(100).expect("recv bytes");
        }
        let bytes = bytes.expect("result payload");
        let buf: [u8; ORDER_RESULT_WIRE_SIZE] = bytes.try_into().expect("128B result wire");
        let decoded = OrderResultWire::decode(&buf).expect("decode result wire");
        assert_eq!(decoded.client_seq, 42);
        assert_eq!(decoded.gateway_ts_ns, 1234);
        assert_eq!(&decoded.text_tag[..2], b"v8");

        cancel.cancel();
        drop(tx);
        let _ = handle.await;
    }
}

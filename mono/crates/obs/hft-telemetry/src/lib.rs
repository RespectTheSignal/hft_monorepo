//! hft-telemetry — tracing + OTel + HDR histogram + counters.
//!
//! # 설계 원칙
//! - **Hot path 절대 불변**: `record_stage_nanos`, counter inc 는 lock-free atomic
//!   또는 very-short critical section. tracing 매크로는 off-hot.
//! - **비동기 로깅**: fmt layer 는 non_blocking writer (tracing-appender). hot thread 는
//!   MPSC send 1회, bg thread 가 실제 write.
//! - **verbose-trace feature**: `trace_verbose!` 가 off 기본 → `cargo build --release`
//!   바이너리에는 trace! 호출이 완전히 사라짐.
//!
//! # 공개 API
//! - [`init`]: tracing + otel subscriber 조립, `TelemetryHandle` 반환 (drop 시 flush).
//! - [`record_stage_nanos`] / [`dump_hdr`]: HDR histogram.
//! - [`counter_inc`] / [`counters_snapshot`]: prometheus-친화 atomic counter.
//! - [`gauge_set`] / [`gauges_snapshot`]: health/metrics endpoint 용 gauge.
//! - [`pin_current_thread`] / [`next_hot_core`]: 리눅스 CPU affinity.
//! - [`trace_verbose!`] 매크로: feature-gated `tracing::trace!`.

#![deny(rust_2018_idioms)]

pub mod prometheus;

use anyhow::{anyhow, Result};
use hdrhistogram::Histogram;
use hft_time::Stage;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use tracing_subscriber::prelude::*;

// ─────────────────────────────────────────────────────────────────────────────
// init / TelemetryHandle
// ─────────────────────────────────────────────────────────────────────────────

/// init() 호출 중복 방지.
static INIT_DONE: AtomicBool = AtomicBool::new(false);

/// telemetry 초기화 설정.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// OTLP exporter endpoint. None 이면 OTel 비활성.
    pub otlp_endpoint: Option<String>,
    /// 기본 log level. env `HFT_LOG` 이 있으면 그것이 우선.
    pub default_level: String,
    /// JSON vs Pretty.
    pub json_logs: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: None,
            default_level: "info".into(),
            json_logs: false,
        }
    }
}

/// init() 에서 반환되는 핸들. Drop 시 OTel flush.
#[must_use = "TelemetryHandle must be held until process shutdown"]
pub struct TelemetryHandle {
    #[allow(dead_code)]
    service_name: String,
    /// OTLP 활성화 시 생성한 provider. Drop 시 남은 span flush 를 위해 shutdown 한다.
    otlp_provider: Option<opentelemetry_sdk::trace::TracerProvider>,
}

impl Drop for TelemetryHandle {
    fn drop(&mut self) {
        if let Some(provider) = &self.otlp_provider {
            // BatchSpanProcessor 가 backend 로 남은 span 을 flush 하도록 block.
            // provider 직접 shutdown 하여 global provider 교체 여부와 무관하게 flush 한다.
            if let Err(err) = provider.shutdown() {
                eprintln!("[hft-telemetry] tracer provider shutdown failed: {err}");
            }
        }
    }
}

/// tracing 서브스크라이버 조립. 프로세스 시작 시 1회만 호출.
///
/// # 동작
/// - fmt layer: stdout (JSON 또는 pretty). `with_target + with_thread_ids` 로 hot-path
///   스레드 디버깅을 돕는다.
/// - EnvFilter: default `cfg.default_level`, env `HFT_LOG` override.
/// - `cfg.otlp_endpoint` 가 `Some` 이면 **OTLP gRPC exporter + BatchSpanProcessor**
///   을 구성해 tracing-opentelemetry layer 로 연결. 실패 시 warn 만 남기고 계속
///   진행 (fmt 레이어는 동작) — observability 가 hot path 를 죽이면 안 되기 때문.
///
/// # 주의
/// OTLP 경로는 tokio runtime 위에서 호출되어야 한다 (BatchSpanProcessor 의
/// runtime::Tokio). 런타임 없이 호출되면 exporter init 이 실패 → warn 후 fmt 만
/// 남고 계속.
pub fn init(cfg: &TelemetryConfig, service_name: &str) -> Result<TelemetryHandle> {
    if INIT_DONE.swap(true, Ordering::SeqCst) {
        return Err(anyhow!("hft-telemetry already initialized"));
    }

    let env_filter = tracing_subscriber::EnvFilter::try_from_env("HFT_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(cfg.default_level.clone()));

    // OTLP tracer/provider 를 조건부로 생성. 성공 시 Some, 실패/비활성 시 None.
    // `tracing_opentelemetry::layer().with_tracer(tracer)` 가 Layer<S> 를 구현하는
    // `OpenTelemetryLayer<S, Tracer>` 를 만들어주며, `S` 는 `.with()` 호출 시점에
    // 추론된다. `Option<L: Layer<S>> : Layer<S>` 이므로 `None` 은 no-op layer 로 동작.
    let otlp = build_otlp_tracer(cfg.otlp_endpoint.as_deref(), service_name);
    let otlp_active = otlp.is_some();

    // 주의: Phase 1 은 blocking stdout. Phase 2 에서 tracing-appender::non_blocking 으로
    // 교체 고려. hot path 는 tracing::trace! 를 쓰지 않으므로 현재는 병목이 아니다.
    let res = if cfg.json_logs {
        let fmt_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_target(true)
            .with_thread_ids(true);
        let otel_layer = otlp
            .as_ref()
            .map(|t| tracing_opentelemetry::layer().with_tracer(t.tracer.clone()));
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .with(otel_layer)
            .try_init()
    } else {
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_thread_ids(true);
        let otel_layer = otlp
            .as_ref()
            .map(|t| tracing_opentelemetry::layer().with_tracer(t.tracer.clone()));
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .with(otel_layer)
            .try_init()
    };
    res.map_err(|e| anyhow!("tracing subscriber init: {e}"))?;

    if otlp_active {
        tracing::info!(
            otlp_endpoint = %cfg.otlp_endpoint.as_deref().unwrap_or(""),
            service = %service_name,
            "OTLP exporter initialized"
        );
    }

    Ok(TelemetryHandle {
        service_name: service_name.to_owned(),
        otlp_provider: otlp.map(|t| t.provider),
    })
}

struct OtlpTracer {
    provider: opentelemetry_sdk::trace::TracerProvider,
    tracer: opentelemetry_sdk::trace::Tracer,
}

/// OTLP gRPC exporter + BatchSpanProcessor 를 띄우고 `TracerProvider + Tracer` 반환.
///
/// - `endpoint` 가 `None` 또는 빈 문자열이면 `None` (비활성).
/// - 내부적으로 `install_batch(runtime::Tokio)` 를 사용하므로 현재 thread 는
///   tokio runtime 위에 있어야 한다. 없으면 실패 → warn 후 `None`.
/// - `TelemetryHandle::Drop` 에서 provider 를 직접 shutdown 하여 flush 를 보장한다.
fn build_otlp_tracer(endpoint: Option<&str>, service_name: &str) -> Option<OtlpTracer> {
    let ep = endpoint?.trim();
    if ep.is_empty() {
        return None;
    }

    use opentelemetry::trace::TracerProvider as _; // `.tracer()` 메서드용 trait import
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::{runtime, trace as sdktrace, Resource};

    let exporter = opentelemetry_otlp::new_exporter().tonic().with_endpoint(ep);

    let trace_config =
        sdktrace::Config::default().with_resource(Resource::new(vec![KeyValue::new(
            "service.name",
            service_name.to_owned(),
        )]));

    // opentelemetry_otlp 0.17+ 에서 `install_batch` 는 `TracerProvider` 를 반환한다.
    // `tracing-opentelemetry::layer().with_tracer(..)` 는 `Tracer` 를 요구하므로
    // provider 에서 named tracer 를 뽑아낸다. (service_name 을 그대로 tracer name 으로 사용)
    match opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter)
        .with_trace_config(trace_config)
        .install_batch(runtime::Tokio)
    {
        Ok(provider) => {
            let tracer = provider.tracer(service_name.to_owned());
            Some(OtlpTracer { provider, tracer })
        }
        Err(e) => {
            // tracing 이 아직 init 안 됐을 수 있으므로 stderr 로 병행 출력.
            eprintln!(
                "[hft-telemetry] OTLP exporter init failed (endpoint={ep}): {e}. Continuing without OTLP."
            );
            None
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// HDR Histogram — stage 별 latency 집계
// ─────────────────────────────────────────────────────────────────────────────

/// HDR 기본 설정: 1ns ~ 60s 범위, 3-digit precision.
/// `new_with_bounds` 는 `low=1` 부터 시작해야 함 (nanos 단위).
const HDR_LOW_NS: u64 = 1;
const HDR_HIGH_NS: u64 = 60 * 1_000_000_000; // 60s
const HDR_SIGFIG: u8 = 3;

struct StageHistograms {
    // Stage::ExchangeServer .. Stage::Consumed 7개.
    slots: [Mutex<Histogram<u64>>; 7],
}

impl StageHistograms {
    fn new() -> Self {
        let mk = || {
            Mutex::new(
                Histogram::<u64>::new_with_bounds(HDR_LOW_NS, HDR_HIGH_NS, HDR_SIGFIG)
                    .expect("hdr init"),
            )
        };
        Self {
            slots: [mk(), mk(), mk(), mk(), mk(), mk(), mk()],
        }
    }

    fn idx(stage: Stage) -> usize {
        match stage {
            Stage::ExchangeServer => 0,
            Stage::WsReceived => 1,
            Stage::Serialized => 2,
            Stage::Pushed => 3,
            Stage::Published => 4,
            Stage::Subscribed => 5,
            Stage::Consumed => 6,
        }
    }
}

fn stage_hdrs() -> &'static StageHistograms {
    use std::sync::OnceLock;
    static INSTANCE: OnceLock<StageHistograms> = OnceLock::new();
    INSTANCE.get_or_init(StageHistograms::new)
}

/// stage 간 delta 를 nanos 로 기록. lock-free 는 아니지만 uncontended 시 < 50ns.
///
/// 값이 HDR 범위를 벗어나면 `saturate`.
pub fn record_stage_nanos(stage: Stage, nanos: u64) {
    let idx = StageHistograms::idx(stage);
    let hdrs = stage_hdrs();
    let clamped = nanos.clamp(HDR_LOW_NS, HDR_HIGH_NS);
    let mut g = hdrs.slots[idx].lock();
    // record_correct 는 coordinated omission 보정. 여기서는 단순 record.
    let _ = g.record(clamped);
}

/// HDR snapshot: 1 stage.
#[derive(Debug, Clone)]
pub struct HdrSnapshot {
    /// 총 샘플 수.
    pub count: u64,
    /// 최소 nanos.
    pub min: u64,
    /// 최대 nanos.
    pub max: u64,
    /// 평균 nanos.
    pub mean: f64,
    /// p50.
    pub p50: u64,
    /// p90.
    pub p90: u64,
    /// p99.
    pub p99: u64,
    /// p99.9.
    pub p999: u64,
    /// p99.99.
    pub p9999: u64,
}

impl HdrSnapshot {
    fn from(h: &Histogram<u64>) -> Self {
        Self {
            count: h.len(),
            min: h.min(),
            max: h.max(),
            mean: h.mean(),
            p50: h.value_at_quantile(0.50),
            p90: h.value_at_quantile(0.90),
            p99: h.value_at_quantile(0.99),
            p999: h.value_at_quantile(0.999),
            p9999: h.value_at_quantile(0.9999),
        }
    }
}

/// stage 별 HDR snapshot 을 반환 (모든 stage).
pub fn dump_hdr() -> [(Stage, HdrSnapshot); 7] {
    const STAGES: [Stage; 7] = [
        Stage::ExchangeServer,
        Stage::WsReceived,
        Stage::Serialized,
        Stage::Pushed,
        Stage::Published,
        Stage::Subscribed,
        Stage::Consumed,
    ];
    let hdrs = stage_hdrs();
    std::array::from_fn(|i| {
        let g = hdrs.slots[i].lock();
        (STAGES[i], HdrSnapshot::from(&g))
    })
}

/// 테스트용: HDR 전체 초기화. 프로덕션에서 호출하면 안 됨.
pub fn reset_hdr_for_test() {
    let hdrs = stage_hdrs();
    for slot in &hdrs.slots {
        slot.lock().reset();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Atomic gauges — health/metrics endpoint 용
// ─────────────────────────────────────────────────────────────────────────────

/// gauge 키. counter 와 달리 값이 증감할 수 있다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum GaugeKey {
    /// 서비스 시작 후 경과 시간 (초).
    UptimeSeconds,
    /// 마지막 heartbeat 수신 후 경과 시간 (ms).
    LastHeartbeatAgeMs,
    /// 현재 활성 strategy 수.
    ActiveStrategies,
}

struct Gauges {
    uptime_seconds: AtomicI64,
    last_heartbeat_age_ms: AtomicI64,
    active_strategies: AtomicI64,
}

fn gauges() -> &'static Gauges {
    use std::sync::OnceLock;
    static INSTANCE: OnceLock<Gauges> = OnceLock::new();
    INSTANCE.get_or_init(|| Gauges {
        uptime_seconds: AtomicI64::new(0),
        last_heartbeat_age_ms: AtomicI64::new(0),
        active_strategies: AtomicI64::new(0),
    })
}

/// 알려진 gauge 값을 설정한다.
#[inline]
pub fn gauge_set(k: GaugeKey, v: i64) {
    let g = gauges();
    let target = match k {
        GaugeKey::UptimeSeconds => &g.uptime_seconds,
        GaugeKey::LastHeartbeatAgeMs => &g.last_heartbeat_age_ms,
        GaugeKey::ActiveStrategies => &g.active_strategies,
    };
    target.store(v, Ordering::Relaxed);
}

/// 알려진 gauge 값을 읽는다.
#[inline]
pub fn gauge_get(k: GaugeKey) -> i64 {
    let g = gauges();
    let target = match k {
        GaugeKey::UptimeSeconds => &g.uptime_seconds,
        GaugeKey::LastHeartbeatAgeMs => &g.last_heartbeat_age_ms,
        GaugeKey::ActiveStrategies => &g.active_strategies,
    };
    target.load(Ordering::Relaxed)
}

/// 모든 gauge snapshot.
pub fn gauges_snapshot() -> Vec<(String, i64)> {
    let g = gauges();
    vec![
        (
            "uptime_seconds".into(),
            g.uptime_seconds.load(Ordering::Relaxed),
        ),
        (
            "last_heartbeat_age_ms".into(),
            g.last_heartbeat_age_ms.load(Ordering::Relaxed),
        ),
        (
            "active_strategies".into(),
            g.active_strategies.load(Ordering::Relaxed),
        ),
    ]
}

// ─────────────────────────────────────────────────────────────────────────────
// Atomic counters — prometheus 친화적, hot-path safe
// ─────────────────────────────────────────────────────────────────────────────

/// 고정 key counter — 이름은 `&'static str` 만 허용해 hot path 에서 format 방지.
struct Counters {
    // 기명 counter 는 고정 슬롯 — 동적 key 는 Mutex<HashMap> 경로.
    zmq_dropped: AtomicU64,
    zmq_hwm_block: AtomicU64,
    pipeline_event: AtomicU64,
    serialization_fail: AtomicU64,
    decode_fail: AtomicU64,
    // SHM fast path (Phase 2 Track C).
    shm_quote_published: AtomicU64,
    shm_quote_dropped: AtomicU64,
    shm_trade_published: AtomicU64,
    shm_trade_lap_drop: AtomicU64,
    shm_order_published: AtomicU64,
    shm_order_full_drop: AtomicU64,
    shm_intern_fail: AtomicU64,
    // order-gateway SHM subscriber 측 (Python strategy → Rust 소비 경로).
    shm_order_consumed: AtomicU64,
    shm_order_invalid: AtomicU64,
    shm_order_cancel_dispatched: AtomicU64,
    // v2 multi-VM 전용.
    shm_publisher_stale: AtomicU64,
    shm_order_batch: AtomicU64,
    shm_backoff_park: AtomicU64,
    // strategy order egress vocabulary (Phase 2 E / Step 3).
    order_egress_zmq_ok: AtomicU64,
    order_egress_backpressure_dropped: AtomicU64,
    order_egress_backpressure_retried: AtomicU64,
    order_egress_backpressure_retry_exhausted: AtomicU64,
    order_egress_backpressure_blocked: AtomicU64,
    order_egress_backpressure_block_timeout: AtomicU64,
    order_egress_zmq_fallback_activated: AtomicU64,
    order_egress_serialize_error: AtomicU64,
    // order-gateway vocabulary (Phase 2 E / Step 3).
    order_gateway_routed_ok: AtomicU64,
    order_gateway_router_queue_full: AtomicU64,
    order_gateway_exchange_not_found: AtomicU64,
    order_gateway_unknown_symbol: AtomicU64,
    // strategy result back-channel vocabulary (Phase 2 E / Step 8).
    order_result_received: AtomicU64,
    order_result_rejected: AtomicU64,
    // cleanup batch vocabulary.
    strategy_account_poll_err: AtomicU64,
    strategy_control_dropped: AtomicU64,
    strategy_order_channel_full: AtomicU64,
    order_gateway_invalid_wire: AtomicU64,
    gateway_heartbeat_emitted: AtomicU64,
    strategy_gateway_stale: AtomicU64,
    supervisor_restart: AtomicU64,
    // 기타 ad-hoc 용 — rare.
    extras: Mutex<ahash::AHashMap<&'static str, Arc<AtomicU64>>>,
}

fn counters() -> &'static Counters {
    use std::sync::OnceLock;
    static INSTANCE: OnceLock<Counters> = OnceLock::new();
    INSTANCE.get_or_init(|| Counters {
        zmq_dropped: AtomicU64::new(0),
        zmq_hwm_block: AtomicU64::new(0),
        pipeline_event: AtomicU64::new(0),
        serialization_fail: AtomicU64::new(0),
        decode_fail: AtomicU64::new(0),
        shm_quote_published: AtomicU64::new(0),
        shm_quote_dropped: AtomicU64::new(0),
        shm_trade_published: AtomicU64::new(0),
        shm_trade_lap_drop: AtomicU64::new(0),
        shm_order_published: AtomicU64::new(0),
        shm_order_full_drop: AtomicU64::new(0),
        shm_intern_fail: AtomicU64::new(0),
        shm_order_consumed: AtomicU64::new(0),
        shm_order_invalid: AtomicU64::new(0),
        shm_order_cancel_dispatched: AtomicU64::new(0),
        shm_publisher_stale: AtomicU64::new(0),
        shm_order_batch: AtomicU64::new(0),
        shm_backoff_park: AtomicU64::new(0),
        order_egress_zmq_ok: AtomicU64::new(0),
        order_egress_backpressure_dropped: AtomicU64::new(0),
        order_egress_backpressure_retried: AtomicU64::new(0),
        order_egress_backpressure_retry_exhausted: AtomicU64::new(0),
        order_egress_backpressure_blocked: AtomicU64::new(0),
        order_egress_backpressure_block_timeout: AtomicU64::new(0),
        order_egress_zmq_fallback_activated: AtomicU64::new(0),
        order_egress_serialize_error: AtomicU64::new(0),
        order_gateway_routed_ok: AtomicU64::new(0),
        order_gateway_router_queue_full: AtomicU64::new(0),
        order_gateway_exchange_not_found: AtomicU64::new(0),
        order_gateway_unknown_symbol: AtomicU64::new(0),
        order_result_received: AtomicU64::new(0),
        order_result_rejected: AtomicU64::new(0),
        strategy_account_poll_err: AtomicU64::new(0),
        strategy_control_dropped: AtomicU64::new(0),
        strategy_order_channel_full: AtomicU64::new(0),
        order_gateway_invalid_wire: AtomicU64::new(0),
        gateway_heartbeat_emitted: AtomicU64::new(0),
        strategy_gateway_stale: AtomicU64::new(0),
        supervisor_restart: AtomicU64::new(0),
        extras: Mutex::new(ahash::AHashMap::default()),
    })
}

/// 알려진 counter 이름. 새로운 이름은 여기에 등록한 뒤 매칭.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterKey {
    /// ZMQ queue full 로 drop 된 이벤트 수.
    ZmqDropped,
    /// ZMQ HWM blocking 발생 횟수.
    ZmqHwmBlock,
    /// 파이프라인 이벤트 처리 완료 수.
    PipelineEvent,
    /// 직렬화 실패.
    SerializationFail,
    /// 디코드 실패.
    DecodeFail,
    /// SHM quote slot 에 성공적으로 publish 된 이벤트 수.
    ShmQuotePublished,
    /// SHM quote publish 실패 (invalid index 등) 수.
    ShmQuoteDropped,
    /// SHM trade ring 에 성공적으로 publish 된 이벤트 수.
    ShmTradePublished,
    /// SHM trade ring lap (reader 가 뒤쳐져 slot 덮어씀) 횟수.
    ShmTradeLapDrop,
    /// SHM order ring 에 성공적으로 publish 된 이벤트 수.
    ShmOrderPublished,
    /// SHM order ring 이 full 이어서 publish 가 거부된 횟수.
    ShmOrderFullDrop,
    /// Symbol table intern 실패 (table full / too long) 수.
    ShmInternFail,
    /// order-gateway SHM subscriber 가 order ring 에서 소비한 frame 수.
    ShmOrderConsumed,
    /// 디코드/검증 실패한 OrderFrame 수 (unknown exchange, symbol resolve 실패 등).
    ShmOrderInvalid,
    /// Cancel 종류 OrderFrame 이 executor 에 dispatch 된 횟수.
    ShmOrderCancelDispatched,
    /// v2 multi-VM: gateway 가 publisher heartbeat stale (>5s) 감지한 횟수.
    ShmPublisherStale,
    /// v2 multi-VM: multi-ring fan-in batch 처리 횟수 (poll_batch >0 반환).
    ShmOrderBatch,
    /// v2 multi-VM: spin budget 소진 → park_timeout 진입 횟수.
    ShmBackoffPark,
    /// Strategy egress 에서 ZMQ fallback 경로로 publish 성공.
    /// NOTE: `ShmOrderPublished` 는 SHM 정상 경로. 둘이 함께 total egress 를 이룬다.
    OrderEgressZmqOk,
    /// Strategy egress 정책(`BackpressurePolicy::Drop`) 에 따른 즉시 drop.
    /// NOTE: `ShmOrderFullDrop` 은 ring full mechanical condition (정책 무관).
    ///       두 counter 는 서로 다른 축의 분석용이며 overlap 가능.
    OrderEgressBackpressureDropped,
    /// Strategy egress 정책(`BackpressurePolicy::RetryWithTimeout`) 내 1회 retry.
    OrderEgressBackpressureRetried,
    /// Retry 한도 초과로 최종 drop. `Retried` 카운트의 terminal tail.
    OrderEgressBackpressureRetryExhausted,
    /// Block 정책 timeout 내 성공. `ShmBackoffPark` (gateway idle park) 와 다름.
    OrderEgressBackpressureBlocked,
    /// Block 정책 timeout 초과 drop.
    OrderEgressBackpressureBlockTimeout,
    /// mode=Shm 운용 중 Zmq 로 fallback 전환된 signal.
    /// 정상상태에서는 발화 빈도가 매우 낮아야 함 (alert 트리거 후보).
    OrderEgressZmqFallbackActivated,
    /// Wire encode 실패. 이론상 발생 불가, defensive.
    OrderEgressSerializeError,
    // NOTE: `OrderGatewayExchangeNotFound` / `OrderGatewayUnknownSymbol` 은
    // `ShmOrderInvalid` 의 의도된 subset 이다. gateway 측 호출자는 총합 축으로
    // `ShmOrderInvalid` 를, 원인 축으로 subset counter 를 함께 증가시킨다.
    /// Gateway router 전달 성공. `PipelineEvent` 범용과 구분.
    OrderGatewayRoutedOk,
    /// Gateway internal tokio channel full → drop.
    /// NOTE: `ZmqDropped` 는 transport-agnostic, 이건 gateway internal queue 전용.
    OrderGatewayRouterQueueFull,
    /// Wire decode 에서 ExchangeId 매핑 실패.
    /// NOTE: `ShmOrderInvalid` 의 subset 으로 이중 집계됨 (총합 + 세부 원인 축).
    OrderGatewayExchangeNotFound,
    /// Wire decode 에서 symbol_id 매핑 실패.
    /// NOTE: `ShmOrderInvalid` 의 subset 으로 이중 집계됨.
    OrderGatewayUnknownSymbol,
    /// strategy 가 gateway 결과 wire 를 수신해 처리한 횟수.
    OrderResultReceived,
    /// strategy 가 거절(REJECTED) 결과를 수신한 횟수.
    OrderResultRejected,
    /// Gate REST account 폴러 에러 (접속/파싱/타임아웃).
    StrategyAccountPollErr,
    /// StrategyControl 채널 포화로 제어 메시지 유실.
    StrategyControlDropped,
    /// strategy hot path 의 주문 채널 포화.
    StrategyOrderChannelFull,
    /// gateway wire decode/recv 실패 (transport 종류와 무관한 총합).
    OrderGatewayInvalidWire,
    /// gateway 가 heartbeat wire 를 result channel 로 emit 한 횟수.
    GatewayHeartbeatEmitted,
    /// strategy 가 gateway heartbeat timeout 을 감지해 safe mode 진입한 횟수.
    StrategyGatewayStale,
    /// supervisor 가 서비스를 soft-restart 한 횟수.
    SupervisorRestart,
}

/// 알려진 counter 를 1 증가. hot path — atomic fetch_add 만.
#[inline]
pub fn counter_inc(k: CounterKey) {
    counter_add(k, 1);
}

/// 알려진 counter 를 `n` 만큼 증가.
#[inline]
pub fn counter_add(k: CounterKey, n: u64) {
    let c = counters();
    let target = match k {
        CounterKey::ZmqDropped => &c.zmq_dropped,
        CounterKey::ZmqHwmBlock => &c.zmq_hwm_block,
        CounterKey::PipelineEvent => &c.pipeline_event,
        CounterKey::SerializationFail => &c.serialization_fail,
        CounterKey::DecodeFail => &c.decode_fail,
        CounterKey::ShmQuotePublished => &c.shm_quote_published,
        CounterKey::ShmQuoteDropped => &c.shm_quote_dropped,
        CounterKey::ShmTradePublished => &c.shm_trade_published,
        CounterKey::ShmTradeLapDrop => &c.shm_trade_lap_drop,
        CounterKey::ShmOrderPublished => &c.shm_order_published,
        CounterKey::ShmOrderFullDrop => &c.shm_order_full_drop,
        CounterKey::ShmInternFail => &c.shm_intern_fail,
        CounterKey::ShmOrderConsumed => &c.shm_order_consumed,
        CounterKey::ShmOrderInvalid => &c.shm_order_invalid,
        CounterKey::ShmOrderCancelDispatched => &c.shm_order_cancel_dispatched,
        CounterKey::ShmPublisherStale => &c.shm_publisher_stale,
        CounterKey::ShmOrderBatch => &c.shm_order_batch,
        CounterKey::ShmBackoffPark => &c.shm_backoff_park,
        CounterKey::OrderEgressZmqOk => &c.order_egress_zmq_ok,
        CounterKey::OrderEgressBackpressureDropped => &c.order_egress_backpressure_dropped,
        CounterKey::OrderEgressBackpressureRetried => &c.order_egress_backpressure_retried,
        CounterKey::OrderEgressBackpressureRetryExhausted => {
            &c.order_egress_backpressure_retry_exhausted
        }
        CounterKey::OrderEgressBackpressureBlocked => &c.order_egress_backpressure_blocked,
        CounterKey::OrderEgressBackpressureBlockTimeout => {
            &c.order_egress_backpressure_block_timeout
        }
        CounterKey::OrderEgressZmqFallbackActivated => &c.order_egress_zmq_fallback_activated,
        CounterKey::OrderEgressSerializeError => &c.order_egress_serialize_error,
        CounterKey::OrderGatewayRoutedOk => &c.order_gateway_routed_ok,
        CounterKey::OrderGatewayRouterQueueFull => &c.order_gateway_router_queue_full,
        CounterKey::OrderGatewayExchangeNotFound => &c.order_gateway_exchange_not_found,
        CounterKey::OrderGatewayUnknownSymbol => &c.order_gateway_unknown_symbol,
        CounterKey::OrderResultReceived => &c.order_result_received,
        CounterKey::OrderResultRejected => &c.order_result_rejected,
        CounterKey::StrategyAccountPollErr => &c.strategy_account_poll_err,
        CounterKey::StrategyControlDropped => &c.strategy_control_dropped,
        CounterKey::StrategyOrderChannelFull => &c.strategy_order_channel_full,
        CounterKey::OrderGatewayInvalidWire => &c.order_gateway_invalid_wire,
        CounterKey::GatewayHeartbeatEmitted => &c.gateway_heartbeat_emitted,
        CounterKey::StrategyGatewayStale => &c.strategy_gateway_stale,
        CounterKey::SupervisorRestart => &c.supervisor_restart,
    };
    target.fetch_add(n, Ordering::Relaxed);
}

/// ad-hoc counter. static str 이므로 hot path 에서도 쓸 수 있지만
/// 첫 호출은 Mutex lock — 가능하면 `CounterKey` 등록 쪽을 권장.
pub fn extra_counter_inc(name: &'static str) {
    let c = counters();
    // 일반적으로 already-registered 경로가 매우 빠르도록 read-then-write 패턴.
    // parking_lot Mutex 는 uncontended 시 cheap.
    let mut map = c.extras.lock();
    let entry = map
        .entry(name)
        .or_insert_with(|| Arc::new(AtomicU64::new(0)))
        .clone();
    drop(map);
    entry.fetch_add(1, Ordering::Relaxed);
}

/// 모든 counter snapshot. tools/latency-probe, /metrics endpoint 등에서 사용.
pub fn counters_snapshot() -> Vec<(String, u64)> {
    let c = counters();
    let mut out = vec![
        ("zmq_dropped".into(), c.zmq_dropped.load(Ordering::Relaxed)),
        (
            "zmq_hwm_block".into(),
            c.zmq_hwm_block.load(Ordering::Relaxed),
        ),
        (
            "pipeline_event".into(),
            c.pipeline_event.load(Ordering::Relaxed),
        ),
        (
            "serialization_fail".into(),
            c.serialization_fail.load(Ordering::Relaxed),
        ),
        ("decode_fail".into(), c.decode_fail.load(Ordering::Relaxed)),
        (
            "shm_quote_published".into(),
            c.shm_quote_published.load(Ordering::Relaxed),
        ),
        (
            "shm_quote_dropped".into(),
            c.shm_quote_dropped.load(Ordering::Relaxed),
        ),
        (
            "shm_trade_published".into(),
            c.shm_trade_published.load(Ordering::Relaxed),
        ),
        (
            "shm_trade_lap_drop".into(),
            c.shm_trade_lap_drop.load(Ordering::Relaxed),
        ),
        (
            "shm_order_published".into(),
            c.shm_order_published.load(Ordering::Relaxed),
        ),
        (
            "shm_order_full_drop".into(),
            c.shm_order_full_drop.load(Ordering::Relaxed),
        ),
        (
            "shm_intern_fail".into(),
            c.shm_intern_fail.load(Ordering::Relaxed),
        ),
        (
            "shm_order_consumed".into(),
            c.shm_order_consumed.load(Ordering::Relaxed),
        ),
        (
            "shm_order_invalid".into(),
            c.shm_order_invalid.load(Ordering::Relaxed),
        ),
        (
            "shm_order_cancel_dispatched".into(),
            c.shm_order_cancel_dispatched.load(Ordering::Relaxed),
        ),
        (
            "shm_publisher_stale".into(),
            c.shm_publisher_stale.load(Ordering::Relaxed),
        ),
        (
            "shm_order_batch".into(),
            c.shm_order_batch.load(Ordering::Relaxed),
        ),
        (
            "shm_backoff_park".into(),
            c.shm_backoff_park.load(Ordering::Relaxed),
        ),
        (
            "order_egress_zmq_ok".into(),
            c.order_egress_zmq_ok.load(Ordering::Relaxed),
        ),
        (
            "order_egress_backpressure_dropped".into(),
            c.order_egress_backpressure_dropped.load(Ordering::Relaxed),
        ),
        (
            "order_egress_backpressure_retried".into(),
            c.order_egress_backpressure_retried.load(Ordering::Relaxed),
        ),
        (
            "order_egress_backpressure_retry_exhausted".into(),
            c.order_egress_backpressure_retry_exhausted
                .load(Ordering::Relaxed),
        ),
        (
            "order_egress_backpressure_blocked".into(),
            c.order_egress_backpressure_blocked.load(Ordering::Relaxed),
        ),
        (
            "order_egress_backpressure_block_timeout".into(),
            c.order_egress_backpressure_block_timeout
                .load(Ordering::Relaxed),
        ),
        (
            "order_egress_zmq_fallback_activated".into(),
            c.order_egress_zmq_fallback_activated
                .load(Ordering::Relaxed),
        ),
        (
            "order_egress_serialize_error".into(),
            c.order_egress_serialize_error.load(Ordering::Relaxed),
        ),
        (
            "order_gateway_routed_ok".into(),
            c.order_gateway_routed_ok.load(Ordering::Relaxed),
        ),
        (
            "order_gateway_router_queue_full".into(),
            c.order_gateway_router_queue_full.load(Ordering::Relaxed),
        ),
        (
            "order_gateway_exchange_not_found".into(),
            c.order_gateway_exchange_not_found.load(Ordering::Relaxed),
        ),
        (
            "order_gateway_unknown_symbol".into(),
            c.order_gateway_unknown_symbol.load(Ordering::Relaxed),
        ),
        (
            "order_result_received".into(),
            c.order_result_received.load(Ordering::Relaxed),
        ),
        (
            "order_result_rejected".into(),
            c.order_result_rejected.load(Ordering::Relaxed),
        ),
        (
            "strategy_account_poll_err".into(),
            c.strategy_account_poll_err.load(Ordering::Relaxed),
        ),
        (
            "strategy_control_dropped".into(),
            c.strategy_control_dropped.load(Ordering::Relaxed),
        ),
        (
            "strategy_order_channel_full".into(),
            c.strategy_order_channel_full.load(Ordering::Relaxed),
        ),
        (
            "order_gateway_invalid_wire".into(),
            c.order_gateway_invalid_wire.load(Ordering::Relaxed),
        ),
        (
            "gateway_heartbeat_emitted".into(),
            c.gateway_heartbeat_emitted.load(Ordering::Relaxed),
        ),
        (
            "strategy_gateway_stale".into(),
            c.strategy_gateway_stale.load(Ordering::Relaxed),
        ),
        (
            "supervisor_restart".into(),
            c.supervisor_restart.load(Ordering::Relaxed),
        ),
    ];
    for (k, v) in c.extras.lock().iter() {
        out.push(((*k).to_string(), v.load(Ordering::Relaxed)));
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// CPU affinity
// ─────────────────────────────────────────────────────────────────────────────

static NEXT_HOT_CORE: AtomicU64 = AtomicU64::new(0);

/// 다음 hot core 번호. linux CPU count 기준으로 순차 부여.
/// Non-linux 또는 core 개수 판별 실패 시 None.
pub fn next_hot_core() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        if n <= 0 {
            return None;
        }
        let n = n as u64;
        let idx = NEXT_HOT_CORE.fetch_add(1, Ordering::Relaxed) % n;
        Some(idx as usize)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 현재 스레드를 주어진 CPU 에 pin. Linux 한정, 다른 OS 는 no-op.
pub fn pin_current_thread(core: usize) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        unsafe {
            let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut cpuset);
            libc::CPU_SET(core, &mut cpuset);
            let rc = libc::sched_setaffinity(
                0,
                std::mem::size_of::<libc::cpu_set_t>(),
                &cpuset as *const _,
            );
            if rc != 0 {
                return Err(anyhow!(
                    "sched_setaffinity(core={core}) failed: errno={}",
                    *libc::__errno_location()
                ));
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = core;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// verbose-trace 매크로 — off 기본, feature on 시 tracing::trace! 로 확장.
// ─────────────────────────────────────────────────────────────────────────────

/// hot path 용 verbose tracing. feature `verbose-trace` off 이면 완전히 사라짐.
///
/// ```ignore
/// trace_verbose!(stage = ?Stage::Pushed, lat_ns = 123, "push complete");
/// ```
#[cfg(feature = "verbose-trace")]
#[macro_export]
macro_rules! trace_verbose {
    ($($arg:tt)*) => {
        ::tracing::trace!($($arg)*);
    };
}

/// no-op 변형 — feature off.
#[cfg(not(feature = "verbose-trace"))]
#[macro_export]
macro_rules! trace_verbose {
    ($($arg:tt)*) => {};
}

// ─────────────────────────────────────────────────────────────────────────────
// 테스트
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    // 테스트 간 global state 공유 → serial 실행 보장 위해 mutex.
    // cargo test 는 기본 병렬 → `--test-threads=1` 권장하지만, 여기서는 sub-lock.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn new_variants() -> [(CounterKey, &'static str, &'static str); 21] {
        [
            (
                CounterKey::OrderEgressZmqOk,
                "OrderEgressZmqOk",
                "order_egress_zmq_ok",
            ),
            (
                CounterKey::OrderEgressBackpressureDropped,
                "OrderEgressBackpressureDropped",
                "order_egress_backpressure_dropped",
            ),
            (
                CounterKey::OrderEgressBackpressureRetried,
                "OrderEgressBackpressureRetried",
                "order_egress_backpressure_retried",
            ),
            (
                CounterKey::OrderEgressBackpressureRetryExhausted,
                "OrderEgressBackpressureRetryExhausted",
                "order_egress_backpressure_retry_exhausted",
            ),
            (
                CounterKey::OrderEgressBackpressureBlocked,
                "OrderEgressBackpressureBlocked",
                "order_egress_backpressure_blocked",
            ),
            (
                CounterKey::OrderEgressBackpressureBlockTimeout,
                "OrderEgressBackpressureBlockTimeout",
                "order_egress_backpressure_block_timeout",
            ),
            (
                CounterKey::OrderEgressZmqFallbackActivated,
                "OrderEgressZmqFallbackActivated",
                "order_egress_zmq_fallback_activated",
            ),
            (
                CounterKey::OrderEgressSerializeError,
                "OrderEgressSerializeError",
                "order_egress_serialize_error",
            ),
            (
                CounterKey::OrderGatewayRoutedOk,
                "OrderGatewayRoutedOk",
                "order_gateway_routed_ok",
            ),
            (
                CounterKey::OrderGatewayRouterQueueFull,
                "OrderGatewayRouterQueueFull",
                "order_gateway_router_queue_full",
            ),
            (
                CounterKey::OrderGatewayExchangeNotFound,
                "OrderGatewayExchangeNotFound",
                "order_gateway_exchange_not_found",
            ),
            (
                CounterKey::OrderGatewayUnknownSymbol,
                "OrderGatewayUnknownSymbol",
                "order_gateway_unknown_symbol",
            ),
            (
                CounterKey::OrderResultReceived,
                "OrderResultReceived",
                "order_result_received",
            ),
            (
                CounterKey::OrderResultRejected,
                "OrderResultRejected",
                "order_result_rejected",
            ),
            (
                CounterKey::StrategyAccountPollErr,
                "StrategyAccountPollErr",
                "strategy_account_poll_err",
            ),
            (
                CounterKey::StrategyControlDropped,
                "StrategyControlDropped",
                "strategy_control_dropped",
            ),
            (
                CounterKey::StrategyOrderChannelFull,
                "StrategyOrderChannelFull",
                "strategy_order_channel_full",
            ),
            (
                CounterKey::OrderGatewayInvalidWire,
                "OrderGatewayInvalidWire",
                "order_gateway_invalid_wire",
            ),
            (
                CounterKey::GatewayHeartbeatEmitted,
                "GatewayHeartbeatEmitted",
                "gateway_heartbeat_emitted",
            ),
            (
                CounterKey::StrategyGatewayStale,
                "StrategyGatewayStale",
                "strategy_gateway_stale",
            ),
            (
                CounterKey::SupervisorRestart,
                "SupervisorRestart",
                "supervisor_restart",
            ),
        ]
    }

    fn snapshot_map() -> HashMap<String, u64> {
        counters_snapshot().into_iter().collect()
    }

    fn snapshot_value(map: &HashMap<String, u64>, key: &str) -> u64 {
        map.get(key).copied().unwrap_or(0)
    }

    fn gauge_snapshot_map() -> HashMap<String, i64> {
        gauges_snapshot().into_iter().collect()
    }

    #[test]
    fn hdr_record_and_dump() {
        let _g = TEST_LOCK.lock();
        reset_hdr_for_test();

        for v in [100, 200, 300, 400, 500, 600, 700, 800, 900, 1000] {
            record_stage_nanos(Stage::WsReceived, v);
        }

        let dump = dump_hdr();
        let (stage, snap) = dump
            .iter()
            .find(|(s, _)| *s == Stage::WsReceived)
            .unwrap()
            .clone();
        assert_eq!(stage, Stage::WsReceived);
        assert_eq!(snap.count, 10);
        assert!(snap.min <= 100);
        assert!(snap.max >= 1000);
        assert!(snap.p50 > 0);
    }

    #[test]
    fn hdr_clamps_out_of_range() {
        let _g = TEST_LOCK.lock();
        reset_hdr_for_test();

        // 0 (below low) 와 1e12 (above high) → clamp
        record_stage_nanos(Stage::Pushed, 0);
        record_stage_nanos(Stage::Pushed, 10_000_000_000_000);

        let dump = dump_hdr();
        let snap = dump
            .iter()
            .find(|(s, _)| *s == Stage::Pushed)
            .unwrap()
            .1
            .clone();
        assert_eq!(snap.count, 2);
    }

    #[test]
    fn counter_inc_increments() {
        // counter 는 전역 누적 → delta 기준 체크
        let before = counters_snapshot()
            .into_iter()
            .find(|(k, _)| k == "zmq_dropped")
            .map(|(_, v)| v)
            .unwrap();
        counter_inc(CounterKey::ZmqDropped);
        counter_add(CounterKey::ZmqDropped, 4);
        let after = counters_snapshot()
            .into_iter()
            .find(|(k, _)| k == "zmq_dropped")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(after - before, 5);
    }

    #[test]
    fn extra_counter_registers_dynamically() {
        extra_counter_inc("my_custom_counter");
        extra_counter_inc("my_custom_counter");
        let snap = counters_snapshot();
        let v = snap
            .iter()
            .find(|(k, _)| k == "my_custom_counter")
            .map(|(_, v)| *v)
            .expect("registered");
        assert!(v >= 2);
    }

    #[test]
    fn trace_verbose_compiles_both_branches() {
        // feature off 이면 macro 는 empty expansion → 부작용 없음.
        // feature on 이면 tracing::trace! 로 확장 → tracing init 안 되어 있어도 일단 컴파일 OK.
        trace_verbose!("hot path event {}", 42);
    }

    #[test]
    fn gauge_set_and_get() {
        let _g = TEST_LOCK.lock();
        gauge_set(GaugeKey::UptimeSeconds, 42);
        assert_eq!(gauge_get(GaugeKey::UptimeSeconds), 42);
    }

    #[test]
    fn gauges_snapshot_contains_values() {
        let _g = TEST_LOCK.lock();
        gauge_set(GaugeKey::UptimeSeconds, 7);
        gauge_set(GaugeKey::LastHeartbeatAgeMs, 15);
        gauge_set(GaugeKey::ActiveStrategies, 2);
        let snap = gauge_snapshot_map();
        assert_eq!(snap.get("uptime_seconds").copied(), Some(7));
        assert_eq!(snap.get("last_heartbeat_age_ms").copied(), Some(15));
        assert_eq!(snap.get("active_strategies").copied(), Some(2));
    }

    #[test]
    fn next_hot_core_returns_option() {
        // linux 아닌 환경에서는 None, linux 에서는 Some. 둘 다 OK.
        let _ = next_hot_core();
    }

    #[test]
    fn pin_current_thread_is_no_op_on_non_linux() {
        // non-linux 에서는 Ok(()) 반환. linux 에서는 실제 pin 시도 → 실패할 수도 있어 skip.
        #[cfg(not(target_os = "linux"))]
        pin_current_thread(0).unwrap();
    }

    #[test]
    fn init_twice_fails() {
        let _g = TEST_LOCK.lock();
        // init 은 실제 subscriber 등록 → 한 번 성공하면 글로벌 상태가 바뀜.
        // 이 테스트가 동작하려면 직전에 init 이 호출되지 않았어야 함.
        // 단독 실행 시 성공, 다른 테스트와 함께 돌리면 `already initialized` 도 OK.
        // 따라서 idempotent 하게 검증: 두 번째 호출은 반드시 Err.
        let cfg = TelemetryConfig::default();
        let first = init(&cfg, "test-a");
        let second = init(&cfg, "test-b");
        // 둘 중 적어도 하나는 Err ('already initialized') 이어야 함.
        assert!(first.is_err() || second.is_err(), "second init must fail");
        // cleanup: 성공한 handle 은 즉시 drop.
        drop(first);
        drop(second);
    }

    #[test]
    fn new_variants_appear_in_snapshot() {
        let _g = TEST_LOCK.lock();

        for (key, _, snake) in new_variants() {
            let before = snapshot_map();
            counter_inc(key);
            let after = snapshot_map();
            assert!(
                after.contains_key(snake),
                "snapshot missing key {snake} for variant {:?}",
                key
            );
            assert_eq!(
                snapshot_value(&after, snake) - snapshot_value(&before, snake),
                1,
                "variant {:?} should increase {snake} by 1",
                key
            );
        }
    }

    #[test]
    fn counter_add_propagates_for_new_variants() {
        let _g = TEST_LOCK.lock();

        for (key, _, snake) in new_variants() {
            let before = snapshot_map();
            counter_add(key, 42);
            let after = snapshot_map();
            assert_eq!(
                snapshot_value(&after, snake) - snapshot_value(&before, snake),
                42,
                "variant {:?} should increase {snake} by 42",
                key
            );
        }
    }

    #[test]
    fn no_snapshot_key_collision_after_extension() {
        let _g = TEST_LOCK.lock();

        let snap = counters_snapshot();
        let keys: HashSet<_> = snap.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys.len(),
            snap.len(),
            "snapshot keys must remain unique after CounterKey extension"
        );
    }

    #[test]
    fn variant_debug_to_snake_case_mapping() {
        let _g = TEST_LOCK.lock();

        for (key, debug_name, snake) in new_variants() {
            assert_eq!(format!("{key:?}"), debug_name);

            let before = snapshot_map();
            counter_inc(key);
            let after = snapshot_map();
            assert_eq!(
                snapshot_value(&after, snake) - snapshot_value(&before, snake),
                1,
                "variant {debug_name} should map to snapshot key {snake}"
            );
        }
    }

    #[test]
    fn four_point_sync_for_new_variants() {
        let _g = TEST_LOCK.lock();

        for (key, _, snake) in new_variants() {
            let before = snapshot_map();
            counter_add(key, 7);
            let after = snapshot_map();
            assert_eq!(
                snapshot_value(&after, snake) - snapshot_value(&before, snake),
                7,
                "variant {:?} must be wired through field/init/match/snapshot as {snake}",
                key
            );
        }
    }
}

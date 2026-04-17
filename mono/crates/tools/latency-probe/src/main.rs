//! latency-probe — subscriber 를 붙여 파이프라인 각 stage 의 latency 를 HDR
//! histogram 에 기록하는 진단 CLI.
//!
//! # 사용 시나리오
//! - publisher 가 돌고 있는 상태에서 이 프로세스를 기동하면 SUB 가 붙어 실시간
//!   `MarketEvent` 를 소비한다.
//! - 각 stage delta (exchange_server→ws_received, ws_received→serialized, ... ,
//!   published→subscribed, subscribed→consumed) 와 end-to-end 를 hdrhistogram 에
//!   기록.
//! - SIGINT / SIGTERM 수신 시 histograms 의 p50/p90/p99/p99.9/max 를 stderr 로 덤프.
//!
//! # Phase 2 Track B 추가 기능
//! - `--target-e2e-p999-ms`, `--target-internal-p999-ns`, `--target-stage-p999
//!   LABEL=NS` CLI 플래그로 p99.9 회귀 임계값을 선언적으로 지정할 수 있다.
//!   종료 시 임계를 초과한 histogram 이 있으면 non-zero exit code 로 빠져나가므로
//!   CI 나 nightly regression 체크에 직접 물릴 수 있다.
//! - 샘플 수가 `--min-samples` 미만인 histogram 은 검사에서 제외 (데이터 부족으로
//!   인한 flaky fail 방지).
//!
//! # 설계 메모
//! - `subscriber::start` 는 `Arc<dyn Downstream>` 을 받는다 → 여기선 `DirectFn` 로
//!   싱글 callback 에 histogram record 를 몰아넣는다.
//! - DirectFn 콜백은 subscriber 의 SubTask 가 실행하는 hot path 안에서 호출되므로
//!   가벼워야 한다. `parking_lot::Mutex` 를 stage 별로 각각 잡아 경합 최소화.
//! - HDR 레코드 단위는 나노초. 5초 단위로 rolling snapshot 을 덤프할 수도 있지만
//!   Phase 1 에선 종료 시 1회 전체 덤프로 충분.
//! - `tokio_util::time::delay_queue` 없이 `tokio::time::interval` 로 주기적
//!   incremental dump 를 선택 가능 (env `HFT_PROBE_DUMP_INTERVAL_S`).

#![deny(rust_2018_idioms)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use hdrhistogram::Histogram;
use hft_config::AppConfig;
use hft_exchange_api::CancellationToken;
use hft_time::{Clock, LatencyStamps, Stage, SystemClock};
use hft_types::MarketEvent;
use parking_lot::Mutex;
use subscriber::{start as subscriber_start, DirectFn};
use tracing::{error, info, warn};

/// 전역 allocator: mimalloc.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// 환경변수: 주기적 덤프 간격 (s). 없으면 종료 시 1회만 덤프.
const ENV_DUMP_INTERVAL_S: &str = "HFT_PROBE_DUMP_INTERVAL_S";

/// HDR histogram upper bound (nanos). 5초 = 5e9ns 는 HFT 기준 충분히 큰 upper.
const HDR_MAX_NS: u64 = 5_000_000_000;

/// HDR significant figures. 3 이면 정확도 ≈ 0.1%.
const HDR_SIGFIG: u8 = 3;

/// 임계값 체크 기본 최소 샘플 수 — 샘플이 너무 적으면 p99.9 가 의미 없다.
const DEFAULT_MIN_SAMPLES_FOR_CHECK: u64 = 1_000;

// ─────────────────────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────────────────────

/// CLI — clap derive. 모든 타겟은 "p99.9 가 이 값보다 크면 실패" 로 해석.
#[derive(Debug, Parser)]
#[command(
    name = "latency-probe",
    about = "Subscribe to publisher PUB and record per-stage HDR latency histograms.",
    long_about = None
)]
struct Cli {
    /// end-to-end (server→consumed) p99.9 허용 최대치 (밀리초). 초과 시 exit code 1.
    #[arg(long = "target-e2e-p999-ms", value_name = "MS")]
    target_e2e_p999_ms: Option<u64>,

    /// ws→consumed (internal) p99.9 허용 최대치 (나노초). 초과 시 exit code 1.
    #[arg(long = "target-internal-p999-ns", value_name = "NS")]
    target_internal_p999_ns: Option<u64>,

    /// 개별 stage pair p99.9 허용 최대치. 포맷: `LABEL=NS`, 반복 사용 가능.
    /// LABEL 은 stage_pairs() 의 라벨과 동일 ("server→ws", "ws→ser", ...).
    /// 알 수 없는 라벨은 warn 후 무시.
    #[arg(
        long = "target-stage-p999",
        value_name = "LABEL=NS",
        value_parser = parse_stage_target,
    )]
    target_stage_p999: Vec<(String, u64)>,

    /// 체크에 필요한 최소 샘플 수. 기본 1_000. 너무 적게 수집된 histogram 은 검사 skip.
    #[arg(long = "min-samples", value_name = "N", default_value_t = DEFAULT_MIN_SAMPLES_FOR_CHECK)]
    min_samples: u64,
}

/// "label=123" 형태 파싱. `=` 가 여러 개라도 첫 번째만 구분자로 본다.
fn parse_stage_target(s: &str) -> std::result::Result<(String, u64), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected `LABEL=NS` but got `{s}`"))?;
    let ns: u64 = v
        .trim()
        .parse()
        .map_err(|e| format!("NS must be u64: `{v}` ({e})"))?;
    Ok((k.trim().to_string(), ns))
}

// ─────────────────────────────────────────────────────────────────────────────
// Histograms
// ─────────────────────────────────────────────────────────────────────────────

/// 측정 대상 stage pair 목록. (from, to, label) 튜플.
fn stage_pairs() -> &'static [(Stage, Stage, &'static str)] {
    &[
        (Stage::ExchangeServer, Stage::WsReceived, "server→ws"),
        (Stage::WsReceived, Stage::Serialized, "ws→ser"),
        (Stage::Serialized, Stage::Pushed, "ser→push"),
        (Stage::Pushed, Stage::Published, "push→pub"),
        (Stage::Published, Stage::Subscribed, "pub→sub"),
        (Stage::Subscribed, Stage::Consumed, "sub→con"),
    ]
}

/// stage 별 + internal + e2e histogram 을 보관.
///
/// stage pair 순서는 `stage_pairs()` 와 대응된다. internal 과 e2e 는 별도 필드.
pub struct Histograms {
    pairs: Vec<Mutex<Histogram<u64>>>,
    internal: Mutex<Histogram<u64>>,
    e2e_ms: Mutex<Histogram<u64>>,
}

impl Histograms {
    /// 모든 histogram 을 `HDR_MAX_NS` / `HDR_SIGFIG` 로 생성.
    /// e2e 는 ms 단위라 max 를 5_000ms 로 별도 구성.
    pub fn new() -> Result<Self> {
        let mut pairs = Vec::with_capacity(stage_pairs().len());
        for _ in stage_pairs() {
            pairs.push(Mutex::new(
                Histogram::<u64>::new_with_bounds(1, HDR_MAX_NS, HDR_SIGFIG)
                    .context("hdr histogram create (pair)")?,
            ));
        }
        let internal = Mutex::new(
            Histogram::<u64>::new_with_bounds(1, HDR_MAX_NS, HDR_SIGFIG)
                .context("hdr histogram create (internal)")?,
        );
        let e2e_ms = Mutex::new(
            Histogram::<u64>::new_with_bounds(1, 5_000, HDR_SIGFIG)
                .context("hdr histogram create (e2e)")?,
        );
        Ok(Self {
            pairs,
            internal,
            e2e_ms,
        })
    }

    /// LatencyStamps 를 받아 등록된 모든 stage pair delta 를 각 histogram 에 record.
    ///
    /// zero stamp 가 있는 쌍은 skip (record_silent). hdrhistogram 의 `record`
    /// 는 upper bound 초과 시 Err 를 내므로 saturate_record 를 사용한다.
    #[inline]
    pub fn record(&self, stamps: &LatencyStamps) {
        for (i, (from, to, _)) in stage_pairs().iter().enumerate() {
            if let Some(ns) = stamps.delta_ns(*from, *to) {
                if let Some(h) = self.pairs.get(i) {
                    h.lock().saturating_record(ns);
                }
            }
        }
        if let Some(ns) = stamps.internal_ns() {
            self.internal.lock().saturating_record(ns);
        }
        if let Some(ms) = stamps.end_to_end_ms() {
            if ms >= 0 {
                self.e2e_ms.lock().saturating_record(ms as u64);
            }
        }
    }

    /// 현재 histogram 을 stderr 로 덤프. 덤프 후 reset 하지 않는다 (누적 유지).
    pub fn dump(&self, tag: &str) {
        eprintln!("─── latency-probe dump [{tag}] ───");
        for (i, (_, _, label)) in stage_pairs().iter().enumerate() {
            if let Some(h) = self.pairs.get(i) {
                let h = h.lock();
                dump_one_ns(label, &h);
            }
        }
        {
            let h = self.internal.lock();
            dump_one_ns("ws→con (internal)", &h);
        }
        {
            let h = self.e2e_ms.lock();
            dump_one_ms("server→con (e2e)", &h);
        }
        eprintln!("──────────────────────────────");
    }

    /// CLI 타겟 대비 p99.9 회귀 체크.
    ///
    /// 위반된 항목을 `Vec<Violation>` 으로 반환한다. 샘플 수가 `min_samples` 미만이면
    /// 해당 항목은 검사를 건너뛰고 warn 로그만 남긴다.
    fn check_targets(&self, cli: &Cli) -> Vec<Violation> {
        let mut v = Vec::new();
        let min = cli.min_samples;

        // stage pair targets
        for (label, target_ns) in &cli.target_stage_p999 {
            let Some(idx) = stage_pairs()
                .iter()
                .position(|(_, _, l)| *l == label.as_str())
            else {
                warn!(unknown_label = %label, "stage target ignored — unknown label");
                continue;
            };
            let h = self.pairs[idx].lock();
            if h.len() < min {
                warn!(label = %label, samples = h.len(), min, "not enough samples to check");
                continue;
            }
            let observed = h.value_at_quantile(0.999);
            if observed > *target_ns {
                v.push(Violation {
                    label: label.clone(),
                    unit: "ns",
                    observed_p999: observed,
                    target_p999: *target_ns,
                    samples: h.len(),
                });
            }
        }

        // internal target
        if let Some(t) = cli.target_internal_p999_ns {
            let h = self.internal.lock();
            if h.len() < min {
                warn!(
                    label = "internal",
                    samples = h.len(),
                    min,
                    "not enough samples to check"
                );
            } else {
                let observed = h.value_at_quantile(0.999);
                if observed > t {
                    v.push(Violation {
                        label: "ws→con (internal)".to_string(),
                        unit: "ns",
                        observed_p999: observed,
                        target_p999: t,
                        samples: h.len(),
                    });
                }
            }
        }

        // e2e target
        if let Some(t) = cli.target_e2e_p999_ms {
            let h = self.e2e_ms.lock();
            if h.len() < min {
                warn!(
                    label = "e2e",
                    samples = h.len(),
                    min,
                    "not enough samples to check"
                );
            } else {
                let observed = h.value_at_quantile(0.999);
                if observed > t {
                    v.push(Violation {
                        label: "server→con (e2e)".to_string(),
                        unit: "ms",
                        observed_p999: observed,
                        target_p999: t,
                        samples: h.len(),
                    });
                }
            }
        }
        v
    }
}

/// p99.9 임계 위반 보고 항목.
#[derive(Debug, Clone)]
pub struct Violation {
    pub label: String,
    pub unit: &'static str,
    pub observed_p999: u64,
    pub target_p999: u64,
    pub samples: u64,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{label:<22} p99.9 observed={obs}{u} target={tgt}{u} (n={n})",
            label = self.label,
            obs = self.observed_p999,
            tgt = self.target_p999,
            u = self.unit,
            n = self.samples,
        )
    }
}

fn dump_one_ns(label: &str, h: &Histogram<u64>) {
    if h.is_empty() {
        eprintln!("  {label:<22} (no samples)");
        return;
    }
    eprintln!(
        "  {label:<22} n={n:>8}  p50={p50:>7}ns  p90={p90:>7}ns  p99={p99:>8}ns  p99.9={p999:>9}ns  max={max:>9}ns",
        label = label,
        n = h.len(),
        p50 = h.value_at_quantile(0.50),
        p90 = h.value_at_quantile(0.90),
        p99 = h.value_at_quantile(0.99),
        p999 = h.value_at_quantile(0.999),
        max = h.max(),
    );
}

fn dump_one_ms(label: &str, h: &Histogram<u64>) {
    if h.is_empty() {
        eprintln!("  {label:<22} (no samples)");
        return;
    }
    eprintln!(
        "  {label:<22} n={n:>8}  p50={p50:>4}ms  p90={p90:>4}ms  p99={p99:>4}ms  p99.9={p999:>4}ms  max={max:>4}ms",
        label = label,
        n = h.len(),
        p50 = h.value_at_quantile(0.50),
        p90 = h.value_at_quantile(0.90),
        p99 = h.value_at_quantile(0.99),
        p999 = h.value_at_quantile(0.999),
        max = h.max(),
    );
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

// ─────────────────────────────────────────────────────────────────────────────
// Signal
// ─────────────────────────────────────────────────────────────────────────────

fn install_signal_handler(cancel: CancellationToken) {
    let fired = Arc::new(AtomicBool::new(false));
    let install_result = ctrlc::set_handler(move || {
        if fired.swap(true, Ordering::SeqCst) {
            eprintln!("[latency-probe] second signal — aborting");
            std::process::exit(130);
        }
        eprintln!("[latency-probe] shutdown signal received — dumping");
        cancel.cancel();
    });
    if let Err(e) = install_result {
        warn!(error = %e, "failed to install ctrlc handler");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Dump interval env
// ─────────────────────────────────────────────────────────────────────────────

fn dump_interval_from_env() -> Option<Duration> {
    let raw = std::env::var(ENV_DUMP_INTERVAL_S).ok()?;
    match raw.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(e) => {
            warn!(
                env = ENV_DUMP_INTERVAL_S,
                value = %raw,
                error = %e,
                "invalid dump interval — disabling periodic dump"
            );
            None
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// run
// ─────────────────────────────────────────────────────────────────────────────

/// `run()` 은 OK 성공 + `Vec<Violation>` (비어있을 수 있음) 을 반환.
/// 타겟 위반이 있으면 caller 에서 non-zero exit code 로 분기.
async fn run(cli: Cli) -> Result<Vec<Violation>> {
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;
    let _tele = init_telemetry(&cfg)?;

    info!(
        service = %cfg.service_name,
        sub = %cfg.zmq.sub_endpoint,
        exchanges = cfg.exchanges.len(),
        targets_stage = cli.target_stage_p999.len(),
        target_e2e_ms = ?cli.target_e2e_p999_ms,
        target_internal_ns = ?cli.target_internal_p999_ns,
        "latency-probe starting"
    );

    let histograms = Arc::new(Histograms::new()?);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());

    // DirectFn closure — subscriber 의 SubTask 가 매 이벤트마다 호출.
    let hs = histograms.clone();
    let clk = clock.clone();
    let downstream = Arc::new(DirectFn::new(
        move |_ev: MarketEvent, mut stamps: LatencyStamps| {
            // Subscriber 는 이미 Subscribed stage 를 mark 했다. 우리가 여기서
            // Consumed 까지 mark 해야 sub→con 및 internal/e2e delta 가 완성된다.
            stamps.mark(Stage::Consumed, &*clk);
            hs.record(&stamps);
        },
    ));

    let handle = subscriber_start(cfg.clone(), downstream)
        .await
        .context("subscriber::start failed")?;

    install_signal_handler(handle.cancel.clone());

    // 주기적 dump task (optional).
    let periodic = dump_interval_from_env();
    let dump_cancel = handle.cancel.child_token();
    let periodic_task = periodic.map(|interval| {
        let hs_task = histograms.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // 첫 tick 은 즉시 발동 → 1 루프 skip 해서 starting 로그 후 뜨도록.
            tick.tick().await;
            loop {
                tokio::select! {
                    _ = dump_cancel.cancelled() => break,
                    _ = tick.tick() => hs_task.dump("periodic"),
                }
            }
        })
    });

    handle.join().await;

    if let Some(t) = periodic_task {
        // cancel 이 전파되어 있음 → join 만.
        if let Err(e) = t.await {
            warn!(error = ?e, "periodic dump task join error");
        }
    }

    histograms.dump("shutdown");

    let violations = histograms.check_targets(&cli);
    if !violations.is_empty() {
        eprintln!("─── latency-probe regression targets VIOLATED ───");
        for v in &violations {
            eprintln!("  {v}");
        }
        eprintln!("─────────────────────────────────────────────────");
    } else if cli.target_e2e_p999_ms.is_some()
        || cli.target_internal_p999_ns.is_some()
        || !cli.target_stage_p999.is_empty()
    {
        info!("all latency targets satisfied");
    }

    info!("latency-probe exited cleanly");
    Ok(violations)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(v) if v.is_empty() => ExitCode::SUCCESS,
        Ok(_violations) => {
            // 타겟 위반 — 덤프는 이미 stderr 로 나갔음. CI 가 식별할 수 있도록 별도 코드.
            error!("latency-probe regression — see stderr for violations");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("[latency-probe] fatal: {e:#}");
            error!(error = %format!("{e:#}"), "latency-probe fatal");
            ExitCode::from(1)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_empty() -> Cli {
        Cli {
            target_e2e_p999_ms: None,
            target_internal_p999_ns: None,
            target_stage_p999: Vec::new(),
            min_samples: 1,
        }
    }

    #[test]
    fn parse_stage_target_ok() {
        let (k, v) = parse_stage_target("server→ws=500000").unwrap();
        assert_eq!(k, "server→ws");
        assert_eq!(v, 500_000);
    }

    #[test]
    fn parse_stage_target_rejects_no_equals() {
        assert!(parse_stage_target("server→ws").is_err());
    }

    #[test]
    fn parse_stage_target_rejects_bad_number() {
        assert!(parse_stage_target("label=abc").is_err());
    }

    #[test]
    fn no_targets_means_no_violations() {
        let h = Histograms::new().unwrap();
        // 빈 histogram 이라도 타겟이 없으면 위반 없음.
        let v = h.check_targets(&cli_empty());
        assert!(v.is_empty());
    }

    #[test]
    fn unknown_stage_label_is_ignored() {
        let h = Histograms::new().unwrap();
        let mut cli = cli_empty();
        cli.target_stage_p999.push(("no-such-label".into(), 1));
        let v = h.check_targets(&cli);
        assert!(v.is_empty());
    }

    #[test]
    fn violation_reported_when_p999_exceeds_target() {
        let h = Histograms::new().unwrap();
        // internal histogram 에 수백만 nanosecond 짜리 샘플 1_000 개 주입.
        for _ in 0..1_000 {
            h.internal.lock().saturating_record(5_000_000);
        }
        let cli = Cli {
            target_e2e_p999_ms: None,
            target_internal_p999_ns: Some(1_000), // 1μs — 5ms 관측치에 훨씬 못 미침
            target_stage_p999: Vec::new(),
            min_samples: 10,
        };
        let v = h.check_targets(&cli);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].unit, "ns");
        assert!(v[0].observed_p999 >= 4_000_000);
    }

    #[test]
    fn insufficient_samples_skips_check() {
        let h = Histograms::new().unwrap();
        h.internal.lock().saturating_record(1_000_000_000); // 거대 샘플 1 개
        let cli = Cli {
            target_e2e_p999_ms: None,
            target_internal_p999_ns: Some(1), // 무조건 실패할 타겟
            target_stage_p999: Vec::new(),
            min_samples: 10, // 10 개 미만이면 skip
        };
        let v = h.check_targets(&cli);
        assert!(v.is_empty(), "sample count below min_samples should skip");
    }
}

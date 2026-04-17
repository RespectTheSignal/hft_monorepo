//! monitoring-agent — 각 서비스 `/metrics` 직접 scrape + Telegram alert.
//!
//! Prometheus 와 독립적으로 동작해, Prometheus 자체 장애 시에도 최소 알림 경로를
//! 유지한다.

#![deny(rust_2018_idioms)]

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use hft_common::telegram::TelegramNotifier;
use hft_config::AppConfig;
use hft_exchange_api::CancellationToken;
use regex::Regex;
use tracing::{error, info, warn};

/// 전역 allocator: mimalloc.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// 단일 scrape 대상.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeTarget {
    /// 서비스 이름 (publisher / order-gateway / strategy).
    name: String,
    /// `/metrics` URL.
    url: String,
}

/// Prometheus text exposition 을 직접 가져오는 간이 scraper.
struct MetricsScraper {
    client: reqwest::Client,
    targets: Vec<ScrapeTarget>,
}

impl MetricsScraper {
    fn new(targets: Vec<ScrapeTarget>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("monitoring-agent reqwest client build")?;
        Ok(Self { client, targets })
    }

    async fn fetch(&self, target: &ScrapeTarget) -> Result<HashMap<String, f64>> {
        let body = self
            .client
            .get(&target.url)
            .send()
            .await
            .with_context(|| format!("scrape GET {}", target.url))?
            .error_for_status()
            .with_context(|| format!("scrape status {}", target.url))?
            .text()
            .await
            .with_context(|| format!("scrape body {}", target.url))?;
        Ok(parse_prometheus_text(&body))
    }
}

/// 경보 심각도.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Warning,
    Critical,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Warning => "WARNING",
            Severity::Critical => "CRITICAL",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Severity::Warning => "⚠️",
            Severity::Critical => "🚨",
        }
    }
}

/// 경보 조건.
#[derive(Debug, Clone, Copy, PartialEq)]
enum AlertCondition {
    /// gauge 값이 threshold 초과.
    GaugeAbove(f64),
    /// gauge 값이 threshold 미만.
    GaugeBelow(f64),
    /// counter 가 이전 scrape 대비 증가.
    CounterIncreased,
    /// counter rate (delta / interval_secs) 가 threshold 초과.
    CounterRateAbove(f64),
}

impl AlertCondition {
    fn describe(self) -> String {
        match self {
            AlertCondition::GaugeAbove(v) => format!("> {v}"),
            AlertCondition::GaugeBelow(v) => format!("< {v}"),
            AlertCondition::CounterIncreased => "delta > 0".into(),
            AlertCondition::CounterRateAbove(v) => format!("rate > {v}/s"),
        }
    }
}

/// 정적 alert rule 정의.
#[derive(Debug, Clone, Copy)]
struct AlertRule {
    name: &'static str,
    metric: &'static str,
    condition: AlertCondition,
    severity: Severity,
    silence_secs: u64,
}

/// 실제 발화된 alert.
#[derive(Debug, Clone)]
struct FiredAlert {
    rule_name: &'static str,
    metric: &'static str,
    severity: Severity,
    value: f64,
    condition_desc: String,
}

/// 이전 counter / silence window 상태.
#[derive(Debug, Default)]
struct AlertState {
    prev_counters: HashMap<(String, String), f64>,
    last_fired: HashMap<(String, String), Instant>,
    scrape_fail_last: HashMap<String, Instant>,
}

impl AlertState {
    fn new() -> Self {
        Self::default()
    }

    fn evaluate(
        &mut self,
        rules: &[AlertRule],
        metrics: &HashMap<String, f64>,
        target: &str,
        interval_secs: u64,
    ) -> Vec<FiredAlert> {
        self.evaluate_at(rules, metrics, target, interval_secs, Instant::now())
    }

    fn evaluate_at(
        &mut self,
        rules: &[AlertRule],
        metrics: &HashMap<String, f64>,
        target: &str,
        interval_secs: u64,
        now: Instant,
    ) -> Vec<FiredAlert> {
        let mut fired = Vec::new();

        for rule in rules {
            let Some(&value) = metrics.get(rule.metric) else {
                continue;
            };

            let should_fire = match rule.condition {
                AlertCondition::GaugeAbove(threshold) => value > threshold,
                AlertCondition::GaugeBelow(threshold) => value < threshold,
                AlertCondition::CounterIncreased => {
                    let key = (target.to_owned(), rule.metric.to_owned());
                    let prev = self.prev_counters.get(&key).copied();
                    self.prev_counters.insert(key, value);
                    matches!(prev, Some(old) if value > old)
                }
                AlertCondition::CounterRateAbove(threshold) => {
                    let key = (target.to_owned(), rule.metric.to_owned());
                    let prev = self.prev_counters.get(&key).copied();
                    self.prev_counters.insert(key, value);
                    let Some(old) = prev else {
                        continue;
                    };
                    if value <= old || interval_secs == 0 {
                        false
                    } else {
                        (value - old) / interval_secs as f64 > threshold
                    }
                }
            };

            if !should_fire || !self.should_fire_rule(target, rule, now) {
                continue;
            }

            fired.push(FiredAlert {
                rule_name: rule.name,
                metric: rule.metric,
                severity: rule.severity,
                value,
                condition_desc: rule.condition.describe(),
            });
        }

        fired
    }

    fn should_fire_scrape_fail(&mut self, target: &str) -> bool {
        self.should_fire_scrape_fail_at(target, Instant::now())
    }

    fn should_fire_scrape_fail_at(&mut self, target: &str, now: Instant) -> bool {
        const SCRAPE_FAIL_SILENCE: Duration = Duration::from_secs(60);
        match self.scrape_fail_last.get(target) {
            Some(last) if now.duration_since(*last) < SCRAPE_FAIL_SILENCE => false,
            _ => {
                self.scrape_fail_last.insert(target.to_owned(), now);
                true
            }
        }
    }

    fn should_fire_rule(&mut self, target: &str, rule: &AlertRule, now: Instant) -> bool {
        let key = (target.to_owned(), rule.name.to_owned());
        if let Some(last) = self.last_fired.get(&key) {
            if now.duration_since(*last) < Duration::from_secs(rule.silence_secs) {
                return false;
            }
        }
        self.last_fired.insert(key, now);
        true
    }
}

fn default_alert_rules() -> Vec<AlertRule> {
    vec![
        AlertRule {
            name: "heartbeat_stale",
            metric: "hft_last_heartbeat_age_ms",
            condition: AlertCondition::GaugeAbove(5_000.0),
            severity: Severity::Critical,
            silence_secs: 60,
        },
        AlertRule {
            name: "service_down",
            metric: "hft_uptime_seconds",
            condition: AlertCondition::GaugeBelow(1.0),
            severity: Severity::Critical,
            silence_secs: 60,
        },
        AlertRule {
            name: "no_strategies",
            metric: "hft_active_strategies",
            condition: AlertCondition::GaugeBelow(1.0),
            severity: Severity::Warning,
            silence_secs: 300,
        },
        AlertRule {
            name: "e2e_latency_high",
            metric: "hft_stage_e2e_p99_ns",
            condition: AlertCondition::GaugeAbove(2_000_000.0),
            severity: Severity::Warning,
            silence_secs: 120,
        },
        AlertRule {
            name: "e2e_latency_critical",
            metric: "hft_stage_e2e_p999_ns",
            condition: AlertCondition::GaugeAbove(5_000_000.0),
            severity: Severity::Critical,
            silence_secs: 60,
        },
        AlertRule {
            name: "supervisor_restart",
            metric: "hft_supervisor_restart_total",
            condition: AlertCondition::CounterIncreased,
            severity: Severity::Critical,
            silence_secs: 60,
        },
        AlertRule {
            name: "gateway_stale",
            metric: "hft_strategy_gateway_stale_total",
            condition: AlertCondition::CounterIncreased,
            severity: Severity::Critical,
            silence_secs: 60,
        },
        AlertRule {
            name: "publisher_stale",
            metric: "hft_shm_publisher_stale_total",
            condition: AlertCondition::CounterIncreased,
            severity: Severity::Critical,
            silence_secs: 60,
        },
        AlertRule {
            name: "quote_dropped",
            metric: "hft_shm_quote_dropped_total",
            condition: AlertCondition::CounterRateAbove(10.0),
            severity: Severity::Warning,
            silence_secs: 120,
        },
        AlertRule {
            name: "order_ring_full",
            metric: "hft_shm_order_full_drop_total",
            condition: AlertCondition::CounterIncreased,
            severity: Severity::Critical,
            silence_secs: 60,
        },
        AlertRule {
            name: "order_dropped",
            metric: "hft_order_egress_backpressure_dropped_total",
            condition: AlertCondition::CounterIncreased,
            severity: Severity::Critical,
            silence_secs: 60,
        },
        AlertRule {
            name: "order_retry_exhausted",
            metric: "hft_order_egress_backpressure_retry_exhausted_total",
            condition: AlertCondition::CounterIncreased,
            severity: Severity::Critical,
            silence_secs: 60,
        },
        AlertRule {
            name: "order_rejected",
            metric: "hft_order_result_rejected_total",
            condition: AlertCondition::CounterIncreased,
            severity: Severity::Warning,
            silence_secs: 120,
        },
        AlertRule {
            name: "zmq_fallback",
            metric: "hft_order_egress_zmq_fallback_activated_total",
            condition: AlertCondition::CounterIncreased,
            severity: Severity::Critical,
            silence_secs: 60,
        },
    ]
}

/// Prometheus text exposition 을 간이 파싱한다.
///
/// label 은 무시하고 metric 이름 + 값만 추출한다.
fn parse_prometheus_text(body: &str) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(caps) = metrics_line_regex().captures(line) else {
            continue;
        };
        let (Some(name), Some(value)) = (caps.get(1), caps.get(2)) else {
            continue;
        };
        let Ok(value) = value.as_str().parse::<f64>() else {
            continue;
        };
        out.insert(name.as_str().to_owned(), value);
    }
    out
}

fn metrics_line_regex() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{[^}]*\})?\s+([-+]?(?:\d+(?:\.\d+)?|\.\d+)(?:[eE][-+]?\d+)?)$"#,
        )
        .expect("prometheus regex")
    })
}

fn build_targets(cfg: &AppConfig) -> Vec<ScrapeTarget> {
    vec![
        ScrapeTarget {
            name: "publisher".into(),
            url: format!("http://{}/metrics", cfg.monitoring.publisher_addr),
        },
        ScrapeTarget {
            name: "order-gateway".into(),
            url: format!("http://{}/metrics", cfg.monitoring.gateway_addr),
        },
        ScrapeTarget {
            name: "strategy".into(),
            url: format!("http://{}/metrics", cfg.monitoring.strategy_addr),
        },
    ]
}

fn format_alert(alert: &FiredAlert, target: &str) -> String {
    format!(
        "{} <b>[{}] {}</b>\nService: <code>{}</code>\nMetric: <code>{}</code> = <code>{:.3}</code>\nCondition: <code>{}</code>",
        alert.severity.icon(),
        alert.severity.label(),
        alert.rule_name,
        target,
        alert.metric,
        alert.value,
        alert.condition_desc,
    )
}

fn format_scrape_fail(target: &str, err: &anyhow::Error) -> String {
    format!(
        "🚨 <b>[CRITICAL] scrape_failed</b>\nService: <code>{}</code>\nError: <code>{}</code>",
        target, err
    )
}

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
            eprintln!("[monitoring-agent] second signal received — aborting");
            std::process::exit(130);
        }
        eprintln!("[monitoring-agent] shutdown signal received — draining");
        cancel.cancel();
    });
    if let Err(e) = install_result {
        warn!(error = %e, "failed to install ctrlc handler (already installed?)");
    }
}

async fn run() -> Result<()> {
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;
    let _tele = init_telemetry(&cfg)?;

    let cancel = CancellationToken::new();
    install_signal_handler(cancel.clone());

    let telegram = TelegramNotifier::from_env();
    if telegram.is_none() {
        warn!("TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID not set — alerts will be logged only");
    }

    let targets = build_targets(&cfg);
    let scraper = MetricsScraper::new(targets)?;
    let rules = default_alert_rules();
    let mut state = AlertState::new();
    let mut interval =
        tokio::time::interval(Duration::from_secs(cfg.monitoring.scrape_interval_secs));

    info!(
        scrape_interval_secs = cfg.monitoring.scrape_interval_secs,
        publisher = %cfg.monitoring.publisher_addr,
        gateway = %cfg.monitoring.gateway_addr,
        strategy = %cfg.monitoring.strategy_addr,
        "monitoring-agent starting"
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {
                for target in &scraper.targets {
                    match scraper.fetch(target).await {
                        Ok(metrics) => {
                            let alerts = state.evaluate(
                                &rules,
                                &metrics,
                                &target.name,
                                cfg.monitoring.scrape_interval_secs,
                            );
                            for alert in alerts {
                                let msg = format_alert(&alert, &target.name);
                                error!(
                                    target: "alert",
                                    service = %target.name,
                                    rule = alert.rule_name,
                                    "{msg}"
                                );
                                if let Some(tg) = &telegram {
                                    if let Err(e) = tg.send(&msg).await {
                                        warn!(error = %e, service = %target.name, "telegram send failed");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let msg = format_scrape_fail(&target.name, &e);
                            warn!(service = %target.name, error = %e, "metrics scrape failed");
                            if let Some(tg) = &telegram {
                                if state.should_fire_scrape_fail(&target.name) {
                                    if let Err(send_err) = tg.send(&msg).await {
                                        warn!(
                                            error = %send_err,
                                            service = %target.name,
                                            "telegram send failed"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    info!("monitoring-agent exited cleanly");
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[monitoring-agent] fatal: {e:#}");
            error!(error = %format!("{e:#}"), "monitoring-agent fatal error");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let parsed = parse_prometheus_text("hft_uptime_seconds 42\n");
        assert_eq!(parsed.get("hft_uptime_seconds"), Some(&42.0));
    }

    #[test]
    fn parse_skips_comments() {
        let parsed =
            parse_prometheus_text("# HELP foo x\n# TYPE foo counter\nhft_uptime_seconds 42\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.get("hft_uptime_seconds"), Some(&42.0));
    }

    #[test]
    fn parse_with_labels() {
        let parsed = parse_prometheus_text("metric{label=\"val\"} 123\n");
        assert_eq!(parsed.get("metric"), Some(&123.0));
    }

    #[test]
    fn gauge_above_fires() {
        let rules = [AlertRule {
            name: "heartbeat_stale",
            metric: "hft_last_heartbeat_age_ms",
            condition: AlertCondition::GaugeAbove(5_000.0),
            severity: Severity::Critical,
            silence_secs: 60,
        }];
        let mut metrics = HashMap::new();
        metrics.insert("hft_last_heartbeat_age_ms".into(), 6_000.0);
        let mut state = AlertState::new();
        let fired = state.evaluate_at(&rules, &metrics, "strategy", 5, Instant::now());
        assert_eq!(fired.len(), 1);
    }

    #[test]
    fn gauge_above_within_silence_does_not_refire() {
        let rules = [AlertRule {
            name: "heartbeat_stale",
            metric: "hft_last_heartbeat_age_ms",
            condition: AlertCondition::GaugeAbove(5_000.0),
            severity: Severity::Critical,
            silence_secs: 60,
        }];
        let mut metrics = HashMap::new();
        metrics.insert("hft_last_heartbeat_age_ms".into(), 6_000.0);
        let mut state = AlertState::new();
        let now = Instant::now();
        assert_eq!(
            state
                .evaluate_at(&rules, &metrics, "strategy", 5, now)
                .len(),
            1
        );
        assert_eq!(
            state
                .evaluate_at(
                    &rules,
                    &metrics,
                    "strategy",
                    5,
                    now + Duration::from_secs(10)
                )
                .len(),
            0
        );
    }

    #[test]
    fn gauge_below_fires() {
        let rules = [AlertRule {
            name: "service_down",
            metric: "hft_uptime_seconds",
            condition: AlertCondition::GaugeBelow(1.0),
            severity: Severity::Critical,
            silence_secs: 60,
        }];
        let mut metrics = HashMap::new();
        metrics.insert("hft_uptime_seconds".into(), 0.0);
        let mut state = AlertState::new();
        assert_eq!(
            state
                .evaluate_at(&rules, &metrics, "publisher", 5, Instant::now())
                .len(),
            1
        );
    }

    #[test]
    fn counter_increased_fires() {
        let rules = [AlertRule {
            name: "restart",
            metric: "hft_supervisor_restart_total",
            condition: AlertCondition::CounterIncreased,
            severity: Severity::Critical,
            silence_secs: 60,
        }];
        let mut state = AlertState::new();
        let mut metrics = HashMap::new();
        metrics.insert("hft_supervisor_restart_total".into(), 1.0);
        assert_eq!(
            state
                .evaluate_at(&rules, &metrics, "publisher", 5, Instant::now())
                .len(),
            0
        );
        metrics.insert("hft_supervisor_restart_total".into(), 2.0);
        assert_eq!(
            state
                .evaluate_at(&rules, &metrics, "publisher", 5, Instant::now())
                .len(),
            1
        );
    }

    #[test]
    fn counter_no_change_no_fire() {
        let rules = [AlertRule {
            name: "restart",
            metric: "hft_supervisor_restart_total",
            condition: AlertCondition::CounterIncreased,
            severity: Severity::Critical,
            silence_secs: 60,
        }];
        let mut state = AlertState::new();
        let mut metrics = HashMap::new();
        metrics.insert("hft_supervisor_restart_total".into(), 1.0);
        let now = Instant::now();
        state.evaluate_at(&rules, &metrics, "publisher", 5, now);
        assert_eq!(
            state
                .evaluate_at(
                    &rules,
                    &metrics,
                    "publisher",
                    5,
                    now + Duration::from_secs(5)
                )
                .len(),
            0
        );
    }

    #[test]
    fn counter_rate_above_fires() {
        let rules = [AlertRule {
            name: "quote_dropped",
            metric: "hft_shm_quote_dropped_total",
            condition: AlertCondition::CounterRateAbove(10.0),
            severity: Severity::Warning,
            silence_secs: 120,
        }];
        let mut state = AlertState::new();
        let mut metrics = HashMap::new();
        metrics.insert("hft_shm_quote_dropped_total".into(), 10.0);
        let now = Instant::now();
        state.evaluate_at(&rules, &metrics, "publisher", 5, now);
        metrics.insert("hft_shm_quote_dropped_total".into(), 70.0);
        assert_eq!(
            state
                .evaluate_at(
                    &rules,
                    &metrics,
                    "publisher",
                    5,
                    now + Duration::from_secs(5)
                )
                .len(),
            1
        );
    }

    #[test]
    fn silence_expires_allows_refire() {
        let rules = [AlertRule {
            name: "heartbeat_stale",
            metric: "hft_last_heartbeat_age_ms",
            condition: AlertCondition::GaugeAbove(5_000.0),
            severity: Severity::Critical,
            silence_secs: 60,
        }];
        let mut metrics = HashMap::new();
        metrics.insert("hft_last_heartbeat_age_ms".into(), 6_000.0);
        let mut state = AlertState::new();
        let now = Instant::now();
        assert_eq!(
            state
                .evaluate_at(&rules, &metrics, "strategy", 5, now)
                .len(),
            1
        );
        assert_eq!(
            state
                .evaluate_at(
                    &rules,
                    &metrics,
                    "strategy",
                    5,
                    now + Duration::from_secs(61)
                )
                .len(),
            1
        );
    }

    #[test]
    fn format_alert_contains_html_markup() {
        let alert = FiredAlert {
            rule_name: "heartbeat_stale",
            metric: "hft_last_heartbeat_age_ms",
            severity: Severity::Critical,
            value: 7_234.0,
            condition_desc: "> 5000".into(),
        };
        let msg = format_alert(&alert, "strategy");
        assert!(msg.contains("<b>[CRITICAL] heartbeat_stale</b>"));
        assert!(msg.contains("Service: <code>strategy</code>"));
    }
}

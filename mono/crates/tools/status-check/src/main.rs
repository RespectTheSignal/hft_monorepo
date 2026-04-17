//! status-check — 파이프라인 상태를 한눈에 보는 운영 CLI.

#![deny(rust_2018_idioms)]

use std::collections::{BTreeMap, HashMap};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use hft_exchange_gate::{AccountBalance, GateAccountClient, GateOpenPosition};
use hft_exchange_rest::{Credentials, RestClient};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::error;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(name = "status-check", about = "HFT 파이프라인 전체 상태 조회")]
struct Cli {
    /// publisher base address.
    #[arg(
        long,
        env = "HFT_PUBLISHER_ADDR",
        default_value = "http://127.0.0.1:9100"
    )]
    publisher_addr: String,
    /// order-gateway base address.
    #[arg(
        long,
        env = "HFT_GATEWAY_ADDR",
        default_value = "http://127.0.0.1:9101"
    )]
    gateway_addr: String,
    /// strategy base address.
    #[arg(
        long,
        env = "HFT_STRATEGY_ADDR",
        default_value = "http://127.0.0.1:9102"
    )]
    strategy_addr: String,
    /// JSON 출력.
    #[arg(long, default_value_t = false)]
    json: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct HealthResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    service: String,
    #[serde(default)]
    uptime_s: i64,
    #[serde(default)]
    counters: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize)]
struct ServiceStatus {
    name: String,
    up: bool,
    status: String,
    uptime_s: Option<i64>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct AccountSection {
    balance: AccountBalanceJson,
    positions: Vec<GateOpenPositionJson>,
}

#[derive(Debug, Clone, Serialize)]
struct AccountBalanceJson {
    total_usdt: f64,
    unrealized_pnl_usdt: f64,
    available_usdt: f64,
}

#[derive(Debug, Clone, Serialize)]
struct GateOpenPositionJson {
    contract: String,
    size: i64,
    entry_price: f64,
    mark_price: f64,
    unrealised_pnl: f64,
}

#[derive(Debug, Clone, Serialize)]
struct MetricsSummary {
    latency: BTreeMap<String, f64>,
    alerts: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Serialize)]
struct StatusReport {
    services: Vec<ServiceStatus>,
    account: Option<AccountSection>,
    metrics: MetricsSummary,
}

fn init_telemetry() -> Result<hft_telemetry::TelemetryHandle> {
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;
    let tcfg = hft_telemetry::TelemetryConfig {
        otlp_endpoint: cfg.telemetry.otlp_endpoint.clone(),
        default_level: cfg.telemetry.default_level.clone(),
        json_logs: cfg.telemetry.stdout_json,
    };
    hft_telemetry::init(&tcfg, "status-check").context("telemetry init failed")
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("status-check reqwest client build")
}

fn gate_credentials() -> Option<Credentials> {
    let key = std::env::var("GATE_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("HFT_GATE_API_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })?;
    let secret = std::env::var("GATE_API_SECRET")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("HFT_GATE_API_SECRET")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })?;
    Some(Credentials::new(key, secret))
}

fn gate_rest_client(timeout: Duration) -> Result<RestClient> {
    let inner = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(2))
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_nodelay(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("gate reqwest client build")?;
    Ok(RestClient::with_client(inner))
}

async fn fetch_health(client: &reqwest::Client, base: &str, name: &str) -> ServiceStatus {
    let url = format!("{}/health", base.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<HealthResponse>().await {
            Ok(body) => ServiceStatus {
                name: name.to_owned(),
                up: true,
                status: body.status,
                uptime_s: Some(body.uptime_s),
                error: None,
            },
            Err(e) => ServiceStatus {
                name: name.to_owned(),
                up: false,
                status: "decode_error".into(),
                uptime_s: None,
                error: Some(e.to_string()),
            },
        },
        Ok(resp) => ServiceStatus {
            name: name.to_owned(),
            up: false,
            status: "http_error".into(),
            uptime_s: None,
            error: Some(format!("HTTP {}", resp.status())),
        },
        Err(e) => ServiceStatus {
            name: name.to_owned(),
            up: false,
            status: "down".into(),
            uptime_s: None,
            error: Some(e.to_string()),
        },
    }
}

async fn fetch_metrics(client: &reqwest::Client, base: &str) -> Result<HashMap<String, f64>> {
    let url = format!("{}/metrics", base.trim_end_matches('/'));
    let body = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP status {url}"))?
        .text()
        .await
        .with_context(|| format!("read body {url}"))?;
    Ok(parse_prometheus_text(&body))
}

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

fn format_uptime(seconds: i64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    format!("{hours}h {minutes}m")
}

fn format_position_line(pos: &GateOpenPositionJson) -> String {
    format!(
        "  {:<13}: {:+} contracts  (entry: {:.4}, mark: {:.4}, pnl: {:+.4})",
        pos.contract, pos.size, pos.entry_price, pos.mark_price, pos.unrealised_pnl
    )
}

fn account_balance_json(balance: &AccountBalance) -> AccountBalanceJson {
    AccountBalanceJson {
        total_usdt: balance.total_usdt,
        unrealized_pnl_usdt: balance.unrealized_pnl_usdt,
        available_usdt: balance.available_usdt,
    }
}

fn open_position_json(pos: &GateOpenPosition) -> GateOpenPositionJson {
    GateOpenPositionJson {
        contract: pos.contract.clone(),
        size: pos.size,
        entry_price: pos.entry_price,
        mark_price: pos.mark_price,
        unrealised_pnl: pos.unrealised_pnl,
    }
}

fn collect_latency(metrics: &HashMap<String, f64>) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for (label, metric) in [
        ("e2e", "hft_stage_e2e_p99_ns"),
        ("ws_received", "hft_stage_ws_received_p99_ns"),
        ("serialized", "hft_stage_serialized_p99_ns"),
        ("pushed", "hft_stage_pushed_p99_ns"),
        ("published", "hft_stage_published_p99_ns"),
        ("subscribed", "hft_stage_subscribed_p99_ns"),
        ("consumed", "hft_stage_consumed_p99_ns"),
    ] {
        if let Some(value) = metrics.get(metric) {
            out.insert(label.to_string(), *value);
        }
    }
    out
}

fn collect_alerts(
    publisher: &HashMap<String, f64>,
    gateway: &HashMap<String, f64>,
    strategy: &HashMap<String, f64>,
) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    out.insert(
        "supervisor_restart".into(),
        publisher
            .get("hft_supervisor_restart_total")
            .copied()
            .unwrap_or(0.0)
            + gateway
                .get("hft_supervisor_restart_total")
                .copied()
                .unwrap_or(0.0)
            + strategy
                .get("hft_supervisor_restart_total")
                .copied()
                .unwrap_or(0.0),
    );
    out.insert(
        "strategy_gateway_stale".into(),
        strategy
            .get("hft_strategy_gateway_stale_total")
            .copied()
            .unwrap_or(0.0),
    );
    out.insert(
        "shm_quote_dropped".into(),
        publisher
            .get("hft_shm_quote_dropped_total")
            .copied()
            .unwrap_or(0.0),
    );
    out.insert(
        "order_egress_backpressure".into(),
        strategy
            .get("hft_order_egress_backpressure_dropped_total")
            .copied()
            .unwrap_or(0.0)
            + strategy
                .get("hft_order_egress_backpressure_retry_exhausted_total")
                .copied()
                .unwrap_or(0.0),
    );
    out
}

fn print_table(report: &StatusReport) {
    println!("=== HFT Pipeline Status ===\n");

    println!("[Services]");
    for svc in &report.services {
        if svc.up {
            println!(
                "  {:<13}: UP   (uptime: {})",
                svc.name,
                format_uptime(svc.uptime_s.unwrap_or_default())
            );
        } else {
            println!(
                "  {:<13}: DOWN ({})",
                svc.name,
                svc.error.as_deref().unwrap_or("unknown")
            );
        }
    }

    println!("\n[Account]");
    if let Some(account) = &report.account {
        println!("  Balance       : {:.2} USDT", account.balance.total_usdt);
        println!(
            "  Unrealized PnL: {:+.2} USDT",
            account.balance.unrealized_pnl_usdt
        );
    } else {
        println!("  Gate credentials missing — account section unavailable");
    }

    println!("\n[Positions]");
    if let Some(account) = &report.account {
        if account.positions.is_empty() {
            println!("  (none)");
        } else {
            for pos in &account.positions {
                println!("{}", format_position_line(pos));
            }
        }
    } else {
        println!("  (unavailable)");
    }

    println!("\n[Latency (p99)]");
    for (stage, ns) in &report.metrics.latency {
        println!("  {:<13}: {:.0} ns", stage, ns);
    }

    println!("\n[Alerts]");
    for (name, value) in &report.metrics.alerts {
        println!("  {:<28}: {:.0}", name, value);
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let _tele = init_telemetry()?;
    let client = http_client()?;

    let (pub_health, gw_health, strat_health) = tokio::join!(
        fetch_health(&client, &cli.publisher_addr, "publisher"),
        fetch_health(&client, &cli.gateway_addr, "order-gateway"),
        fetch_health(&client, &cli.strategy_addr, "strategy"),
    );

    let (pub_metrics, gw_metrics, strat_metrics) = tokio::join!(
        fetch_metrics(&client, &cli.publisher_addr),
        fetch_metrics(&client, &cli.gateway_addr),
        fetch_metrics(&client, &cli.strategy_addr),
    );

    let account = if let Some(creds) = gate_credentials() {
        let http = gate_rest_client(Duration::from_secs(5))?;
        let account = GateAccountClient::new(creds, http);
        let (balance, positions) =
            tokio::join!(account.fetch_accounts(), account.fetch_open_positions());
        let balance = balance.context("fetch_accounts failed")?;
        let positions = positions.context("fetch_open_positions failed")?;
        Some(AccountSection {
            balance: account_balance_json(&balance),
            positions: positions.iter().map(open_position_json).collect(),
        })
    } else {
        None
    };

    let pub_metrics = pub_metrics.unwrap_or_default();
    let gw_metrics = gw_metrics.unwrap_or_default();
    let strat_metrics = strat_metrics.unwrap_or_default();

    let report = StatusReport {
        services: vec![pub_health, gw_health, strat_health],
        account,
        metrics: MetricsSummary {
            latency: collect_latency(&strat_metrics),
            alerts: collect_alerts(&pub_metrics, &gw_metrics, &strat_metrics),
        },
    };

    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialize json report")?
        );
    } else {
        print_table(&report);
    }

    let all_up = report.services.iter().all(|s| s.up);
    Ok(if all_up {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("[status-check] fatal: {e:#}");
            error!(error = %format!("{e:#}"), "status-check fatal error");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_prometheus_text() {
        let parsed = parse_prometheus_text(
            "# HELP x\n# TYPE x gauge\nhft_uptime_seconds 42\nmetric{label=\"a\"} 1.5\n",
        );
        assert_eq!(parsed.get("hft_uptime_seconds"), Some(&42.0));
        assert_eq!(parsed.get("metric"), Some(&1.5));
    }

    #[test]
    fn test_format_uptime() {
        assert_eq!(format_uptime(12_240), "3h 24m");
    }

    #[test]
    fn test_format_position_line() {
        let line = format_position_line(&GateOpenPositionJson {
            contract: "BTC_USDT".into(),
            size: 3,
            entry_price: 65_234.5,
            mark_price: 65_300.0,
            unrealised_pnl: 19.65,
        });
        assert!(line.contains("BTC_USDT"));
        assert!(line.contains("+3"));
        assert!(line.contains("65234.5000"));
    }
}

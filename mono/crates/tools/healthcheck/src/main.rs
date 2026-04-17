//! healthcheck — 서비스 `/health` probe + 선택적 Gate 테스트 주문.

#![deny(rust_2018_idioms)]

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use hft_exchange_api::{ExchangeExecutor, OrderRequest, OrderSide, OrderType, TimeInForce};
use hft_exchange_gate::{GateAccountClient, GateExecutor};
use hft_exchange_rest::{Credentials, RestClient};
use hft_types::{ExchangeId, Symbol};
use serde::Deserialize;
use tracing::error;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(
    name = "healthcheck",
    about = "HFT 파이프라인 서비스 health 확인 + 선택적 테스트 주문"
)]
struct Cli {
    /// publisher /health 주소.
    #[arg(
        long,
        env = "HFT_PUBLISHER_ADDR",
        default_value = "http://127.0.0.1:9100"
    )]
    publisher_addr: String,
    /// order-gateway /health 주소.
    #[arg(
        long,
        env = "HFT_GATEWAY_ADDR",
        default_value = "http://127.0.0.1:9101"
    )]
    gateway_addr: String,
    /// strategy /health 주소.
    #[arg(
        long,
        env = "HFT_STRATEGY_ADDR",
        default_value = "http://127.0.0.1:9102"
    )]
    strategy_addr: String,
    /// 테스트 주문 실행 여부.
    #[arg(long, default_value_t = false)]
    test_order: bool,
    /// 테스트 주문 심볼.
    #[arg(long, default_value = "BTC_USDT")]
    test_symbol: String,
    /// 테스트 주문 수량.
    #[arg(long, default_value_t = 1)]
    test_size: i64,
    /// 테스트 주문 가격. 0 이면 conservative low-limit 가격 자동 계산.
    #[arg(long, default_value_t = 0.0)]
    test_price: f64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
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

fn init_telemetry() -> Result<hft_telemetry::TelemetryHandle> {
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;
    let tcfg = hft_telemetry::TelemetryConfig {
        otlp_endpoint: cfg.telemetry.otlp_endpoint.clone(),
        default_level: cfg.telemetry.default_level.clone(),
        json_logs: cfg.telemetry.stdout_json,
    };
    hft_telemetry::init(&tcfg, "healthcheck").context("telemetry init failed")
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("healthcheck reqwest client build")
}

fn gate_credentials() -> Result<Credentials> {
    let key = std::env::var("GATE_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("HFT_GATE_API_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .context("GATE_API_KEY or HFT_GATE_API_KEY missing")?;
    let secret = std::env::var("GATE_API_SECRET")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("HFT_GATE_API_SECRET")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .context("GATE_API_SECRET or HFT_GATE_API_SECRET missing")?;
    Ok(Credentials::new(key, secret))
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

async fn fetch_health(client: &reqwest::Client, base: &str) -> Result<HealthResponse> {
    let url = format!("{}/health", base.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("{url} returned HTTP {}", resp.status());
    }
    resp.json::<HealthResponse>()
        .await
        .with_context(|| format!("decode health json from {url}"))
}

fn stale_counter_warnings(body: &HealthResponse) -> Vec<(String, u64)> {
    body.counters
        .iter()
        .filter_map(|(k, &v)| {
            if v > 0 && (k.contains("stale") || k.contains("restart")) {
                Some((k.clone(), v))
            } else {
                None
            }
        })
        .collect()
}

async fn compute_test_price(account: &GateAccountClient, symbol: &str) -> Result<f64> {
    let contract = account
        .fetch_contract(symbol)
        .await
        .with_context(|| format!("fetch_contract {symbol}"))?;
    let tick = contract.order_price_round.max(0.0001);
    Ok((tick * 10.0).max(1.0))
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let _tele = init_telemetry()?;
    let client = http_client()?;
    let mut all_ok = true;

    let targets = [
        ("publisher", cli.publisher_addr.as_str()),
        ("order-gateway", cli.gateway_addr.as_str()),
        ("strategy", cli.strategy_addr.as_str()),
    ];

    for (name, addr) in targets {
        match fetch_health(&client, addr).await {
            Ok(body) => {
                println!(
                    "[OK] {name}: status={}, uptime={}s",
                    body.status, body.uptime_s
                );
                for (k, v) in stale_counter_warnings(&body) {
                    println!("  [WARN] {k} = {v}");
                }
            }
            Err(e) => {
                println!("[FAIL] {name}: {e:#}");
                all_ok = false;
            }
        }
    }

    if cli.test_order {
        println!("\n--- Test Order ---");
        let creds = gate_credentials()?;
        let http = gate_rest_client(Duration::from_secs(10))?;
        let executor = GateExecutor::new(creds.clone())?.with_http(http.clone());
        let account = GateAccountClient::new(creds, http);

        let price = if cli.test_price > 0.0 {
            cli.test_price
        } else {
            let auto = compute_test_price(&account, &cli.test_symbol).await?;
            println!("  test_price=0 → 자동 계산: conservative limit price {auto}");
            auto
        };

        let order_req = OrderRequest {
            exchange: ExchangeId::Gate,
            symbol: Symbol::new(&cli.test_symbol),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            qty: cli.test_size as f64,
            price: Some(price),
            reduce_only: false,
            tif: TimeInForce::Gtc,
            client_seq: 0,
            origin_ts_ns: 0,
            client_id: Arc::<str>::from("t-healthcheck"),
        };

        match executor.place_order(order_req).await {
            Ok(ack) => {
                println!("[OK] test order placed: id={}", ack.exchange_order_id);
                match executor.cancel(&ack.exchange_order_id).await {
                    Ok(()) => println!("[OK] test order cancelled"),
                    Err(e) => {
                        println!("[WARN] cancel failed: {e:#} (수동 확인 필요)");
                        all_ok = false;
                    }
                }
            }
            Err(e) => {
                println!("[FAIL] test order failed: {e:#}");
                all_ok = false;
            }
        }
    }

    Ok(if all_ok {
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
            eprintln!("[healthcheck] fatal: {e:#}");
            error!(error = %format!("{e:#}"), "healthcheck fatal error");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_health_response() {
        let body = r#"{
            "status":"ok",
            "service":"hft-strategy",
            "uptime_s":3600,
            "counters":{"supervisor_restart":0,"strategy_gateway_stale":2}
        }"#;
        let parsed: HealthResponse = serde_json::from_str(body).expect("health response");
        assert_eq!(parsed.status, "ok");
        assert_eq!(parsed.service, "hft-strategy");
        assert_eq!(parsed.uptime_s, 3600);
        assert_eq!(parsed.counters.get("strategy_gateway_stale"), Some(&2));
    }

    #[test]
    fn test_stale_counter_warning() {
        let mut counters = BTreeMap::new();
        counters.insert("supervisor_restart".into(), 1);
        counters.insert("strategy_gateway_stale".into(), 3);
        counters.insert("gateway_heartbeat_emitted".into(), 100);
        let warnings = stale_counter_warnings(&HealthResponse {
            status: "ok".into(),
            service: "strategy".into(),
            uptime_s: 1,
            counters,
        });
        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().any(|(k, _)| k == "supervisor_restart"));
        assert!(warnings.iter().any(|(k, _)| k == "strategy_gateway_stale"));
    }
}

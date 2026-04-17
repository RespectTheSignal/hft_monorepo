//! close-positions — Gate 계정 전포지션 긴급 시장가 청산.

#![deny(rust_2018_idioms)]

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use hft_exchange_api::{ExchangeExecutor, OrderRequest, OrderSide, OrderType, TimeInForce};
use hft_exchange_gate::{GateAccountClient, GateExecutor, GateOpenPosition};
use hft_exchange_rest::{Credentials, RestClient};
use hft_types::{ExchangeId, Symbol};
use tracing::error;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(
    name = "close-positions",
    about = "긴급 전포지션 시장가 청산",
    long_about = "Gate.io USDT 선물 계정의 모든 열린 포지션을 즉시 시장가 청산합니다.\n전략 프로세스가 죽었거나 응답하지 않을 때 사용하는 비상 도구입니다."
)]
struct Cli {
    /// 대상 심볼 필터. 비어 있으면 전체 포지션.
    #[arg(long, value_delimiter = ',')]
    symbols: Vec<String>,
    /// 주문 전송 없이 로그만 출력한다.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
    /// 확인 프롬프트를 건너뛴다.
    #[arg(long, default_value_t = false)]
    yes: bool,
    /// 주문당 최대 수량. 0 이면 분할 없이 전량.
    #[arg(long, default_value_t = 0)]
    max_size_per_order: i64,
    /// 주문 간 딜레이 (ms).
    #[arg(long, default_value_t = 100)]
    delay_ms: u64,
}

fn init_telemetry() -> Result<hft_telemetry::TelemetryHandle> {
    let cfg = hft_config::load_all().map_err(|e| anyhow!("config load: {e}"))?;
    let tcfg = hft_telemetry::TelemetryConfig {
        otlp_endpoint: cfg.telemetry.otlp_endpoint.clone(),
        default_level: cfg.telemetry.default_level.clone(),
        json_logs: cfg.telemetry.stdout_json,
    };
    hft_telemetry::init(&tcfg, "close-positions").context("telemetry init failed")
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

fn close_side_for_size(size: i64) -> OrderSide {
    if size > 0 {
        OrderSide::Sell
    } else {
        OrderSide::Buy
    }
}

fn split_chunks(total: i64, max_size_per_order: i64) -> Vec<i64> {
    if total <= 0 {
        return Vec::new();
    }
    let chunk = if max_size_per_order > 0 {
        max_size_per_order
    } else {
        total
    };
    let mut remaining = total;
    let mut out = Vec::new();
    while remaining > 0 {
        let qty = remaining.min(chunk);
        out.push(qty);
        remaining -= qty;
    }
    out
}

fn select_targets<'a>(
    positions: &'a [GateOpenPosition],
    symbols: &[String],
) -> Vec<&'a GateOpenPosition> {
    positions
        .iter()
        .filter(|p| p.size != 0)
        .filter(|p| symbols.is_empty() || symbols.iter().any(|s| s == &p.contract))
        .collect()
}

fn install_signal_handler(cancelled: Arc<AtomicBool>) {
    let install_result = ctrlc::set_handler(move || {
        cancelled.store(true, Ordering::SeqCst);
        eprintln!("[close-positions] shutdown signal received — stopping after current request");
    });
    if let Err(e) = install_result {
        eprintln!("[close-positions] failed to install ctrlc handler: {e}");
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let _tele = init_telemetry()?;
    let creds = gate_credentials()?;
    let http = gate_rest_client(Duration::from_secs(10))?;
    let executor = GateExecutor::new(creds.clone())?.with_http(http.clone());
    let account = GateAccountClient::new(creds, http);
    let cancelled = Arc::new(AtomicBool::new(false));
    install_signal_handler(cancelled.clone());

    let snapshot = account
        .fetch_open_positions()
        .await
        .context("fetch_open_positions failed")?;
    let targets = select_targets(&snapshot, &cli.symbols);

    if targets.is_empty() {
        println!("열린 포지션 없음 — 종료");
        return Ok(ExitCode::SUCCESS);
    }

    println!("=== 청산 대상 ===");
    for p in &targets {
        let side = if p.size > 0 { "LONG" } else { "SHORT" };
        println!(
            "  {} : {} {} contracts (entry: {:.4})",
            p.contract,
            side,
            p.size.abs(),
            p.entry_price
        );
    }

    if !cli.yes && !cli.dry_run {
        print!("\n정말 전부 시장가 청산하시겠습니까? (yes/no): ");
        io::stdout().flush().context("stdout flush")?;
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("stdin read_line")?;
        if !input.trim().eq_ignore_ascii_case("yes") {
            println!("취소됨");
            return Ok(ExitCode::from(2));
        }
    }

    let mut success = 0;
    let mut fail = 0;

    for pos in &targets {
        if cancelled.load(Ordering::SeqCst) {
            println!("[WARN] signal received — remaining positions skipped");
            break;
        }
        let close_side = close_side_for_size(pos.size);
        for (chunk_idx, qty) in split_chunks(pos.size.abs(), cli.max_size_per_order)
            .into_iter()
            .enumerate()
        {
            if cli.dry_run {
                println!(
                    "[DRY-RUN] {} : {} {} contracts (market, reduce_only)",
                    pos.contract, close_side, qty
                );
                success += 1;
                continue;
            }

            let order_req = OrderRequest {
                exchange: ExchangeId::Gate,
                symbol: Symbol::new(&pos.contract),
                side: close_side,
                order_type: OrderType::Market,
                qty: qty as f64,
                price: None,
                reduce_only: true,
                tif: TimeInForce::Ioc,
                client_seq: 0,
                origin_ts_ns: 0,
                client_id: Arc::<str>::from(format!("t-close-{}-{chunk_idx}", pos.contract)),
            };

            match executor.place_order(order_req).await {
                Ok(ack) => {
                    println!(
                        "[OK] {} : closed {} contracts (order_id={})",
                        pos.contract, qty, ack.exchange_order_id
                    );
                    success += 1;
                }
                Err(e) => {
                    println!("[FAIL] {} : {e:#}", pos.contract);
                    fail += 1;
                }
            }

            if cli.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(cli.delay_ms)).await;
            }
        }
    }

    println!("\n=== 결과 ===");
    println!("  성공: {success}, 실패: {fail}");

    if !cli.dry_run {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let final_positions = account
            .fetch_open_positions()
            .await
            .context("final fetch_open_positions failed")?;
        let remaining = select_targets(&final_positions, &cli.symbols);
        if remaining.is_empty() {
            println!("  모든 포지션 청산 완료");
        } else {
            println!("  [WARN] 잔여 포지션:");
            for p in remaining {
                println!("    {} : {} contracts", p.contract, p.size);
            }
        }
    }

    Ok(if fail == 0 {
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
            eprintln!("[close-positions] fatal: {e:#}");
            error!(error = %format!("{e:#}"), "close-positions fatal error");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_close_side_logic() {
        assert_eq!(close_side_for_size(3), OrderSide::Sell);
        assert_eq!(close_side_for_size(-5), OrderSide::Buy);
    }

    #[test]
    fn test_chunk_split() {
        assert_eq!(split_chunks(25, 10), vec![10, 10, 5]);
        assert_eq!(split_chunks(25, 0), vec![25]);
    }

    #[test]
    fn test_symbol_filter() {
        let positions = vec![
            GateOpenPosition {
                contract: "BTC_USDT".into(),
                size: 1,
                entry_price: 1.0,
                mark_price: 1.0,
                unrealised_pnl: 0.0,
            },
            GateOpenPosition {
                contract: "ETH_USDT".into(),
                size: 2,
                entry_price: 1.0,
                mark_price: 1.0,
                unrealised_pnl: 0.0,
            },
        ];
        let selected = select_targets(&positions, &[String::from("BTC_USDT")]);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].contract, "BTC_USDT");
    }

    #[test]
    fn test_empty_positions_early_exit() {
        let positions = Vec::<GateOpenPosition>::new();
        let selected = select_targets(&positions, &[]);
        assert!(selected.is_empty());
    }
}

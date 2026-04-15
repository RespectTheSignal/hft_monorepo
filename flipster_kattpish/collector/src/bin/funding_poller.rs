use anyhow::Result;
use chrono::{TimeZone, Utc};
use collector::exchanges::flipster::FlipsterCollector;
use collector::ilp::IlpWriter;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

type HmacSha256 = Hmac<Sha256>;

async fn poll_binance(writer: &IlpWriter) -> Result<()> {
    let url = "https://fapi.binance.com/fapi/v1/premiumIndex";
    let arr: Vec<Value> = reqwest::get(url).await?.json().await?;
    for d in arr {
        let Some(sym) = d.get("symbol").and_then(|x| x.as_str()) else {
            continue;
        };
        let rate = d
            .get("lastFundingRate")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let next_ms = d
            .get("nextFundingTime")
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let next = Utc.timestamp_millis_opt(next_ms).single().unwrap_or_else(Utc::now);
        writer.write_funding("binance", sym, rate, next).await.ok();
    }
    Ok(())
}

/// GET /api/v1/market/funding-info — returns an array of FundingInfo:
///   { symbol, lastFundingRate, fundingRate, nextFundingTime, fundingIntervalHours, ... }
/// Requires HMAC auth (same scheme as the WS handshake). We reuse the
/// FlipsterCollector helper for the signature, and pull credentials from env.
async fn poll_flipster(writer: &IlpWriter) -> Result<()> {
    let Some(flip) = FlipsterCollector::from_env() else {
        warn!("flipster funding: FLIPSTER_API_KEY/SECRET not set — skip");
        return Ok(());
    };
    let path = "/api/v1/market/funding-info";
    let expires = Utc::now().timestamp() + 60;
    // recompute signature inline (FlipsterCollector::sign_path is private)
    let mut mac =
        HmacSha256::new_from_slice(flip.api_secret.as_bytes()).expect("hmac key");
    mac.update(format!("GET{path}{expires}").as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());

    let client = reqwest::Client::builder()
        .user_agent("flipster-research/0.1")
        .build()?;
    let resp = client
        .get(format!("https://trading-api.flipster.io{path}"))
        .header("api-key", &flip.api_key)
        .header("api-expires", expires.to_string())
        .header("api-signature", sig)
        .send()
        .await?;
    if !resp.status().is_success() {
        warn!(
            "flipster funding: HTTP {} — {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
        return Ok(());
    }
    let arr: Vec<Value> = resp.json().await?;
    info!(count = arr.len(), "flipster funding snapshot received");

    for d in arr {
        let Some(sym) = d.get("symbol").and_then(|x| x.as_str()) else {
            continue;
        };
        // Flipster encodes decimals as strings for precision.
        let parse_dec = |k: &str| -> Option<f64> {
            d.get(k).and_then(|x| {
                x.as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .or_else(|| x.as_f64())
            })
        };
        let rate = parse_dec("fundingRate")
            .or_else(|| parse_dec("lastFundingRate"))
            .unwrap_or(0.0);
        // nextFundingTime is a nanosecond-precision integer timestamp (string).
        let next_ns = d
            .get("nextFundingTime")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let next = if next_ns > 0 {
            Utc.timestamp_nanos(next_ns)
        } else {
            Utc::now()
        };
        writer.write_funding("flipster", sym, rate, next).await.ok();
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let qdb_host = std::env::var("QDB_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let qdb_port: u16 = std::env::var("QDB_ILP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9009);
    let writer = IlpWriter::connect(&qdb_host, qdb_port, 64)?;

    loop {
        info!("polling funding rates");
        if let Err(e) = poll_binance(&writer).await {
            warn!(error = %e, "binance funding poll failed");
        }
        if let Err(e) = poll_flipster(&writer).await {
            warn!(error = %e, "flipster funding poll failed");
        }
        writer.flush().await.ok();
        tokio::time::sleep(Duration::from_secs(60 * 30)).await; // 30 min
    }
}

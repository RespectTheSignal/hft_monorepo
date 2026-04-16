use flipster_client::{FlipsterClient, FlipsterConfig, OrderParams, OrderType, Side};
use std::collections::HashMap;
use std::time::Instant;

#[tokio::main]
async fn main() {
    let raw = std::fs::read_to_string("/tmp/flipster_cookies.json").expect("read cookies");
    let cookies: HashMap<String, String> = serde_json::from_str(&raw).expect("parse cookies");

    let config = FlipsterConfig {
        session_id_bolts: cookies.get("session_id_bolts").cloned().unwrap_or_default(),
        session_id_nuts: cookies.get("session_id_nuts").cloned().unwrap_or_default(),
        ajs_user_id: cookies.get("ajs_user_id").cloned().unwrap_or_default(),
        cf_bm: cookies.get("__cf_bm").cloned().unwrap_or_default(),
        ga: cookies.get("_ga").cloned().unwrap_or_default(),
        ga_rh8fm2jkcm: cookies.get("_ga_RH8FM2JKCM").cloned().unwrap_or_default(),
        ajs_anonymous_id: cookies.get("ajs_anonymous_id").cloned().unwrap_or_default(),
        analytics_session_id: cookies.get("analytics_session_id").cloned().unwrap_or_default(),
        internal: cookies.get("internal").cloned().unwrap_or("false".into()),
        referral_path: cookies.get("referral_path").cloned().unwrap_or_default(),
        referrer_symbol: cookies.get("referrer_symbol").cloned().unwrap_or_default(),
        dry_run: false,
        proxy: None,
    };

    let client = FlipsterClient::new(config);
    let price = 75000.0;

    // Warmup
    let r = client
        .place_order(
            "BTCUSDT.PERP",
            OrderParams::builder()
                .side(Side::Long)
                .price(price)
                .amount(1.0)
                .order_type(OrderType::Market)
                .build(),
        )
        .await
        .expect("warmup open");
    let slot: u32 = r.raw["position"]["slot"].as_u64().unwrap() as u32;
    client.close_position("BTCUSDT.PERP", slot, price).await.expect("warmup close");
    println!("warmup done\n");

    let mut open_lats = Vec::new();
    let mut close_lats = Vec::new();

    for i in 0..10 {
        let t0 = Instant::now();
        let r = client
            .place_order(
                "BTCUSDT.PERP",
                OrderParams::builder()
                    .side(Side::Long)
                    .price(price)
                    .amount(1.0)
                    .order_type(OrderType::Market)
                    .build(),
            )
            .await;
        let open_ms = t0.elapsed().as_micros() as f64 / 1000.0;

        match r {
            Ok(resp) => {
                let slot = resp.raw["position"]["slot"].as_u64().unwrap() as u32;

                let t1 = Instant::now();
                let _ = client.close_position("BTCUSDT.PERP", slot, price).await;
                let close_ms = t1.elapsed().as_micros() as f64 / 1000.0;

                open_lats.push(open_ms);
                close_lats.push(close_ms);
                println!(
                    "[{:2}] open: {:6.1} ms  close: {:6.1} ms  round: {:6.1} ms",
                    i + 1, open_ms, close_ms, open_ms + close_ms,
                );
            }
            Err(e) => {
                println!("[{:2}] open: {:6.1} ms  ERROR: {}", i + 1, open_ms, e);
                open_lats.push(open_ms);
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }

    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let min = |v: &[f64]| v.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = |v: &[f64]| v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    println!("\n--- Rust reqwest + HTTP/2 (open) ---");
    println!("avg: {:.1} ms  min: {:.1} ms  max: {:.1} ms", avg(&open_lats), min(&open_lats), max(&open_lats));
    println!("--- Rust reqwest + HTTP/2 (close) ---");
    println!("avg: {:.1} ms  min: {:.1} ms  max: {:.1} ms", avg(&close_lats), min(&close_lats), max(&close_lats));
}

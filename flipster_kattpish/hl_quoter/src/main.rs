//! hl_quoter binary — latency-arb maker quoter for SUI/ARB.

use anyhow::{Context, Result};
use ethers::signers::LocalWallet;
use futures_util::{SinkExt, StreamExt};
use hl_pairs::exec::{agent_address, LiveCfg, LiveExec};
use parking_lot::Mutex as PLMutex;
use hl_quoter::quoter::{
    decide, Action, QuoterCfg,
    SENTINEL_FAILED_PLACE, SENTINEL_INFLIGHT_CANCEL, SENTINEL_INFLIGHT_PLACE,
};
use hl_quoter::state::{MyQuote, QuoterState, Side};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
struct Coin {
    name: String,           // HL coin
    bin_symbol: String,     // BTC_USDT etc on Binance
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hl_quoter=info")),
        )
        .init();

    let coins_env = std::env::var("HQ_COINS").unwrap_or_else(|_| "SUI,ARB".into());
    let coins: Vec<Coin> = coins_env.split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .map(|n| Coin { bin_symbol: format!("{n}_USDT").to_lowercase().replace("_","").to_string() + "@bookTicker", name: n })
        .collect();
    info!("quoter coins: {:?}", coins.iter().map(|c| &c.name).collect::<Vec<_>>());

    let dry = std::env::var("HQ_DRY").unwrap_or_else(|_| "1".into()) == "1";
    info!("DRY_RUN = {}", dry);

    let cfg_env = LiveCfg::from_env().context("HLP_AGENT_KEY missing in .env")?;
    let exec = if !dry {
        info!(agent = %agent_address(&cfg_env.agent_key)?, max_size_usd = cfg_env.max_size_usd, testnet = cfg_env.testnet,
              "LIVE quoter — placing real HL orders");
        Some(Arc::new(LiveExec::new(cfg_env).await?))
    } else { None };

    // Global rate limiter — at most N actions per rolling 60s window.
    // HL allows ~1200/min/IP; we cap conservatively at 600.
    let rate_cap: usize = std::env::var("HQ_RATE_CAP_PER_MIN").ok().and_then(|s| s.parse().ok()).unwrap_or(600);
    let action_log: Arc<PLMutex<std::collections::VecDeque<Instant>>> = Arc::new(PLMutex::new(std::collections::VecDeque::with_capacity(rate_cap*2)));

    let mut qcfg = QuoterCfg::default();
    if let Some(v) = std::env::var("HQ_SIZE_USD").ok().and_then(|s| s.parse().ok()) { qcfg.size_usd = v; }
    if let Some(v) = std::env::var("HQ_SAFETY_BP").ok().and_then(|s| s.parse().ok()) { qcfg.safety_bp = v; }
    if let Some(v) = std::env::var("HQ_MIN_REPLACE_MS").ok().and_then(|s| s.parse().ok()) { qcfg.min_replace_ms = v; }
    if let Some(v) = std::env::var("HQ_MAX_INVENTORY_COIN").ok().and_then(|s| s.parse().ok()) { qcfg.max_inventory_coin = v; }
    info!("quoter cfg: size=${} safety={}bp min_replace={}ms max_inv_coin={}",
        qcfg.size_usd, qcfg.safety_bp, qcfg.min_replace_ms, qcfg.max_inventory_coin);
    let state = QuoterState::new();

    // === Binance WS streams (combined) ===
    let bin_streams: Vec<String> = coins.iter()
        .map(|c| format!("{}usdt@bookTicker", c.name.to_lowercase()))
        .collect();
    let bin_url = format!("wss://stream.binance.com:9443/stream?streams={}", bin_streams.join("/"));
    let state_b = state.clone();
    tokio::spawn(async move { run_binance_ws(bin_url, state_b).await });

    // === HL WS bbo per coin ===
    for c in &coins {
        let coin = c.name.clone();
        let st = state.clone();
        tokio::spawn(async move { run_hl_bbo_ws(coin, st).await });
    }

    // === HL userFills (master address) — track fills → net_pos ===
    let master_addr = std::env::var("HLP_MASTER_ADDRESS").ok();
    if let Some(addr) = master_addr.clone() {
        let st = state.clone();
        let watch_coins: Vec<String> = coins.iter().map(|c| c.name.clone()).collect();
        tokio::spawn(async move { run_hl_user_fills_ws(addr, st, watch_coins).await });
    } else {
        warn!("HLP_MASTER_ADDRESS missing — net_pos won't track real fills (inventory cap won't work)");
    }

    // === Decision loop — 50 ms tick (faster than 250 ms HL lag) ===
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        tick.tick().await;
        for c in &coins {
            for side in [Side::Bid, Side::Ask] {
                let cs = match state.snapshot(&c.name) { Some(s) => s, None => continue };
                let action = decide(&qcfg, &cs, side);
                match action {
                    Action::Hold => {}
                    Action::Place { side, price, size } => {
                        if !rate_check(&action_log, rate_cap) { continue; }
                        if let Some(le) = &exec {
                            execute_place(le.clone(), &state, c.name.clone(), side, price, size).await;
                        } else {
                            debug!("[DRY] {} PLACE {:?} {:.6} sz={:.4}", c.name, side, price, size);
                        }
                    }
                    Action::Cancel { side, oid } => {
                        if !rate_check(&action_log, rate_cap) { continue; }
                        if let Some(le) = &exec {
                            execute_cancel(le.clone(), &state, c.name.clone(), side, oid).await;
                        } else {
                            debug!("[DRY] {} CANCEL {:?} oid={}", c.name, side, oid);
                            state.set_my_quote(&c.name, side, None);
                        }
                    }
                }
            }
        }
    }
}

fn rate_check(log: &Arc<PLMutex<std::collections::VecDeque<Instant>>>, cap: usize) -> bool {
    let now = Instant::now();
    let mut g = log.lock();
    while let Some(&front) = g.front() {
        if now.duration_since(front) > Duration::from_secs(60) { g.pop_front(); } else { break; }
    }
    if g.len() >= cap { return false; }
    g.push_back(now);
    true
}

async fn execute_place(
    le: Arc<LiveExec>, state: &QuoterState, coin: String, side: Side, price: f64, raw_size: f64,
) {
    if le.killed() { return; }
    let size = match le.round_size(&coin, raw_size) {
        Some(s) if s > 0.0 => s, _ => return,
    };
    if size * price < 11.0 { return; }
    // Placeholder mark — prevents next tick duplicate PLACE.
    state.set_my_quote(&coin, side, Some(MyQuote {
        oid: SENTINEL_INFLIGHT_PLACE, price, size, placed_at: Instant::now(),
    }));
    let is_buy = side == Side::Bid;
    let st = state.clone();
    tokio::spawn(async move {
        match le.place_alo(&coin, is_buy, price, size, /*reduce_only=*/false).await {
            Ok(oid) => {
                info!("PLACE {} {} sz={:.4} px={:.6} oid={}", coin, if is_buy {"BID"} else {"ASK"}, size, price, oid);
                st.set_my_quote(&coin, side, Some(MyQuote { oid, price, size, placed_at: Instant::now() }));
            }
            Err(e) => {
                warn!("PLACE {} {} fail: {}", coin, if is_buy {"BID"} else {"ASK"}, e);
                st.set_my_quote(&coin, side, Some(MyQuote {
                    oid: SENTINEL_FAILED_PLACE, price, size, placed_at: Instant::now(),
                }));
            }
        }
    });
}

async fn execute_cancel(
    le: Arc<LiveExec>, state: &QuoterState, coin: String, side: Side, oid: u64,
) {
    // Mark as in-flight cancel — Hold until SDK confirms.
    state.set_my_quote(&coin, side, Some(MyQuote {
        oid: SENTINEL_INFLIGHT_CANCEL, price: 0.0, size: 0.0, placed_at: Instant::now(),
    }));
    let st = state.clone();
    tokio::spawn(async move {
        match le.cancel(&coin, oid).await {
            Ok(()) => debug!("CANCEL {} oid={} ok", coin, oid),
            Err(e) => debug!("CANCEL {} oid={} (may have filled): {}", coin, oid, e),
        }
        st.set_my_quote(&coin, side, None);
    });
}

// ---- Binance WS combined stream ----
async fn run_binance_ws(url: String, state: QuoterState) {
    loop {
        info!("Binance WS connecting…");
        match tokio_tungstenite::connect_async(&url).await {
            Ok((mut ws, _)) => {
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(t) = msg {
                        if let Some((sym, bid, ask)) = parse_binance_book(&t) {
                            // sym is uppercase + USDT, e.g. "SUIUSDT"
                            let coin = sym.trim_end_matches("USDT").to_string();
                            state.update_bin(&coin, bid, ask);
                        }
                    }
                }
                warn!("Binance WS disconnected, reconnecting");
            }
            Err(e) => warn!("Binance WS connect err: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn parse_binance_book(text: &str) -> Option<(String, f64, f64)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let data = v.get("data").unwrap_or(&v);
    let sym = data.get("s")?.as_str()?.to_string();
    let bid: f64 = data.get("b")?.as_str()?.parse().ok()?;
    let ask: f64 = data.get("a")?.as_str()?.parse().ok()?;
    Some((sym, bid, ask))
}

// ---- HL WS bbo channel ----
async fn run_hl_bbo_ws(coin: String, state: QuoterState) {
    loop {
        info!("HL WS connecting for {}…", coin);
        match tokio_tungstenite::connect_async("wss://api.hyperliquid.xyz/ws").await {
            Ok((mut ws, _)) => {
                let sub = serde_json::json!({"method":"subscribe","subscription":{"type":"bbo","coin":coin}});
                if ws.send(Message::Text(sub.to_string())).await.is_err() { continue; }
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(t) = msg {
                        if let Some((bid, ask)) = parse_hl_bbo(&t) {
                            state.update_hl(&coin, bid, ask);
                        }
                    }
                }
                warn!("HL WS for {} disconnected", coin);
            }
            Err(e) => warn!("HL WS connect err: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn parse_hl_bbo(text: &str) -> Option<(f64, f64)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("channel")?.as_str()? != "bbo" { return None; }
    let bbo = v.get("data")?.get("bbo")?.as_array()?;
    if bbo.len() < 2 { return None; }
    let bid: f64 = bbo[0].as_object()?.get("px")?.as_str()?.parse().ok()?;
    let ask: f64 = bbo[1].as_object()?.get("px")?.as_str()?.parse().ok()?;
    Some((bid, ask))
}

// ---- HL userFills WS — track real fills for inventory ----
async fn run_hl_user_fills_ws(master: String, state: QuoterState, watch: Vec<String>) {
    loop {
        info!("HL userFills WS connecting for {}…", &master[..10]);
        match tokio_tungstenite::connect_async("wss://api.hyperliquid.xyz/ws").await {
            Ok((mut ws, _)) => {
                let sub = serde_json::json!({"method":"subscribe","subscription":{"type":"userFills","user":master}});
                if ws.send(Message::Text(sub.to_string())).await.is_err() { continue; }
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(t) = msg {
                        let v: serde_json::Value = match serde_json::from_str(&t) { Ok(v) => v, Err(_) => continue };
                        if v.get("channel").and_then(|x| x.as_str()) != Some("userFills") { continue; }
                        let data = match v.get("data") { Some(d) => d, None => continue };
                        // skip snapshot
                        if data.get("isSnapshot").and_then(|x| x.as_bool()).unwrap_or(false) { continue; }
                        let fills = match data.get("fills").and_then(|x| x.as_array()) { Some(a) => a, None => continue };
                        for f in fills {
                            let coin = f.get("coin").and_then(|x| x.as_str()).unwrap_or("");
                            if !watch.iter().any(|w| w == coin) { continue; }
                            let sz: f64 = f.get("sz").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                            let side = f.get("side").and_then(|x| x.as_str()).unwrap_or("");
                            // 'B' = buy, 'A' = sell. add_to_pos signs by side.
                            let signed = if side == "B" { sz } else { -sz };
                            state.add_to_pos(coin, signed);
                            info!("FILL {} {} sz={} → net_pos updated", coin, if side=="B" {"BUY"} else {"SELL"}, sz);
                        }
                    }
                }
                warn!("HL userFills WS disconnected");
            }
            Err(e) => warn!("HL userFills connect err: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// Silence unused warning for now
#[allow(dead_code)]
fn _unused(_: &LocalWallet) {}

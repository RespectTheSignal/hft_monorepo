//! HL latency-arb taker — fires on Binance BBO move when HL is stale.
//!
//! Logic (per coin, per BIN tick):
//!   1. New BIN bid/ask arrives.
//!   2. Compute fair = (BIN_bid + BIN_ask)/2.
//!   3. If HL_ask < fair * (1 - cost_bp): HL is stale-cheap → BUY HL.
//!      If HL_bid > fair * (1 + cost_bp): HL is stale-rich → SELL HL.
//!   4. Entry = IOC limit (Binance-driven price + 1 bp slip cap).
//!   5. Immediately on fill, ALO post-only exit at fair (maker rebate).
//!   6. If exit unfilled in N ms → IOC close (taker fallback).
//!
//! Position state per coin: at most one open trade at a time. New BIN
//! signal while we hold = ignored (skip).

use anyhow::{Context, Result};
use ethers::types::H160;
use futures_util::{SinkExt, StreamExt};
use hl_pairs::exec::{agent_address, LiveCfg, LiveExec};
use parking_lot::Mutex as PLMutex;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

#[derive(Debug, Clone, Default)]
struct CoinState {
    bin_bid: f64,
    bin_ask: f64,
    hl_bid: f64,
    hl_ask: f64,
    /// 1 = long, -1 = short, 0 = flat. Set when we fire entry; cleared on close.
    open_side: i32,
    open_size_coin: f64,
    open_entry_px: f64,
    open_at: Option<Instant>,
    /// Maker exit oid (Alo) — None if not placed yet.
    exit_oid: Option<u64>,
    /// Cooldown after a trade so we don't re-fire instantly.
    last_action_at: Option<Instant>,
}

#[derive(Debug, Clone)]
struct Cfg {
    coins: Vec<String>,
    /// Required edge (bp) after assumed costs to fire entry.
    edge_bp: f64,
    /// Slippage cap on entry IOC (bp above target).
    entry_slip_bp: f64,
    /// Maker exit target distance from entry (bp).
    exit_target_bp: f64,
    /// Cancel maker exit + IOC close after this many ms (default 20s —
    /// longer than p90 lag of all 4 coins so maker has time to fill).
    exit_timeout_ms: u64,
    /// Stop-loss bp: if HL mid moves this many bp against us before
    /// timeout, force IOC close. Caps adverse hold time.
    stop_bp: f64,
    /// Cooldown between trades on same coin (ms).
    cooldown_ms: u64,
    /// Per-trade size USD.
    size_usd: f64,
}

fn cfg_from_env() -> Cfg {
    let coins = std::env::var("HQ_COINS").unwrap_or_else(|_| "SUI,ARB,AVAX,LINK".into())
        .split(',').map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty()).collect();
    Cfg {
        coins,
        edge_bp: std::env::var("HT_EDGE_BP").ok().and_then(|s| s.parse().ok()).unwrap_or(20.0),
        entry_slip_bp: std::env::var("HT_ENTRY_SLIP_BP").ok().and_then(|s| s.parse().ok()).unwrap_or(5.0),
        exit_target_bp: std::env::var("HT_EXIT_TARGET_BP").ok().and_then(|s| s.parse().ok()).unwrap_or(3.0),
        exit_timeout_ms: std::env::var("HT_EXIT_TIMEOUT_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(20000),
        stop_bp: std::env::var("HT_STOP_BP").ok().and_then(|s| s.parse().ok()).unwrap_or(15.0),
        cooldown_ms: std::env::var("HT_COOLDOWN_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(3000),
        size_usd: std::env::var("HT_SIZE_USD").ok().and_then(|s| s.parse().ok()).unwrap_or(12.0),
    }
}

type SharedState = Arc<PLMutex<HashMap<String, CoinState>>>;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .init();

    let cfg = cfg_from_env();
    info!("hl_taker cfg: {:?}", cfg);

    let lcfg = LiveCfg::from_env().context("HLP_AGENT_KEY missing")?;
    info!(agent = %agent_address(&lcfg.agent_key)?, max_size_usd = lcfg.max_size_usd,
          testnet = lcfg.testnet, "LIVE taker — placing real HL orders");
    let exec = Arc::new(LiveExec::new(lcfg.clone()).await?);

    let state: SharedState = Arc::new(PLMutex::new(
        cfg.coins.iter().map(|c| (c.clone(), CoinState::default())).collect(),
    ));

    // Binance WS combined
    let bin_streams: Vec<String> = cfg.coins.iter()
        .map(|c| format!("{}usdt@bookTicker", c.to_lowercase())).collect();
    let bin_url = format!("wss://stream.binance.com:9443/stream?streams={}", bin_streams.join("/"));
    {
        let st = state.clone(); let cfg2 = cfg.clone(); let exec2 = exec.clone();
        tokio::spawn(async move { run_binance_ws(bin_url, st, cfg2, exec2).await });
    }

    // HL bbo per coin
    for c in &cfg.coins {
        let coin = c.clone(); let st = state.clone();
        tokio::spawn(async move { run_hl_bbo_ws(coin, st).await });
    }

    // userFills WS — fill detect
    let master = std::env::var("HLP_MASTER_ADDRESS").context("HLP_MASTER_ADDRESS")?;
    {
        let st = state.clone(); let watch: Vec<String> = cfg.coins.clone();
        tokio::spawn(async move { run_user_fills(master, st, watch).await });
    }

    // Exit timeout sweeper
    {
        let st = state.clone(); let exec2 = exec.clone(); let cfg2 = cfg.clone();
        tokio::spawn(async move { exit_sweeper(st, exec2, cfg2).await });
    }

    info!("hl_taker running for {:?}", cfg.coins);
    tokio::signal::ctrl_c().await.ok();
    Ok(())
}

// --- Binance WS: every tick → check signal → maybe fire entry ---
async fn run_binance_ws(url: String, state: SharedState, cfg: Cfg, exec: Arc<LiveExec>) {
    loop {
        info!("BIN WS connecting…");
        match tokio_tungstenite::connect_async(&url).await {
            Ok((mut ws, _)) => {
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(t) = msg {
                        let v: serde_json::Value = match serde_json::from_str(&t) { Ok(v)=>v, Err(_)=>continue };
                        let data = v.get("data").unwrap_or(&v);
                        let sym = match data.get("s").and_then(|x| x.as_str()) { Some(s)=>s, None=>continue };
                        let coin = sym.trim_end_matches("USDT").to_string();
                        let bid: f64 = data.get("b").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                        let ask: f64 = data.get("a").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                        if bid <= 0.0 || ask <= 0.0 { continue; }
                        check_signal(&coin, bid, ask, &state, &cfg, &exec);
                    }
                }
                warn!("BIN WS dc");
            }
            Err(e) => warn!("BIN WS err: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn check_signal(coin: &str, bin_bid: f64, bin_ask: f64, state: &SharedState, cfg: &Cfg, exec: &Arc<LiveExec>) {
    let mut g = state.lock();
    let cs = match g.get_mut(coin) { Some(s)=>s, None=>return };
    cs.bin_bid = bin_bid; cs.bin_ask = bin_ask;
    if cs.open_side != 0 { return; } // already in trade
    if cs.hl_bid <= 0.0 || cs.hl_ask <= 0.0 { return; }
    if let Some(t) = cs.last_action_at {
        if t.elapsed().as_millis() < cfg.cooldown_ms as u128 { return; }
    }
    let bin_mid = (bin_bid + bin_ask) * 0.5;
    let edge_thresh = cfg.edge_bp / 1e4;
    // Stale-cheap: HL ask < BIN mid * (1 - edge) → BUY HL ask
    if cs.hl_ask < bin_mid * (1.0 - edge_thresh) {
        // Marketable IOC: cross BIN's fair + slip, ensures fast fill before
        // HL catches up (don't wait for BIN-tracking price to come to us).
        let entry_px = bin_mid * (1.0 + cfg.entry_slip_bp / 1e4);
        let raw_sz = cfg.size_usd / cs.hl_ask;
        cs.last_action_at = Some(Instant::now()); // optimistic — prevents re-fire on next tick
        let coin_s = coin.to_string();
        let exec2 = exec.clone(); let state2 = state.clone();
        let exit_target = cs.hl_ask * (1.0 + cfg.exit_target_bp / 1e4);
        drop(g);
        tokio::spawn(async move {
            fire_entry_and_exit(coin_s, true, entry_px, raw_sz, exit_target, exec2, state2).await;
        });
        return;
    }
    // Stale-rich: HL bid > BIN mid * (1 + edge) → SELL HL bid
    if cs.hl_bid > bin_mid * (1.0 + edge_thresh) {
        let entry_px = bin_mid * (1.0 - cfg.entry_slip_bp / 1e4);
        let raw_sz = cfg.size_usd / cs.hl_bid;
        cs.last_action_at = Some(Instant::now());
        let coin_s = coin.to_string();
        let exec2 = exec.clone(); let state2 = state.clone();
        let exit_target = cs.hl_bid * (1.0 - cfg.exit_target_bp / 1e4);
        drop(g);
        tokio::spawn(async move {
            fire_entry_and_exit(coin_s, false, entry_px, raw_sz, exit_target, exec2, state2).await;
        });
    }
}

async fn fire_entry_and_exit(coin: String, is_buy: bool, entry_px: f64, raw_sz: f64,
                              exit_target: f64, exec: Arc<LiveExec>, state: SharedState) {
    let size = match exec.round_size(&coin, raw_sz) { Some(s) if s>0.0 => s, _ => return };
    if size * entry_px < 11.0 { return; }
    let entry_px_r = round_sig(entry_px, 5);
    info!("ENTRY {} {} sz={:.5} px={:.6}", coin, if is_buy {"BUY"} else {"SELL"}, size, entry_px_r);
    let fill = match exec.place_ioc(&coin, is_buy, entry_px_r, size, false).await {
        Ok((fpx, fsz)) => { info!("ENTRY {} fill={:.6} sz={:.5}", coin, fpx, fsz); (fpx, fsz) }
        Err(e) => { warn!("ENTRY {} fail: {}", coin, e); return; }
    };
    let (fill_px, fill_sz) = fill;
    // Post maker exit
    let exit_target_r = round_sig(exit_target, 5);
    let close_buy = !is_buy;
    info!("EXIT_POST {} {} px={:.6} sz={:.5}", coin, if close_buy {"BUY"} else {"SELL"}, exit_target_r, fill_sz);
    let exit_oid = match exec.place_alo(&coin, close_buy, exit_target_r, fill_sz, true).await {
        Ok(oid) => Some(oid),
        Err(e) => { warn!("EXIT_POST {} fail: {} — will IOC close after timeout", coin, e); None }
    };
    {
        let mut g = state.lock();
        if let Some(cs) = g.get_mut(&coin) {
            cs.open_side = if is_buy { 1 } else { -1 };
            cs.open_size_coin = fill_sz;
            cs.open_entry_px = fill_px;
            cs.open_at = Some(Instant::now());
            cs.exit_oid = exit_oid;
        }
    }
}

async fn exit_sweeper(state: SharedState, exec: Arc<LiveExec>, cfg: Cfg) {
    let mut iv = tokio::time::interval(Duration::from_millis(500));
    loop {
        iv.tick().await;
        let candidates: Vec<(String, i32, f64, Option<u64>, &'static str)> = {
            let g = state.lock();
            g.iter().filter_map(|(coin, cs)| {
                if cs.open_side == 0 { return None; }
                let opened = cs.open_at?;
                let timed_out = opened.elapsed().as_millis() >= cfg.exit_timeout_ms as u128;
                // Stop loss check — HL mid moved against us by stop_bp
                let hl_mid = (cs.hl_bid + cs.hl_ask) * 0.5;
                let mut stopped = false;
                if hl_mid > 0.0 && cs.open_entry_px > 0.0 {
                    let move_bp = (hl_mid - cs.open_entry_px) / cs.open_entry_px * 1e4;
                    let against = if cs.open_side == 1 { -move_bp } else { move_bp };
                    if against >= cfg.stop_bp {
                        stopped = true;
                    }
                }
                if timed_out || stopped {
                    let reason = if stopped { "stop" } else { "timeout" };
                    Some((coin.clone(), cs.open_side, cs.open_size_coin, cs.exit_oid, reason))
                } else { None }
            }).collect()
        };
        for (coin, side, sz, exit_oid, reason) in candidates {
            // Cancel maker exit if still open
            if let Some(oid) = exit_oid {
                if let Err(e) = exec.cancel(&coin, oid).await {
                    info!("CANCEL maker exit {} oid={} (may be filled): {}", coin, oid, e);
                }
            }
            // Force IOC close
            let close_buy = side == -1;
            let coin_state_snapshot = state.lock().get(&coin).cloned();
            let ref_px = match coin_state_snapshot {
                Some(cs) if cs.hl_bid > 0.0 && cs.hl_ask > 0.0 =>
                    if close_buy { cs.hl_ask } else { cs.hl_bid },
                _ => continue,
            };
            let close_px = if close_buy { ref_px * 1.005 } else { ref_px * 0.995 };
            let close_px = round_sig(close_px, 5);
            match exec.place_ioc(&coin, close_buy, close_px, sz, true).await {
                Ok((fpx, fsz)) => info!("EXIT_{} {} {} fill={:.6} sz={:.5}", reason.to_uppercase(), coin, if close_buy {"BUY"} else {"SELL"}, fpx, fsz),
                Err(e) => warn!("EXIT_{} {} fail: {}", reason.to_uppercase(), coin, e),
            }
            // Clear state — userFills WS will also confirm
            let mut g = state.lock();
            if let Some(cs) = g.get_mut(&coin) {
                cs.open_side = 0; cs.open_size_coin = 0.0;
                cs.open_at = None; cs.exit_oid = None;
            }
        }
    }
}

// --- HL bbo WS ---
async fn run_hl_bbo_ws(coin: String, state: SharedState) {
    loop {
        match tokio_tungstenite::connect_async("wss://api.hyperliquid.xyz/ws").await {
            Ok((mut ws, _)) => {
                let sub = serde_json::json!({"method":"subscribe","subscription":{"type":"bbo","coin":coin}});
                if ws.send(Message::Text(sub.to_string())).await.is_err() { continue; }
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(t) = msg {
                        let v: serde_json::Value = match serde_json::from_str(&t) { Ok(v)=>v, Err(_)=>continue };
                        if v.get("channel").and_then(|x| x.as_str()) != Some("bbo") { continue; }
                        let bbo = match v.get("data").and_then(|d| d.get("bbo")).and_then(|x| x.as_array()) { Some(a)=>a, None=>continue };
                        if bbo.len() < 2 { continue; }
                        let bid: f64 = bbo[0].as_object().and_then(|o| o.get("px")).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                        let ask: f64 = bbo[1].as_object().and_then(|o| o.get("px")).and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                        let mut g = state.lock();
                        if let Some(cs) = g.get_mut(&coin) { cs.hl_bid = bid; cs.hl_ask = ask; }
                    }
                }
            }
            Err(e) => warn!("HL WS {}: {}", coin, e),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// --- userFills WS — close position when maker exit fills ---
async fn run_user_fills(master: String, state: SharedState, watch: Vec<String>) {
    let _ = H160::from_str(master.trim_start_matches("0x")); // sanity
    loop {
        match tokio_tungstenite::connect_async("wss://api.hyperliquid.xyz/ws").await {
            Ok((mut ws, _)) => {
                let sub = serde_json::json!({"method":"subscribe","subscription":{"type":"userFills","user":master}});
                if ws.send(Message::Text(sub.to_string())).await.is_err() { continue; }
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(t) = msg {
                        let v: serde_json::Value = match serde_json::from_str(&t) { Ok(v)=>v, Err(_)=>continue };
                        if v.get("channel").and_then(|x| x.as_str()) != Some("userFills") { continue; }
                        let data = match v.get("data") { Some(d)=>d, None=>continue };
                        if data.get("isSnapshot").and_then(|x| x.as_bool()).unwrap_or(false) { continue; }
                        let fills = match data.get("fills").and_then(|x| x.as_array()) { Some(a)=>a, None=>continue };
                        for f in fills {
                            let coin = f.get("coin").and_then(|x| x.as_str()).unwrap_or("");
                            if !watch.iter().any(|w| w == coin) { continue; }
                            let sz: f64 = f.get("sz").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                            let px: f64 = f.get("px").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                            let side = f.get("side").and_then(|x| x.as_str()).unwrap_or("");
                            let closed_pnl: f64 = f.get("closedPnl").and_then(|x| x.as_str()).and_then(|s| s.parse().ok()).unwrap_or(0.0);
                            info!("FILL {} {} sz={} px={} closedPnl={}", coin, if side=="B" {"BUY"} else {"SELL"}, sz, px, closed_pnl);
                            // If closedPnl != 0 → this was a closing fill → clear state
                            if closed_pnl != 0.0 {
                                let mut g = state.lock();
                                if let Some(cs) = g.get_mut(coin) {
                                    cs.open_side = 0; cs.open_size_coin = 0.0;
                                    cs.open_at = None; cs.exit_oid = None;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => warn!("userFills err: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn round_sig(x: f64, sig: i32) -> f64 {
    if x == 0.0 { return 0.0; }
    let mag = x.abs().log10().floor() as i32;
    let scale = 10f64.powi(sig - 1 - mag);
    (x * scale).round() / scale
}

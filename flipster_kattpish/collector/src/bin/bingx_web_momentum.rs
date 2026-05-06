//! BingX Web↔API momentum signal generator.
//!
//! Subscribes simultaneously to:
//!   A) Public WS  `wss://open-api-swap.bingx.com/swap-market`     (~30 Hz)
//!   B) UI WS      `wss://ws-uswap.we-api.com/market`               (~4 Hz)
//!
//! Hypothesis (verified, 15 min × 25 syms, 2026-05-06):
//!   When |api_mid − web_mid| ≥ 3 bp, market continues in the api direction
//!   for the next 60–120 s. Aggregate +3.45 bp / +7.94 bp directional return
//!   at thr=2bp / thr=3bp respectively (h=120s). Comfortably beats the
//!   Banner-fee taker round-trip of 3.2 bp.
//!
//! On signal, emits a single `TradeSignal` ZMQ frame (action="entry"),
//! schedules the matching action="exit" after `BWM_HOLD_S` seconds.
//! `bingx_executor` (separate binary, already running) consumes them and
//! routes to BingX market open/close via the web API path.
//!
//! Env (all optional, sane defaults):
//!
//! | var                  | default                                  |
//! |----------------------|------------------------------------------|
//! | BWM_ACCOUNT_ID       | BINGX_WEB_MOM_v1                         |
//! | BWM_SIZE_USD         | 20                                       |
//! | BWM_THRESHOLD_BP     | 3.0                                      |
//! | BWM_COOLDOWN_S       | 30                                       |
//! | BWM_HOLD_S           | 90                                       |
//! | BWM_SYMBOLS          | BTC,ETH,SOL,HYPE,DOGE,XRP,BNB,LINK,...   |
//! | SIGNAL_PUB_ADDR      | ipc:///tmp/bingx_web_signal.sock         |

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::{SecondsFormat, Utc};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

const PUBLIC_WS: &str = "wss://open-api-swap.bingx.com/swap-market";
const UI_WS_HOST: &str = "wss://ws-uswap.we-api.com/market";
const UI_BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const UI_ORIGIN: &str = "https://bingx.com";

const DEFAULT_SYMBOLS: &str = "BTC,ETH,SOL,HYPE,DOGE,XRP,BNB,LINK,SUI,AVAX,PENDLE,ZEC,TAO";

#[derive(Clone)]
struct Cfg {
    account_id: String,
    size_usd: f64,
    threshold_bp: f64,
    cooldown_s: f64,
    hold_s: f64,
    /// Take-profit: close when api_mid moved this many bp in our favor since entry.
    take_profit_bp: f64,
    /// Stop-loss: close when api_mid moved this many bp against us since entry.
    stop_loss_bp: f64,
    symbols: Vec<String>,
}

impl Cfg {
    fn from_env() -> Self {
        let symbols = env::var("BWM_SYMBOLS")
            .unwrap_or_else(|_| DEFAULT_SYMBOLS.to_string())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Self {
            account_id: env::var("BWM_ACCOUNT_ID")
                .unwrap_or_else(|_| "BINGX_WEB_MOM_v1".into()),
            size_usd: env::var("BWM_SIZE_USD").ok().and_then(|s| s.parse().ok()).unwrap_or(20.0),
            threshold_bp: env::var("BWM_THRESHOLD_BP").ok().and_then(|s| s.parse().ok()).unwrap_or(3.0),
            cooldown_s: env::var("BWM_COOLDOWN_S").ok().and_then(|s| s.parse().ok()).unwrap_or(30.0),
            hold_s: env::var("BWM_HOLD_S").ok().and_then(|s| s.parse().ok()).unwrap_or(90.0),
            take_profit_bp: env::var("BWM_TP_BP").ok().and_then(|s| s.parse().ok()).unwrap_or(5.0),
            stop_loss_bp: env::var("BWM_SL_BP").ok().and_then(|s| s.parse().ok()).unwrap_or(10.0),
            symbols,
        }
    }
}

#[derive(Default, Clone, Copy)]
struct SymState {
    api_bid: Option<f64>,
    api_ask: Option<f64>,
    api_mid: Option<f64>,
    web_mid: Option<f64>,
    last_signal: Option<Instant>,
}

/// Tracks an entry that has been published but not yet closed. Used by both
/// the TP/SL price monitor (on every api tick) and the timeout fallback so
/// only one EXIT actually fires per entry — whichever calls `remove(base)`
/// first wins.
#[derive(Clone, Copy)]
struct OpenEntry {
    side_long: bool, // true=long, false=short
    entry_mid: f64,
    position_id: i64,
}

struct App {
    cfg: Cfg,
    state: DashMap<String, SymState>,
    open_entries: DashMap<String, OpenEntry>,
    next_position_id: Mutex<i64>,
    pub_socket: Mutex<zmq::Socket>,
}

impl App {
    fn new(cfg: Cfg) -> Result<Self> {
        let addr = env::var("SIGNAL_PUB_ADDR")
            .unwrap_or_else(|_| "ipc:///tmp/bingx_web_signal.sock".to_string());
        let ctx = zmq::Context::new();
        let socket = ctx.socket(zmq::PUB)?;
        socket.set_sndhwm(10_000)?;
        socket.set_linger(100)?;
        socket.bind(&addr)?;
        info!(addr = %addr, "[bwm] PUB bound");
        Ok(Self {
            cfg,
            state: DashMap::new(),
            open_entries: DashMap::new(),
            next_position_id: Mutex::new(1),
            pub_socket: Mutex::new(socket),
        })
    }

    async fn next_pid(&self) -> i64 {
        let mut g = self.next_position_id.lock().await;
        let v = *g;
        *g += 1;
        v
    }

    async fn publish(&self, signal: &TradeSignal) -> Result<()> {
        let payload = serde_json::to_vec(signal)?;
        let g = self.pub_socket.lock().await;
        g.send(&payload, zmq::DONTWAIT).map_err(|e| anyhow!("zmq send: {e}"))?;
        Ok(())
    }

    /// Fire an EXIT signal for `base` if it's still in `open_entries`.
    /// Returns true if we won the race and published; false if another
    /// caller (TP/SL or timeout) already exited it.
    async fn fire_exit(self: &Arc<Self>, base: &str, reason: &str, exit_price_hint: f64) -> bool {
        let oe = match self.open_entries.remove(base) {
            Some((_, oe)) => oe,
            None => return false,
        };
        let side = if oe.side_long { "long" } else { "short" };
        let exit_price = self
            .state
            .get(base)
            .and_then(|s| s.api_mid)
            .unwrap_or(exit_price_hint);
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
        let signal = TradeSignal {
            account_id: self.cfg.account_id.clone(),
            base: base.to_string(),
            action: "exit".into(),
            side: side.into(),
            size_usd: self.cfg.size_usd,
            flipster_price: exit_price,
            gate_price: 0.0,
            position_id: oe.position_id,
            timestamp: now,
            flipster_bid: None,
            flipster_ask: None,
        };
        let pnl_bp = if oe.side_long {
            (exit_price - oe.entry_mid) / oe.entry_mid * 10_000.0
        } else {
            (oe.entry_mid - exit_price) / oe.entry_mid * 10_000.0
        };
        info!(
            base = %base, side, reason, entry = oe.entry_mid, exit = exit_price,
            pnl_bp = format!("{pnl_bp:+.2}"),
            pid = oe.position_id, "[bwm] EXIT signal"
        );
        if let Err(e) = self.publish(&signal).await {
            warn!(error = %e, "[bwm] publish exit failed");
        }
        true
    }

    /// Called whenever api_mid is updated. Two responsibilities:
    ///   1. If we have an open entry for this base, check TP/SL — fire EXIT early.
    ///   2. Else, if api↔web disagreement crosses threshold (and cooldown passed),
    ///      fire ENTRY + schedule the timeout-based EXIT fallback.
    async fn check_signal(self: &Arc<Self>, base: &str) {
        let snap = match self.state.get(base) {
            Some(r) => *r.value(),
            None => return,
        };
        let api = match snap.api_mid { Some(v) if v > 0.0 => v, _ => return };

        // ── Path 1: TP/SL on open entry ──
        if let Some(oe) = self.open_entries.get(base).map(|r| *r.value()) {
            // PnL bp from api mid (signed by side).
            let raw_bp = (api - oe.entry_mid) / oe.entry_mid * 10_000.0;
            let pnl_bp = if oe.side_long { raw_bp } else { -raw_bp };
            if pnl_bp >= self.cfg.take_profit_bp {
                self.fire_exit(base, "TP", api).await;
            } else if pnl_bp <= -self.cfg.stop_loss_bp {
                self.fire_exit(base, "SL", api).await;
            }
            return; // never re-enter while in position
        }

        // ── Path 2: New entry signal ──
        let web = match snap.web_mid { Some(v) if v > 0.0 => v, _ => return };
        let diff_bp = (api - web) / web * 10_000.0;
        if diff_bp.abs() < self.cfg.threshold_bp { return; }

        // cooldown
        if let Some(t) = snap.last_signal {
            if t.elapsed().as_secs_f64() < self.cfg.cooldown_s { return; }
        }

        // Determine side: api > web ⇒ market moving up ⇒ LONG.
        let side_long = diff_bp > 0.0;
        let side = if side_long { "long" } else { "short" };
        // Mark cooldown now (before await) so a flood of ticks doesn't spam.
        self.state.alter(base, |_, mut s| {
            s.last_signal = Some(Instant::now());
            s
        });

        let pid = self.next_pid().await;
        // Register the open entry — TP/SL monitor needs it to be present
        // before we publish the entry. (And before any later api tick fires
        // check_signal again.) DashMap insert is atomic.
        self.open_entries.insert(base.to_string(), OpenEntry {
            side_long,
            entry_mid: api,
            position_id: pid,
        });

        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);
        let entry = TradeSignal {
            account_id: self.cfg.account_id.clone(),
            base: base.to_string(),
            action: "entry".into(),
            side: side.into(),
            size_usd: self.cfg.size_usd,
            flipster_price: api,
            gate_price: web,
            position_id: pid,
            timestamp: now,
            flipster_bid: snap.api_bid,
            flipster_ask: snap.api_ask,
        };
        info!(
            base = %base, side = %side, diff_bp = format!("{diff_bp:+.2}"),
            api, web, pid, "[bwm] ENTRY signal"
        );
        if let Err(e) = self.publish(&entry).await {
            warn!(error = %e, "[bwm] publish entry failed");
        }

        // Schedule timeout-based EXIT fallback. If TP/SL already fired the exit
        // before this fires, `fire_exit` is a no-op (entry already removed).
        let app = self.clone();
        let base_owned = base.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(app.cfg.hold_s)).await;
            app.fire_exit(&base_owned, "TIMEOUT", api).await;
        });
    }
}

/// Wire-format signal — matches `pairs_core::TradeSignal` field order so the
/// existing `bingx_executor` consumes it without changes. Only the fields the
/// executor reads are populated. `flipster_bid`/`flipster_ask` carry the
/// BingX API best bid/ask at signal time so the executor can place a maker
/// limit entry at the passive side.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct TradeSignal {
    account_id: String,
    base: String,
    action: String,
    side: String,
    size_usd: f64,
    flipster_price: f64,
    gate_price: f64,
    position_id: i64,
    timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    flipster_bid: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flipster_ask: Option<f64>,
}

// ── PUBLIC WS ─────────────────────────────────────────────────────────────

async fn run_public_ws(app: Arc<App>) {
    let mut attempt = 0u32;
    loop {
        info!(url = PUBLIC_WS, "[bwm/public] connecting");
        match public_session(&app).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = (1u64 << attempt.min(5)).min(30);
                attempt = attempt.saturating_add(1);
                warn!(error = %e, retry_in = secs, "[bwm/public] disconnected");
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }
}

async fn public_session(app: &Arc<App>) -> Result<()> {
    let (mut ws, _) = connect_async(PUBLIC_WS).await?;
    for s in &app.cfg.symbols {
        let req = serde_json::json!({
            "id": uuid_simple(),
            "reqType": "sub",
            "dataType": format!("{s}-USDT@bookTicker"),
        });
        ws.send(Message::Text(req.to_string())).await?;
    }
    info!(n = app.cfg.symbols.len(), "[bwm/public] subscribed");
    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let text = match msg {
            Message::Binary(bytes) => match gunzip(&bytes) {
                Some(t) => t,
                None => continue,
            },
            Message::Text(t) => {
                if t == "Ping" { ws.send(Message::Text("Pong".into())).await?; continue; }
                t
            }
            Message::Ping(p) => { ws.send(Message::Pong(p)).await?; continue; }
            Message::Close(_) => return Err(anyhow!("public ws closed")),
            _ => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v, Err(_) => continue,
        };
        if let Some(p) = v.get("ping") {
            let _ = ws.send(Message::Text(format!("{{\"pong\":{p}}}"))).await;
            continue;
        }
        let dt = v.get("dataType").and_then(|x| x.as_str()).unwrap_or("");
        if !dt.contains("@bookTicker") { continue; }
        let sym = dt.split('@').next().unwrap_or("").trim_end_matches("-USDT").to_string();
        let data = match v.get("data") { Some(d) => d, None => continue };
        let bid = data.get("b").and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok());
        let ask = data.get("a").and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok());
        let (Some(bid), Some(ask)) = (bid, ask) else { continue };
        let mid = (bid + ask) * 0.5;
        app.state.entry(sym.clone()).and_modify(|s| {
            s.api_bid = Some(bid); s.api_ask = Some(ask); s.api_mid = Some(mid);
        }).or_insert(SymState {
            api_bid: Some(bid), api_ask: Some(ask), api_mid: Some(mid),
            web_mid: None, last_signal: None,
        });
        let app2 = app.clone();
        // Spawn the signal check off-loop so heavy churn doesn't stall recv.
        tokio::spawn(async move { app2.check_signal(&sym).await; });
    }
    Ok(())
}

// ── UI WS ─────────────────────────────────────────────────────────────────

async fn run_ui_ws(app: Arc<App>) {
    let mut attempt = 0u32;
    loop {
        let url = format!(
            "{UI_WS_HOST}?platformid=30&device_id={did}&channel=official\
             &device_brand=Linux_Chrome_120&traceId={tid}&device_level=high",
            did = uuid_simple(),
            tid = uuid_simple(),
        );
        info!(url = %url[..120.min(url.len())].to_string(), "[bwm/ui] connecting");
        match ui_session(&app, &url).await {
            Ok(_) => attempt = 0,
            Err(e) => {
                let secs = (1u64 << attempt.min(5)).min(30);
                attempt = attempt.saturating_add(1);
                warn!(error = %e, retry_in = secs, "[bwm/ui] disconnected");
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        }
    }
}

async fn ui_session(app: &Arc<App>, url: &str) -> Result<()> {
    let mut req = url.into_client_request()?;
    let h = req.headers_mut();
    h.insert("User-Agent", UI_BROWSER_UA.parse()?);
    h.insert("Origin", UI_ORIGIN.parse()?);
    let (mut ws, _) = connect_async(req).await?;
    for s in &app.cfg.symbols {
        let req = serde_json::json!({
            "dataType": format!("swap-bbo.{s}_USDT"),
            "id": uuid_simple(),
            "reqType": "sub",
        });
        ws.send(Message::Text(req.to_string())).await?;
    }
    info!(n = app.cfg.symbols.len(), "[bwm/ui] subscribed");
    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let text = match msg {
            Message::Binary(bytes) => match gunzip(&bytes) {
                Some(t) => t,
                None => continue,
            },
            Message::Text(t) => t,
            Message::Ping(p) => { ws.send(Message::Pong(p)).await?; continue; }
            Message::Close(_) => return Err(anyhow!("ui ws closed")),
            _ => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v, Err(_) => continue,
        };
        if let Some(p) = v.get("ping") {
            let _ = ws.send(Message::Text(format!("{{\"pong\":{p}}}"))).await;
            continue;
        }
        let dt = v.get("dataType").and_then(|x| x.as_str()).unwrap_or("");
        if !dt.starts_with("swap-bbo.") { continue; }
        let sym = dt.trim_start_matches("swap-bbo.").trim_end_matches("_USDT").to_string();
        let data = match v.get("data") { Some(d) => d, None => continue };
        let bid = data.get("maxBid").and_then(|x| x.get("price"))
            .and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok());
        let ask = data.get("minAsk").and_then(|x| x.get("price"))
            .and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok());
        let (Some(bid), Some(ask)) = (bid, ask) else { continue };
        let mid = (bid + ask) * 0.5;
        app.state.entry(sym.clone()).and_modify(|s| { s.web_mid = Some(mid); })
            .or_insert(SymState { web_mid: Some(mid), ..Default::default() });
        // Don't trigger signal check here — only on api updates (api is fresher).
    }
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────────────────

fn gunzip(bytes: &[u8]) -> Option<String> {
    use std::io::Read;
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut s = String::new();
    decoder.read_to_string(&mut s).ok()?;
    Some(s)
}

fn uuid_simple() -> String {
    // 32-hex (no dashes); both BingX endpoints just need uniqueness, not entropy.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) as u128;
    let t = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u128;
    let mixed = t.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(n);
    format!("{mixed:032x}")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Cfg::from_env();
    info!(
        account_id = %cfg.account_id,
        size_usd = cfg.size_usd,
        threshold_bp = cfg.threshold_bp,
        cooldown_s = cfg.cooldown_s,
        hold_s = cfg.hold_s,
        n_syms = cfg.symbols.len(),
        "[bwm] starting"
    );
    let app = Arc::new(App::new(cfg)?);

    let a1 = app.clone();
    let a2 = app.clone();
    tokio::spawn(async move { run_public_ws(a1).await });
    tokio::spawn(async move { run_ui_ws(a2).await });

    tokio::signal::ctrl_c().await?;
    info!("[bwm] shutdown");
    Ok(())
}

//! Live executor — Rust port of scripts/live_executor.py.
//!
//! Subscribes to the paper bot's trade_signal ZMQ feed, places orders on
//! Flipster + Gate, and writes a trade log compatible with the Python
//! version's JSONL schema.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use clap::Parser;
use dashmap::{DashMap, DashSet};
use tokio::sync::Mutex;

use flipster_client::FlipsterClient;
use gate_client::GateClient;

mod config;
mod cookies;
mod executor;
mod fill_publisher;
mod flipster_account;
mod flipster_contracts;
mod flipster_helpers;
mod flipster_ws;
mod gate_contracts;
mod gate_helpers;
mod questdb;
mod symbol_stats;
mod trade_log;
mod zmq_sub;

use crate::executor::{Executor, Stats};
use crate::trade_log::TradeLog;

#[derive(Parser, Debug)]
#[command(
    name = "executor",
    about = "Rust live executor for the pairs paper bot"
)]
struct Cli {
    /// Variant account_id to follow (e.g. T04_es35).
    #[arg(long)]
    variant: String,

    /// Notional size per entry, USD.
    #[arg(long, default_value_t = 5.0)]
    size_usd: f64,

    /// Comma-separated symbol whitelist (overrides blacklist if set).
    #[arg(long, default_value = "")]
    symbols: String,

    /// Comma-separated symbol blacklist.
    #[arg(long, default_value = "")]
    blacklist: String,

    /// Path to cookies.json produced by scripts/dump_cookies.py.
    #[arg(long, default_value = "~/.config/flipster_kattpish/cookies.json")]
    cookies_file: String,

    /// If true, parse and log signals but do NOT place orders.
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Cross-leverage cap on Gate.
    #[arg(long, default_value_t = 10)]
    gate_leverage: u32,

    /// Path to JSONL trade log.
    #[arg(long, default_value = "logs/live_trades.jsonl")]
    trade_log: PathBuf,

    /// Single-leg Flipster mode: skip Gate placement entirely. Used for
    /// `gate_lead` strategy where the bet is directional Flipster following
    /// a Gate move — we already see Gate's move (no need to hedge it).
    #[arg(long, default_value_t = false)]
    flipster_only: bool,

    /// Flipster margin mode for new orders. "Cross" or "Isolated"
    /// (case-sensitive — Flipster's API expects exactly those tokens).
    /// Default Cross to preserve gate_lead executor behavior; SR
    /// executor sets --margin Isolated.
    #[arg(long, default_value = "Cross")]
    margin: String,

    /// Account-level Flipster trade mode. Valid: "ONE_WAY" or
    /// "MULTIPLE_POSITIONS". Default empty = leave whatever is set on
    /// the account. SR executor passes --trade-mode MULTIPLE_POSITIONS
    /// so each entry on the same symbol gets its own slot.
    #[arg(long, default_value = "")]
    trade_mode: String,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 16)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let legacy_filters = std::env::var("EXEC_LEGACY_FILTERS").as_deref() == Ok("1");
    tracing::info!(
        variant = %cli.variant,
        size_usd = cli.size_usd,
        dry_run = cli.dry_run,
        margin = %cli.margin,
        trade_mode = %cli.trade_mode,
        legacy_filters,
        "starting executor"
    );
    if legacy_filters {
        tracing::warn!(
            "EXEC_LEGACY_FILTERS=1 — applying chop/dyn_filter/adjusted_size in executor (Phase 3b rollback). Coordinator filters in collector are still active unless COORDINATOR_FILTERS=0 there."
        );
    } else {
        tracing::info!(
            "EXEC_LEGACY_FILTERS off (default) — chop/dyn_filter/adjusted_size are owned by collector::coordinator. signal.size_usd is trusted as the final order size; executor's --size-usd CLI flag is bypassed for sizing decisions in this mode."
        );
    }

    // Bring up the fill_publisher PUB socket so the collector's coordinator
    // (Phase 3+) can see live fills/aborts in real time. Non-fatal: if the
    // bind fails, executor continues and `publish_*` are silent no-ops.
    if let Err(e) = fill_publisher::init() {
        tracing::warn!(error = %e, "fill_publisher init failed (continuing without)");
    }

    // Optionally switch the account into MULTIPLE_POSITIONS mode so the
    // executor can stack multiple slots on the same symbol. Only applied
    // if the operator passed --trade-mode explicitly. Failure is non-
    // fatal — log and continue; the account stays at whatever mode it
    // had before.
    if !cli.trade_mode.is_empty() {
        match flipster_account::set_trade_mode(&cli.trade_mode).await {
            Ok(()) => tracing::info!(mode = %cli.trade_mode, "[init] flipster trade-mode applied"),
            Err(e) => {
                tracing::warn!(error = %e, mode = %cli.trade_mode, "[init] flipster trade-mode failed")
            }
        }
    }

    let cookies_path = cookies::expand_tilde(&cli.cookies_file);
    let bundle = cookies::load(&cookies_path)?;
    tracing::info!(
        flipster = bundle.flipster.len(),
        gate = bundle.gate.len(),
        ts = %bundle.ts,
        "[cookies] loaded"
    );

    // Build clients.
    let flipster_client = Arc::new(FlipsterClient::from_all_cookies(&bundle.flipster));
    let gate_client = Arc::new(
        GateClient::from_cookies(&bundle.gate).map_err(|e| anyhow::anyhow!("gate client: {e}"))?,
    );
    if cli.flipster_only {
        tracing::info!("[init] --flipster-only — skipping Gate cookie validation");
    } else if !gate_client.is_logged_in().await {
        anyhow::bail!("gate cookies do not authenticate — re-run dump_cookies.py");
    } else {
        tracing::info!("[init] gate cookies validated");
    }

    // Public-API client for QuestDB queries + Gate contract specs.
    let qdb_http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let gate_contracts = gate_contracts::load(&qdb_http).await;

    // Load Flipster instrument specs (max leverage, tick size etc.) from
    // the public web snapshot endpoint, authed via cookies.
    let flip_cookie_hdr = bundle
        .flipster
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    let flip_contracts = match flipster_contracts::load(&qdb_http, &flip_cookie_hdr).await {
        Ok(m) => {
            tracing::info!(n = m.len(), "[flip-contracts] loaded");
            m
        }
        Err(e) => {
            tracing::warn!(error = %e, "[flip-contracts] load failed; default 10x for all");
            flipster_contracts::ContractMap::default()
        }
    };

    // Spawn Flipster private WS in the background.
    let flip_state = flipster_ws::spawn(bundle.flipster.clone());

    // Spawn the v1-API account sync (REST poll + WS subscribe). Runs in
    // parallel with the v2 cookie WS above. Reads FLIPSTER_API_KEY /
    // FLIPSTER_API_SECRET from env; if unset, the helper logs a warning
    // and the returned state stays at default (no failure).
    let _account_state = flipster_account::spawn();

    // Build Executor.
    let blacklist: HashSet<String> = cli
        .blacklist
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().to_string())
        .collect();
    let whitelist: HashSet<String> = cli
        .symbols
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().to_string())
        .collect();
    let trade_log = TradeLog::new(cli.trade_log.clone());
    let sym_stats_path = std::env::var("GL_DYN_FILTER_PATH")
        .ok()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from("/home/teamreporter/.config/flipster_kattpish/sym_stats.json")
        });
    let sym_stats = symbol_stats::SymbolStatsStore::new(Some(sym_stats_path));
    let exec = Arc::new(Executor {
        variant: cli.variant.clone(),
        size_usd: cli.size_usd,
        dry_run: cli.dry_run,
        flipster_only: cli.flipster_only,
        margin: cli.margin.clone(),
        multiple_positions: cli.trade_mode.eq_ignore_ascii_case("MULTIPLE_POSITIONS"),
        blacklist,
        whitelist,
        flipster: flipster_client,
        gate: gate_client,
        gate_contracts,
        gate_leverage: cli.gate_leverage,
        gate_leverage_set: Mutex::new(HashSet::new()),
        flip_state,
        qdb_http,
        open_positions: DashMap::new(),
        seen_entries: DashSet::new(),
        seen_exits: DashSet::new(),
        stats: Stats::default(),
        trade_log,
        shutting_down: AtomicBool::new(false),
        unmatched_first_seen: DashMap::new(),
        order_first_seen: DashMap::new(),
        cancel_failure_count: Arc::new(DashMap::new()),
        abandoned_orders: Arc::new(DashMap::new()),
        last_signal: DashMap::new(),
        sym_stats: sym_stats.clone(),
        flip_contracts,
    });

    // Periodic save of symbol stats (every 30s).
    {
        let store = sym_stats.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.tick().await;
            loop {
                tick.tick().await;
                store.save().await;
            }
        });
    }

    // Periodic dynamic-filter report (every 5 min).
    {
        let exec_r = exec.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
            tick.tick().await;
            loop {
                tick.tick().await;
                let report = exec_r.sym_stats.report();
                if !report.is_empty() {
                    let mut lines = String::from("[DYN-REPORT] symbol stats:\n");
                    for (sym, n, avg_pnl, avg_paper, avg_slip, scale, cool) in
                        report.iter().take(25)
                    {
                        lines.push_str(&format!(
                            "  {:<10} n={:<4} pnl={:+5.2}bp paper={:+5.2}bp slip={:5.2}bp scale={:.2}{}\n",
                            sym, n, avg_pnl, avg_paper, avg_slip, scale,
                            if *cool { " [COOL]" } else { "" }
                        ));
                    }
                    tracing::info!("{}", lines);
                }
            }
        });
    }

    // Periodic STATS dump.
    {
        let exec = exec.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.tick().await;
            loop {
                tick.tick().await;
                exec.stats.print();
            }
        });
    }

    // Sweeper: every 500ms, force-close any tracked position older than 3s.
    // Without this, orphans accumulate when the exit signal arrives during
    // our LIMIT fill wait (collector publishes exit faster than fill confirms).
    {
        let exec = exec.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
            tick.tick().await;
            loop {
                tick.tick().await;
                exec.clone().sweep_stale_positions(3.0).await;
                // Also catch abort-orphans: positions on Flipster that we
                // never registered (LIMIT-IOC abort race). 5s grace lets
                // normal entry flow finish before we declare orphan.
                exec.clone().sweep_abort_orphans(5.0).await;
                // Cancel stuck entry orders that limit_ioc_entry's own
                // cancel call failed to land. 8s grace > LIMIT-IOC's
                // ~3s wait, so we never race normal flow.
                exec.clone().sweep_stale_orders(8.0).await;
            }
        });
    }

    // ZMQ subscriber feeds the dispatcher.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<zmq_sub::TradeSignal>();
    let variant = cli.variant.clone();
    tokio::task::spawn_blocking(move || zmq_sub::run_subscriber(&variant, tx));

    while let Some(ev) = rx.recv().await {
        exec.dispatch(ev);
    }
    Ok(())
}

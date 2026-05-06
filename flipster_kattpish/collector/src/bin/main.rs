use anyhow::Result;
use chrono::{DateTime, Utc};
use collector::exchanges::{
    binance::BinanceCollector, bitget::BitgetCollector, bybit::BybitCollector,
    flipster::FlipsterCollector, gate::GateCollector, run_collector,
};
use collector::ilp::IlpWriter;
use collector::latency::{LatencyEngine, LatencySignal};
use collector::market_watcher;
use collector::model::{BookTick, ExchangeName};
use collector::strategies;
use tokio::sync::{broadcast, mpsc};
use tracing_subscriber::EnvFilter;

fn symbols_env(key: &str, default: &[&str]) -> Vec<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_else(|| default.iter().map(|s| s.to_string()).collect())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let qdb_host = std::env::var("QDB_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let qdb_port: u16 = std::env::var("QDB_ILP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9009);
    let writer = IlpWriter::connect(&qdb_host, qdb_port, 512)?;

    // ZMQ PUB for trade_signal — lets live executors receive entries/exits
    // in-process (<10ms) instead of polling QuestDB (1-2s commit lag).
    // Non-fatal if bind fails; paper bot still functions via QuestDB writes.
    if let Err(e) = collector::signal_publisher::init() {
        tracing::warn!(error = %e, "signal_publisher init failed — falling back to QuestDB-only");
    }

    // ZMQ SUB for executor → collector feedback (fills + aborts). Phase 2:
    // logs every event; future phases (coordinator, ILP merge) hang on the
    // returned broadcast Sender. Non-fatal: connect happens lazily so the
    // channel is created even when the executor is offline.
    let fill_events_tx = collector::fill_subscriber::spawn();

    // Phase 3a: parallel learner. Subscribes to fill_events, pairs entry+exit
    // by (account_id, position_id), updates a collector-side sym_stats store
    // independent of the executor's. Phase 3b uses it for pre-publish
    // filtering via the coordinator.
    let live_stats_store = collector::live_stats::spawn(fill_events_tx.subscribe());

    // Phase 3b: hand the coordinator the live-stats handle so it can apply
    // chop_detect / sym_stats / adjusted_size filters before publishing.
    // Override default behaviour with COORDINATOR_FILTERS=0 (pass-through)
    // or CHOP_WINDOW_MS=<n> (default 15000).
    collector::coordinator::init(live_stats_store);

    // Broadcast bus for downstream consumers (e.g. latency calculator, paper bot).
    // 1M slots absorbs the replay producer's burst rate without lagging the
    // slower paper-bot subscribers during a backtest. In live mode the extra
    // capacity is never allocated beyond what fits actual backlog.
    let (tx, _rx) = broadcast::channel::<BookTick>(1_048_576);

    // Latency engine: consumes ticks, detects binance price moves before
    // flipster reflection, writes latency_log and emits signals.
    let entry_threshold_bp: f64 = std::env::var("LATENCY_ENTRY_BP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.05);
    let (sig_tx, mut sig_rx) = mpsc::channel::<LatencySignal>(1024);
    let engine = LatencyEngine::new(writer.clone(), entry_threshold_bp, sig_tx);
    engine.clone().spawn(tx.subscribe());

    // Drain signal channel so try_send never fills. Real consumers
    // (paper bot) can replace this.
    tokio::spawn(async move {
        while let Some(s) = sig_rx.recv().await {
            tracing::debug!(?s, "latency signal");
        }
    });

    // Paper-bot strategies run in-process so we share the single broadcast
    // bus (no duplicate WS connections). Gate on env var.
    //
    // PAPER_VARIANTS=1 spawns multiple parameter variants in parallel — each
    // gets its own PaperBook + EWMA state, all subscribe to the same tick
    // broadcast, and trades are tagged with `account_id` for SQL-side comparison.
    if std::env::var("PAPER_BOT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        let base = strategies::Params::from_env();
        let multi = std::env::var("PAPER_VARIANTS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // Slow-gap state shared across variants that opt-in. The watcher
        // queries QuestDB against the hedge venue's bookticker table.
        let hedge_table = match base.hedge_venue {
            ExchangeName::Gate => "gate_bookticker",
            ExchangeName::Binance => "binance_bookticker",
            ExchangeName::Hyperliquid => "hyperliquid_bookticker",
            _ => "gate_bookticker",
        };
        let questdb_url =
            std::env::var("QUESTDB_HTTP_URL").unwrap_or_else(|_| "http://127.0.0.1:9000".into());
        let window_minutes: i64 = std::env::var("PAIRS_GAP_WINDOW_MINUTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        let update_interval_secs: u64 = std::env::var("PAIRS_GAP_UPDATE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let gap_state = market_watcher::new_shared();
        market_watcher::spawn(
            gap_state.clone(),
            questdb_url,
            hedge_table.into(),
            window_minutes,
            update_interval_secs,
        );

        let variants: Vec<strategies::Params> = if multi {
            let mut v = Vec::new();
            // All variants use the EWMA signal path (slow_avg was empirically
            // weaker during the 2026-04-15 test — see project_pairs_results).
            // The F/G/H slots are now fixed-size companions of the winning
            // EWMA variants, to isolate sizing effect from signal effect.
            let mk = |id: &str,
                      entry: f64,
                      exit: f64,
                      min_std: f64,
                      stop: f64,
                      fixed_size: Option<f64>|
             -> strategies::Params {
                let mut p = base.clone();
                p.account_id = id.to_string();
                p.pairs_entry_sigma = entry;
                p.pairs_exit_sigma = exit;
                p.pairs_min_std_bp = min_std;
                p.pairs_stop_sigma = stop;
                p.pairs_fixed_size_usd = fixed_size;
                p
            };
            // ---- Kelly sizing (existing 5) ----
            v.push(mk("A_baseline", 2.5, 0.3, 0.3, 100.0, None));
            v.push(mk("B_minstd2", 2.5, 0.3, 2.0, 100.0, None));
            v.push(mk("C_minstd3", 2.5, 0.3, 3.0, 100.0, None));
            v.push(mk("D_high_sig", 4.0, 0.3, 0.3, 100.0, None));
            v.push(mk("E_stop6", 2.5, 0.3, 2.0, 6.0, None));
            // ---- Fixed-size companions of the winning variants ----
            // F mirrors B with $200 notional per trade (fair sizing control).
            // G mirrors C with $200 (tighter min_std + same fixed size).
            // H mirrors B with $1000 notional (capital scaling test — is the
            // edge robust at 5x the base size given slippage we're not
            // modeling yet?).
            v.push(mk("F_minstd2_fix200", 2.5, 0.3, 2.0, 100.0, Some(200.0)));
            v.push(mk("G_minstd3_fix200", 2.5, 0.3, 3.0, 100.0, Some(200.0)));
            v.push(mk("H_minstd2_fix1000", 2.5, 0.3, 2.0, 100.0, Some(1000.0)));
            // I mirrors G with $1000 notional — tests whether C/G's tight-edge
            // (+3.23 bp / 63% win at $200) survives 5x scaling. If I lags G in
            // bp, slippage / fill-latency is eating the edge and live sizing
            // needs a cap; if I ≈ G, we have capacity headroom.
            v.push(mk("I_minstd3_fix1000", 2.5, 0.3, 3.0, 100.0, Some(1000.0)));

            // ---- J~N: structural fixes after the 50ms fill-latency audit ----
            // The 0-ms edge of C/G (+3.7 bp) collapsed to −5 bp at 50ms. Root
            // cause: cross-venue round-trip cost (2× spread + fee) is larger
            // than the mean-reversion amplitude on mid-caps, and the EWMA
            // signal + lack of stealth amplified adverse selection.
            //
            // J_core is the essential fix (cost filter + hard spread cap +
            // bn cooldown). K/L/M flip one extra knob each on top of J so we
            // can isolate which of the optional improvements matters. N is
            // the "kitchen sink" — everything on at once.
            let mk_improved = |id: &str,
                               rolling_sec: f64,
                               auto_bl_min: usize,
                               asym_exit: bool,
                               max_hold_ms: i64|
             -> strategies::Params {
                let mut p = base.clone();
                p.account_id = id.to_string();
                p.pairs_entry_sigma = 2.5;
                p.pairs_exit_sigma = 0.3;
                // min_std relaxed: the spread-aware cost filter (#1) is a
                // structurally tighter gate than min_std, and keeping
                // min_std=3 together with max_spread=1 excludes every symbol
                // on the book (majors have std<1bp). Cost filter is the
                // operative gate for these variants.
                p.pairs_min_std_bp = 0.5;
                p.pairs_stop_sigma = 100.0;
                p.pairs_fixed_size_usd = Some(200.0);
                // #1 cost filter — the single biggest expected impact
                p.pairs_spread_edge_safety = 1.2;
                // #2 hard spread cap (major pairs only)
                p.max_entry_spread_bp = 1.0;
                // #3 stealth — avoid the moment adverse selection peaks
                p.pairs_bn_change_cooldown_ms = 200;
                // Optional knobs per variant
                p.pairs_rolling_window_sec = rolling_sec;
                p.pairs_auto_blacklist_min_trades = auto_bl_min;
                p.pairs_asymmetric_exit = asym_exit;
                p.pairs_max_hold_ms = max_hold_ms;
                p
            };
            // Baseline improved core — still EWMA, no auto-bl, no asym exit,
            // default 60s hold. Measures the combined effect of #1+#2+#3.
            v.push(mk_improved("J_core", 0.0, 0, false, 60_000));
            // Swap EWMA for a 30s rolling window (#4).
            v.push(mk_improved("K_roll", 30.0, 0, false, 60_000));
            // Add per-symbol auto-blacklist (#5).
            v.push(mk_improved("L_autobl", 0.0, 20, false, 60_000));
            // Add asymmetric exit + longer max_hold so the signal can play out (#6).
            v.push(mk_improved("M_asymexit", 0.0, 0, true, 300_000));
            // Everything on — rolling + auto-bl + asym exit + long hold.
            v.push(mk_improved("N_full", 30.0, 20, true, 300_000));

            // ---- O/P: pure asym-exit ablation on the winning min_std=3 path ----
            // M_asymexit hinted that asymmetric exit pushes converged-only pnl
            // from +3.04 bp to +4.71 bp. Apply the same trick to G/I (our best
            // baseline path) with ZERO other changes — no cost filter, no
            // spread cap, no stealth — so we can attribute the delta to asym
            // exit alone. If O beats G (and P beats I) by a clean margin, we
            // have a new production candidate.
            let mut o = mk("O_minstd3_fix200_asym", 2.5, 0.3, 3.0, 100.0, Some(200.0));
            o.pairs_asymmetric_exit = true;
            o.pairs_max_hold_ms = 300_000;
            v.push(o);

            let mut p = mk("P_minstd3_fix1000_asym", 2.5, 0.3, 3.0, 100.0, Some(1000.0));
            p.pairs_asymmetric_exit = true;
            p.pairs_max_hold_ms = 300_000;
            v.push(p);
            // Keep market_watcher running — its output is still useful for
            // external analysis/dashboard — but none of the variants consume
            // it via `gap_state` anymore.
            let _ = &gap_state;
            v
        } else {
            vec![base.clone()]
        };

        // In BACKTEST mode, force every variant onto the backtest tag + flag.
        // This is cheaper than making the operator set PAPER_MODE_TAG and
        // BACKTEST_MODE in the env, and guarantees live vs replay data never
        // mix in position_log.
        let backtest_start: Option<DateTime<Utc>> = std::env::var("BACKTEST_START_TS")
            .ok()
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| t.with_timezone(&Utc));
        let backtest_end: Option<DateTime<Utc>> = std::env::var("BACKTEST_END_TS")
            .ok()
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| t.with_timezone(&Utc));
        let is_backtest = backtest_start.is_some() && backtest_end.is_some();
        let sweep_mode = std::env::var("BACKTEST_SWEEP")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // The sweep grid is always appended to the variant list — both live
        // and backtest. This way we get the same 27-variant lineup running
        // in paper mode (mode='paper') AND in any backtest replay, so live
        // numbers and backtest numbers can be compared variant-for-variant.
        // The historical `BACKTEST_SWEEP` env is now a no-op kept for
        // backward compat (it never disables anything).
        let _ = sweep_mode;
        let mut variants = variants;
        if true {
            let mk_sweep = |id: &str,
                            min_std: f64,
                            entry_sigma: f64,
                            asym: bool,
                            max_hold_ms: i64|
             -> strategies::Params {
                let mut p = base.clone();
                p.account_id = id.to_string();
                p.pairs_entry_sigma = entry_sigma;
                p.pairs_exit_sigma = 0.3;
                p.pairs_min_std_bp = min_std;
                p.pairs_stop_sigma = 100.0;
                p.pairs_fixed_size_usd = Some(1000.0);
                p.pairs_asymmetric_exit = asym;
                p.pairs_max_hold_ms = max_hold_ms;
                p
            };
            variants.extend(vec![
                // ---- min_std sweep (pivot: es=2.5, asym, 300s) ----
                mk_sweep("S01_minstd1", 1.0, 2.5, true, 300_000),
                mk_sweep("S02_minstd2", 2.0, 2.5, true, 300_000),
                mk_sweep("S03_pivot", 3.0, 2.5, true, 300_000), // pivot
                mk_sweep("S04_minstd4", 4.0, 2.5, true, 300_000),
                // ---- entry_sigma sweep ----
                mk_sweep("S05_es20", 3.0, 2.0, true, 300_000),
                mk_sweep("S06_es30", 3.0, 3.0, true, 300_000),
                // ---- max_hold sweep ----
                mk_sweep("S07_hold60", 3.0, 2.5, true, 60_000),
                mk_sweep("S08_hold120", 3.0, 2.5, true, 120_000),
                mk_sweep("S09_hold600", 3.0, 2.5, true, 600_000),
                // ---- asym_exit ablation ----
                mk_sweep("S10_noasym60", 3.0, 2.5, false, 60_000), // = original I
                mk_sweep("S11_noasym300", 3.0, 2.5, false, 300_000),
                // ---- T-series: untested optimal combos ----
                mk_sweep("T01_best", 4.0, 3.0, true, 300_000), // S04 min_std + S06 entry_sigma
                mk_sweep("T02_best_long", 4.0, 3.0, true, 600_000), // T01 + longer hold
                mk_sweep("T03_minstd5", 5.0, 3.0, true, 300_000), // push min_std further
                mk_sweep("T04_es35", 4.0, 3.5, true, 300_000), // push entry_sigma further
            ]);
            tracing::info!(
                total = variants.len(),
                "BACKTEST_SWEEP=1 — appended 11-variant sweep grid to legacy set"
            );
        }

        // BACKTEST_TAG lets two parallel backtests (e.g. one with
        // PAPER_FILL_LATENCY_MS=50, one with =200) tag their position_log
        // rows differently so the summaries don't merge.
        let backtest_tag = std::env::var("BACKTEST_TAG").unwrap_or_else(|_| "backtest".into());
        if is_backtest {
            for p in variants.iter_mut() {
                p.mode_tag = backtest_tag.clone();
                p.backtest_mode = true;
            }
        }

        for params in variants {
            tracing::info!(
                variant = %params.account_id,
                backtest = is_backtest,
                "paper_bot variant enabled"
            );
            let writer_s = writer.clone();
            let rx_s = tx.subscribe();
            tokio::spawn(async move {
                strategies::run(writer_s, rx_s, params).await;
            });
        }

        // Gate→Flipster lead-lag strategy. Runs as a paper bot alongside the
        // pairs variants. Set GATE_LEAD=0 to disable (default: enabled).
        //
        // BACKTEST_GL_SWEEP=1 spawns 4 variants with min_move_bp ∈ {15,20,25,30}
        // and an empty whitelist (full Binance∩Flipster universe). Each
        // variant gets its own account_id (GL_BT_mm15…mm30) so position_log
        // rows can be grouped per (variant, base) for whitelist screening.
        if std::env::var("GATE_LEAD").unwrap_or_else(|_| "1".into()) != "0" {
            let gl_sweep = is_backtest
                && std::env::var("BACKTEST_GL_SWEEP")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
            // BACKTEST_GL_ANCHOR_SWEEP=1 → fix min_move_bp at 30 and
            // sweep anchor_s ∈ {2, 3, 5, 7, 10} instead. Use this to
            // pick the best lookback window without touching mm.
            let anchor_sweep = is_backtest
                && std::env::var("BACKTEST_GL_ANCHOR_SWEEP")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
            let mm_grid: Vec<f64> = if gl_sweep {
                vec![15.0, 20.0, 25.0, 30.0]
            } else {
                vec![] // single default variant
            };
            let anchor_grid: Vec<f64> = if anchor_sweep {
                vec![2.0, 3.0, 5.0, 7.0, 10.0]
            } else {
                vec![]
            };
            let mut gl_variants: Vec<collector::gate_lead::GateLeadParams> = Vec::new();
            if gl_sweep {
                for mm in &mm_grid {
                    let mut p = collector::gate_lead::GateLeadParams::from_env();
                    p.backtest_mode = true;
                    p.min_move_bp = *mm;
                    p.account_id = format!("GL_BT_mm{}", *mm as i32);
                    p.mode_tag = backtest_tag.clone();
                    p.whitelist = Vec::new();
                    gl_variants.push(p);
                }
            } else if anchor_sweep {
                for a in &anchor_grid {
                    let mut p = collector::gate_lead::GateLeadParams::from_env();
                    p.backtest_mode = true;
                    p.min_move_bp = 30.0;
                    p.anchor_s = *a;
                    p.account_id = format!("GL_BT_a{}s", *a as i32);
                    p.mode_tag = backtest_tag.clone();
                    p.whitelist = Vec::new();
                    gl_variants.push(p);
                }
            } else {
                let mut p = collector::gate_lead::GateLeadParams::from_env();
                p.backtest_mode = is_backtest;
                if is_backtest {
                    p.mode_tag = backtest_tag.clone();
                }
                gl_variants.push(p);
            }
            for gl_params in gl_variants {
                tracing::info!(
                    account_id = %gl_params.account_id,
                    min_move_bp = gl_params.min_move_bp,
                    whitelist_len = gl_params.whitelist.len(),
                    backtest = gl_params.backtest_mode,
                    mode_tag = %gl_params.mode_tag,
                    "gate_lead strategy enabled"
                );
                let writer_g = writer.clone();
                let rx_g = tx.subscribe();
                tokio::spawn(async move {
                    collector::gate_lead::run(writer_g, rx_g, gl_params).await;
                });
            }
        }

        // HL-lead strategy: gate_lead clone with Hyperliquid as the follower
        // leg (Binance still leader). Enable with HL_LEAD=1.
        if std::env::var("HL_LEAD").unwrap_or_else(|_| "0".into()) != "0" {
            let mut p = collector::hl_lead::GateLeadParams::from_env();
            p.backtest_mode = is_backtest;
            if is_backtest {
                p.mode_tag = backtest_tag.clone();
            }
            tracing::info!(
                account_id = %p.account_id,
                min_move_bp = p.min_move_bp,
                whitelist_len = p.whitelist.len(),
                backtest = p.backtest_mode,
                "hl_lead strategy enabled"
            );
            let writer_h = writer.clone();
            let rx_h = tx.subscribe();
            tokio::spawn(async move {
                collector::hl_lead::run(writer_h, rx_h, p).await;
            });
        }

        // MEXC-lead strategy: gate_lead clone with MEXC as the follower
        // leg (Binance still leader, same lead-lag detection logic). Enable
        // with MEXC_LEAD=1. In backtest replay, this also flips the follower
        // table to mexc_bookticker (see replay_merged caller above).
        //
        // BACKTEST_MEXC_SWEEP=1 spawns 4 variants over min_move_bp ∈
        // {15,20,25,30}, mirroring the gate_lead sweep — fastest way to find
        // the bp threshold that survives MEXC's bid-ask + fees on alts.
        if std::env::var("MEXC_LEAD").unwrap_or_else(|_| "0".into()) != "0" {
            let mexc_sweep = is_backtest
                && std::env::var("BACKTEST_MEXC_SWEEP")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
            let mm_grid_m: Vec<f64> = if mexc_sweep {
                vec![15.0, 20.0, 25.0, 30.0]
            } else {
                vec![]
            };
            let mut m_variants: Vec<collector::mexc_lead::GateLeadParams> = Vec::new();
            if mexc_sweep {
                for mm in &mm_grid_m {
                    let mut p = collector::mexc_lead::GateLeadParams::from_env();
                    p.backtest_mode = true;
                    p.min_move_bp = *mm;
                    p.account_id = format!("ML_BT_mm{}", *mm as i32);
                    p.mode_tag = backtest_tag.clone();
                    p.whitelist = Vec::new();
                    m_variants.push(p);
                }
            } else {
                let mut p = collector::mexc_lead::GateLeadParams::from_env();
                p.backtest_mode = is_backtest;
                if is_backtest {
                    p.mode_tag = backtest_tag.clone();
                }
                m_variants.push(p);
            }
            for ml_params in m_variants {
                tracing::info!(
                    account_id = %ml_params.account_id,
                    min_move_bp = ml_params.min_move_bp,
                    whitelist_len = ml_params.whitelist.len(),
                    backtest = ml_params.backtest_mode,
                    mode_tag = %ml_params.mode_tag,
                    "mexc_lead strategy enabled"
                );
                let writer_m = writer.clone();
                let rx_m = tx.subscribe();
                tokio::spawn(async move {
                    collector::mexc_lead::run(writer_m, rx_m, ml_params).await;
                });
            }
        }

        // BingX-lead strategy: gate_lead clone with BingX as the follower
        // leg (Binance still leader). Same logic as mexc_lead, different
        // follower exchange. Enable with BINGX_LEAD=1.
        // BACKTEST_BINGX_SWEEP=1 spawns 4 variants over min_move_bp ∈
        // {15,20,25,30} for the same gate_lead-style sweep.
        if std::env::var("BINGX_LEAD").unwrap_or_else(|_| "0".into()) != "0" {
            let bingx_sweep = is_backtest
                && std::env::var("BACKTEST_BINGX_SWEEP")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
            // BACKTEST_BINGX_GRID=1 sweeps anchor_s × exit_bp at fixed mm30.
            // 9 variants in parallel: anchor ∈ {2,3,5}, exit ∈ {3,5,7}.
            let bingx_grid = is_backtest
                && std::env::var("BACKTEST_BINGX_GRID")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
            let mm_grid_b: Vec<f64> = if bingx_sweep {
                vec![15.0, 20.0, 25.0, 30.0]
            } else {
                vec![]
            };
            let mut b_variants: Vec<collector::bingx_lead::GateLeadParams> = Vec::new();
            if bingx_grid {
                for a in [2.0_f64, 3.0, 5.0] {
                    for e in [3.0_f64, 5.0, 7.0] {
                        let mut p = collector::bingx_lead::GateLeadParams::from_env();
                        p.backtest_mode = true;
                        p.min_move_bp = 30.0;
                        p.anchor_s = a;
                        p.exit_bp = e;
                        p.account_id = format!("BX_BT_a{}e{}", a as i32, e as i32);
                        p.mode_tag = backtest_tag.clone();
                        p.whitelist = Vec::new();
                        b_variants.push(p);
                    }
                }
            } else if bingx_sweep {
                for mm in &mm_grid_b {
                    let mut p = collector::bingx_lead::GateLeadParams::from_env();
                    p.backtest_mode = true;
                    p.min_move_bp = *mm;
                    p.account_id = format!("BX_BT_mm{}", *mm as i32);
                    p.mode_tag = backtest_tag.clone();
                    p.whitelist = Vec::new();
                    b_variants.push(p);
                }
            } else {
                let mut p = collector::bingx_lead::GateLeadParams::from_env();
                p.backtest_mode = is_backtest;
                if is_backtest {
                    p.mode_tag = backtest_tag.clone();
                }
                b_variants.push(p);
            }
            for bx_params in b_variants {
                tracing::info!(
                    account_id = %bx_params.account_id,
                    min_move_bp = bx_params.min_move_bp,
                    whitelist_len = bx_params.whitelist.len(),
                    backtest = bx_params.backtest_mode,
                    mode_tag = %bx_params.mode_tag,
                    "bingx_lead strategy enabled"
                );
                let writer_b = writer.clone();
                let rx_b = tx.subscribe();
                // Live mode: feed executor → collector fills back into the
                // strategy so ref_mid is the actual maker fill price, not
                // the signal-time mid. Backtest variants (multiple parallel
                // params) intentionally don't share — the fill_events_tx is
                // single-source and routing per-account is handled inside
                // bingx_lead's filter on `f.account_id == account_id`.
                let fill_rx_b = if bx_params.backtest_mode {
                    None
                } else {
                    Some(fill_events_tx.subscribe())
                };
                tokio::spawn(async move {
                    collector::bingx_lead::run(writer_b, rx_b, fill_rx_b, bx_params).await;
                });
            }
        }

        // LIGHTER_LEAD=1 spawns a parallel bingx_lead variant whose follower
        // venue is Lighter instead of BingX. Reuses the same Binance leader,
        // same lead-lag detection, but follower=Lighter (175 majors). All
        // tunable params via GL_LH_* envs (separate from the BingX variant
        // so both can run side-by-side). Lighter has 0/0 fees so smaller
        // alpha thresholds (lower min_move_bp + exit_bp) are viable here.
        if std::env::var("LIGHTER_LEAD").unwrap_or_else(|_| "0".into()) != "0" {
            let mut lh = collector::bingx_lead::GateLeadParams::default();
            lh.account_id = std::env::var("GL_LH_ACCOUNT_ID")
                .unwrap_or_else(|_| "LH_PAPER_v1".into());
            lh.size_usd = std::env::var("GL_LH_SIZE_USD")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(20.0);
            // Lighter universe = majors. Lower thresholds because:
            //   - 30bp Binance moves on majors are rare; 10bp common.
            //   - 0/0 fees → small mean-revert alpha is profitable.
            lh.min_move_bp = std::env::var("GL_LH_MIN_MOVE_BP")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(10.0);
            lh.exit_bp = std::env::var("GL_LH_EXIT_BP")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(3.0);
            lh.stop_bp = std::env::var("GL_LH_STOP_BP")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(5.0);
            lh.anchor_s = std::env::var("GL_LH_ANCHOR_S")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(2.0);
            lh.hold_max_s = std::env::var("GL_LH_HOLD_MAX_S")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(2.0);
            lh.cooldown_s = std::env::var("GL_LH_COOLDOWN_S")
                .ok().and_then(|v| v.parse().ok()).unwrap_or(3.0);
            lh.fee_bp_per_side = 0.0;  // Lighter Standard = 0/0
            lh.follower_venue = ExchangeName::Lighter;
            // Empty whitelist = trade every base symbol. The strategy's
            // base_of() handles Lighter (bare base, no quote suffix).
            lh.whitelist = std::env::var("GL_LH_WHITELIST")
                .ok()
                .map(|v| v.split(',').filter(|s| !s.is_empty()).map(|s| s.trim().to_uppercase()).collect())
                .unwrap_or_default();
            lh.blacklist = std::env::var("GL_LH_BLACKLIST")
                .ok()
                .map(|v| v.split(',').filter(|s| !s.is_empty()).map(|s| s.trim().to_uppercase()).collect())
                .unwrap_or_default();
            lh.mode_tag = std::env::var("GL_LH_MODE_TAG")
                .unwrap_or_else(|_| "lighter_paper".into());
            lh.backtest_mode = is_backtest;
            tracing::info!(
                account_id = %lh.account_id,
                follower = "lighter",
                min_move_bp = lh.min_move_bp,
                exit_bp = lh.exit_bp,
                whitelist_len = lh.whitelist.len(),
                "LIGHTER_LEAD enabled"
            );
            let writer_l = writer.clone();
            let rx_l = tx.subscribe();
            let fill_rx_l = if lh.backtest_mode { None } else { Some(fill_events_tx.subscribe()) };
            tokio::spawn(async move {
                collector::bingx_lead::run(writer_l, rx_l, fill_rx_l, lh).await;
            });
        }

        // Spread-reversion strategy (Binance↔Flipster mid-gap mean revert).
        // Off by default — enable via SPREAD_REVERT=1. Independent of
        // gate_lead; uses strategy='spread_revert' tag in position_log.
        if std::env::var("SPREAD_REVERT").unwrap_or_else(|_| "0".into()) != "0" {
            let writer_sr = writer.clone();
            let rx_sr = tx.subscribe();
            let mut sr_params = collector::spread_revert::SpreadRevertParams::from_env();
            sr_params.backtest_mode = is_backtest;
            tracing::info!(
                account_id = %sr_params.account_id,
                backtest = sr_params.backtest_mode,
                "spread_revert strategy enabled"
            );
            // Spin up the baseline aggregator first (skipped in
            // backtest mode — replays don't query live-time tables for
            // baselines). spread_revert holds the same handle and
            // seeds its rolling window from it on first tick.
            let baselines: collector::baseline_writer::SharedBaselines = if !is_backtest {
                collector::baseline_writer::spawn()
            } else {
                std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()))
            };
            tokio::spawn(async move {
                collector::spread_revert::run(writer_sr, rx_sr, sr_params, baselines).await;
            });
        }

        // HL clone of spread_revert: BIN↔HL gap mean reversion.
        // Enable via HL_SPREAD_REVERT=1.
        if std::env::var("HL_SPREAD_REVERT").unwrap_or_else(|_| "0".into()) != "0" {
            let writer_h = writer.clone();
            let rx_h = tx.subscribe();
            let mut h_params = collector::hl_spread_revert::SpreadRevertParams::from_env();
            h_params.backtest_mode = is_backtest;
            tracing::info!(
                account_id = %h_params.account_id,
                backtest = h_params.backtest_mode,
                "hl_spread_revert strategy enabled"
            );
            // Always spawn baseline_writer — even in backtest. Live 30-min
            // window approximates the historical baseline closely enough
            // for our HL dataset (backtest range fits in available history).
            let baselines = collector::hl_baseline_writer::spawn();
            // HSR_LIVE=1 + HLP_AGENT_KEY → real IOC orders on HL.
            let live_exec = if std::env::var("HSR_LIVE").unwrap_or_default() == "1" {
                match hl_pairs::exec::LiveCfg::from_env() {
                    Some(cfg) => match hl_pairs::exec::LiveExec::new(cfg.clone()).await {
                        Ok(e) => {
                            tracing::warn!(
                                agent = %hl_pairs::exec::agent_address(&cfg.agent_key).unwrap_or_default(),
                                max_size_usd = cfg.max_size_usd,
                                daily_loss_cap_usd = cfg.daily_loss_cap_usd,
                                testnet = cfg.testnet,
                                "HSR_LIVE=1 — hl_spread_revert sending real HL orders"
                            );
                            // Set max leverage on top performers (cross).
                            let coins = ["DASH","CHILLGUY","MEGA","VVV","STBL","PENDLE","ZEC","MERL","HYPE","ENA","DYDX","INJ","STX","ORDI","BABY","USTC","AR","VIRTUAL","CHIP","SPX"];
                            let n_ok = e.set_max_leverage_for(&coins).await;
                            tracing::warn!("set max leverage on {}/{} coins (cross)", n_ok, coins.len());
                            Some(std::sync::Arc::new(e))
                        }
                        Err(err) => { tracing::warn!(error = %err, "LiveExec init failed — paper-only"); None }
                    },
                    None => { tracing::warn!("HSR_LIVE=1 but HLP_AGENT_KEY missing — paper-only"); None }
                }
            } else { None };

            // Live state cache: master positions + open orders refreshed every 5s.
            // Strategy uses this to enforce "1 position per coin at master level".
            let live_state = if live_exec.is_some() {
                let master = std::env::var("HLP_MASTER_ADDRESS").unwrap_or_default();
                let testnet = std::env::var("HLP_TESTNET").map(|v| v=="1").unwrap_or(false);
                if !master.is_empty() {
                    Some(hl_pairs::live_state::spawn(master, testnet, 5))
                } else {
                    tracing::warn!("HLP_MASTER_ADDRESS missing — live_state guard disabled");
                    None
                }
            } else { None };
            tokio::spawn(async move {
                collector::hl_spread_revert::run(writer_h, rx_h, h_params, baselines, live_exec, live_state).await;
            });
        }

        // If BACKTEST_START_TS + BACKTEST_END_TS are set, run historical
        // replay through the same broadcast channel, wait for it to finish,
        // flush the ILP writer, emit a summary, and exit. Falls through to
        // the live path when either var is missing.
        if let (Some(start_ts), Some(end_ts)) = (backtest_start, backtest_end) {
            let questdb_http = std::env::var("QUESTDB_HTTP_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:9000".into());
            let cfg = collector::questdb_reader::ReaderConfig {
                http_url: questdb_http.clone(),
                poll_interval_ms: 0, // unused in replay
                start_lookback_secs: 0,
                batch_limit: 50_000,
            };
            tracing::info!(
                start = %start_ts,
                end = %end_ts,
                hedge = hedge_table,
                "backtest replay: running"
            );
            // Follower table picks the strategy under test:
            //   MEXC_LEAD=1 → mexc_bookticker (mexc_lead reads from this)
            //   HL_LEAD=1   → hyperliquid_bookticker (hl_lead reads)
            //   default     → flipster_bookticker (gate_lead / pairs / spread_revert)
            let mexc_lead_on = std::env::var("MEXC_LEAD").unwrap_or_else(|_| "0".into()) != "0";
            let hl_lead_on = std::env::var("HL_LEAD").unwrap_or_else(|_| "0".into()) != "0";
            let bingx_lead_on = std::env::var("BINGX_LEAD").unwrap_or_else(|_| "0".into()) != "0";
            let (follower_exchange, follower_table) = if mexc_lead_on {
                (ExchangeName::Mexc, "mexc_bookticker")
            } else if bingx_lead_on {
                (ExchangeName::Bingx, "bingx_bookticker")
            } else if hl_lead_on {
                (ExchangeName::Hyperliquid, "hyperliquid_bookticker")
            } else {
                (ExchangeName::Flipster, "flipster_bookticker")
            };
            tracing::info!(
                follower = follower_exchange.as_str(),
                follower_table,
                "backtest replay: follower selected"
            );
            let emitted = collector::questdb_reader::replay_merged(
                cfg,
                start_ts,
                end_ts,
                base.hedge_venue,
                hedge_table,
                follower_exchange,
                follower_table,
                tx.clone(),
            )
            .await?;
            // Let the variant tasks drain the remaining backlog + sweep any
            // open positions to the timeout via a couple of real-time ticks.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let _ = writer.flush().await;
            tracing::info!(emitted, "backtest replay: flushed, printing summary");

            // Summary via QuestDB HTTP.
            let start_iso = start_ts.to_rfc3339();
            let end_iso = end_ts.to_rfc3339();
            let sql = format!(
                "SELECT strategy, account_id, count() n, round(avg(pnl_bp),2) avg_bp, \
                 round(sum(pnl_bp*size/10000),2) sum_usd, \
                 round(sum(case when pnl_bp>0 then 1.0 else 0.0 end)/count()*100,1) win \
                 FROM position_log WHERE mode='{backtest_tag}' \
                 AND timestamp > '{start_iso}' AND timestamp <= '{end_iso}' \
                 GROUP BY strategy, account_id ORDER BY sum_usd DESC"
            );
            let client = reqwest::Client::new();
            let url = format!("{}/exec", questdb_http.trim_end_matches('/'));
            match client.get(&url).query(&[("query", sql)]).send().await {
                Ok(resp) => {
                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                        if let Some(ds) = v.get("dataset").and_then(|x| x.as_array()) {
                            println!("\n=== BACKTEST SUMMARY ({} → {}) ===", start_iso, end_iso);
                            println!(
                                "{:<14} {:<24} {:>7} {:>9} {:>11} {:>7}",
                                "strategy", "variant", "n", "avg_bp", "sum_$", "win%"
                            );
                            for row in ds {
                                if let Some(a) = row.as_array() {
                                    let strat = a.first().and_then(|x| x.as_str()).unwrap_or("?");
                                    let variant = a.get(1).and_then(|x| x.as_str()).unwrap_or("?");
                                    let n = a.get(2).and_then(|x| x.as_i64()).unwrap_or(0);
                                    let avg_bp = a.get(3).and_then(|x| x.as_f64()).unwrap_or(0.0);
                                    let sum_usd = a.get(4).and_then(|x| x.as_f64()).unwrap_or(0.0);
                                    let win = a.get(5).and_then(|x| x.as_f64()).unwrap_or(0.0);
                                    println!(
                                        "{:<14} {:<24} {:>7} {:>+9.2} {:>+11.2} {:>6.1}",
                                        strat, variant, n, avg_bp, sum_usd, win
                                    );
                                }
                            }
                            println!();
                        }
                    }
                }
                Err(e) => tracing::error!(error = %e, "backtest summary query failed"),
            }
            return Ok(());
        }
    }

    // USE_ZMQ=1 short-circuits both WS subscribers and the QuestDB poller:
    // collector connects directly to each exchange's data_publisher ZMQ
    // PUB socket and re-emits parsed bookticker frames into the same
    // broadcast channel. <10ms tick latency.
    //
    // No external subscriber/IPC bridge — the ZMQ SUB lives inside the
    // collector. Override endpoints via env: ZMQ_FLIPSTER, ZMQ_BINANCE,
    // ZMQ_GATE.
    if std::env::var("USE_ZMQ")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        let flipster_addr =
            std::env::var("ZMQ_FLIPSTER").unwrap_or_else(|_| "tcp://211.181.122.104:7000".into());
        let binance_addr =
            std::env::var("ZMQ_BINANCE").unwrap_or_else(|_| "tcp://211.181.122.24:6000".into());
        let gate_addr =
            std::env::var("ZMQ_GATE").unwrap_or_else(|_| "tcp://211.181.122.24:5559".into());
        collector::zmq_reader::spawn(flipster_addr, ExchangeName::Flipster, tx.clone());
        collector::zmq_reader::spawn(binance_addr, ExchangeName::Binance, tx.clone());
        collector::zmq_reader::spawn(gate_addr, ExchangeName::Gate, tx.clone());
        // Hyperliquid has no ZMQ publisher yet — fall back to QuestDB polling
        // for the hyperliquid_bookticker table (populated by hyperliquid_publisher).
        // Required for the hl_lead strategy to see HL ticks.
        {
            let qdb_http = std::env::var("QUESTDB_HTTP_URL")
                .unwrap_or_else(|_| "http://211.181.122.102:9000".into());
            let cfg = collector::questdb_reader::ReaderConfig {
                http_url: qdb_http,
                poll_interval_ms: 200,
                ..Default::default()
            };
            collector::questdb_reader::spawn(
                ExchangeName::Hyperliquid, "hyperliquid_bookticker", cfg, tx.clone(),
            );
            tracing::info!("USE_ZMQ=1 — added QDB polling for hyperliquid_bookticker (hl_lead leg)");
        }
        tracing::info!("USE_ZMQ=1 — direct ZMQ SUB feeds active (flipster/binance/gate)");
        let flush_writer = writer.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                let _ = flush_writer.flush().await;
            }
        });
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    }

    // READ_FROM_QUESTDB=1 short-circuits all WS subscribers and instead
    // streams bookticker rows out of a shared QuestDB (e.g. a central feed
    // host). Paper bot + latency engine subscribe to the same broadcast
    // channel so they don't know the difference.
    if std::env::var("READ_FROM_QUESTDB")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        let http_url =
            std::env::var("QUESTDB_HTTP_URL").unwrap_or_else(|_| "http://127.0.0.1:9000".into());
        let poll_ms: u64 = std::env::var("QUESTDB_READ_POLL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        let cfg = collector::questdb_reader::ReaderConfig {
            http_url,
            poll_interval_ms: poll_ms,
            ..Default::default()
        };
        // BINANCE_WS_DIRECT=1 → subscribe to Binance USDⓈ-M perpetual WS
        // directly (combined-stream `bookTicker`) instead of polling the
        // central QuestDB. Saves the data_publisher → QDB write delay
        // (typically 50-200ms), which is the single biggest latency lever
        // for any strategy that fires on Binance shocks (gate_lead /
        // bingx_lead / mexc_lead). Other exchanges still come via QDB
        // poll. Symbols come from BINANCE_WS_SYMBOLS (comma list, e.g.
        // "LABUSDT,RAVEUSDT,..."); empty = full perpetual universe.
        let binance_ws_direct = std::env::var("BINANCE_WS_DIRECT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        // BINGX_WS_DIRECT=1 → subscribe to BingX swap public bookTicker WS
        // directly. Killing the central QDB poll on BingX is the second
        // half of the latency budget for bingx_lead — without it, the
        // strategy compares fresh Binance (WS) against a 100ms-stale
        // BingX feed, which the lag/velocity filters interpret as
        // "BingX already caught up" and skips most signals.
        let bingx_ws_direct = std::env::var("BINGX_WS_DIRECT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let mut tables: Vec<(ExchangeName, &'static str)> = vec![
            (ExchangeName::Flipster, "flipster_bookticker"),
            (ExchangeName::Gate, "gate_bookticker"),
            (ExchangeName::Hyperliquid, "hyperliquid_bookticker"),
            (ExchangeName::Mexc, "mexc_bookticker"),
        ];
        if !binance_ws_direct {
            tables.push((ExchangeName::Binance, "binance_bookticker"));
        }
        if !bingx_ws_direct {
            tables.push((ExchangeName::Bingx, "bingx_bookticker"));
        }
        for (exchange, table) in tables {
            collector::questdb_reader::spawn(exchange, table, cfg.clone(), tx.clone());
        }
        if binance_ws_direct {
            // Pick symbol set. Prefer explicit BINANCE_WS_SYMBOLS; fall
            // back to fetch_usdt_perps (full universe). Each WS connection
            // is capped at 10 symbols to avoid Binance's stream-limit RST.
            let env_syms = std::env::var("BINANCE_WS_SYMBOLS").ok();
            let syms: Vec<String> = if let Some(raw) = env_syms.as_deref() {
                raw.split(',')
                    .map(|s| s.trim().to_uppercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            } else {
                let set = collector::exchanges::binance::fetch_usdt_perps()
                    .await
                    .unwrap_or_default();
                let mut v: Vec<String> = set.into_iter().collect();
                v.sort();
                v
            };
            tracing::info!(
                count = syms.len(),
                "BINANCE_WS_DIRECT=1 — subscribing to Binance bookTicker WS instead of QDB poll"
            );
            for chunk in syms.chunks(10) {
                let chunk: Vec<String> = chunk.to_vec();
                let writer_b = writer.clone();
                let tx_b = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = collector::exchanges::run_collector(
                        collector::exchanges::binance::BinanceCollector::new(chunk.clone()),
                        chunk,
                        writer_b,
                        tx_b,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "binance WS chunk exited");
                    }
                });
            }
        }
        if bingx_ws_direct {
            // BingX symbol shape is "BTC-USDT" (dash). Pull from
            // BINGX_WS_SYMBOLS or fall back to the Binance whitelist
            // converted to BingX form (replace "USDT" with "-USDT").
            let env_syms = std::env::var("BINGX_WS_SYMBOLS").ok();
            let bin_syms = std::env::var("BINANCE_WS_SYMBOLS").ok();
            let syms: Vec<String> = if let Some(raw) = env_syms.as_deref() {
                raw.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            } else if let Some(raw) = bin_syms.as_deref() {
                // Convert "LABUSDT" → "LAB-USDT" using the trailing USDT.
                raw.split(',')
                    .filter_map(|s| {
                        let s = s.trim();
                        s.strip_suffix("USDT").map(|base| format!("{base}-USDT"))
                    })
                    .filter(|s| !s.is_empty())
                    .collect()
            } else {
                Vec::new()
            };
            tracing::info!(
                count = syms.len(),
                "BINGX_WS_DIRECT=1 — subscribing to BingX bookTicker WS instead of QDB poll"
            );
            // BingX WS limit appears generous; safe chunking at 50 symbols.
            for chunk in syms.chunks(50) {
                let chunk: Vec<String> = chunk.to_vec();
                let writer_b = writer.clone();
                let tx_b = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = collector::exchanges::run_collector(
                        collector::exchanges::bingx::BingxCollector::new(chunk.clone()),
                        chunk,
                        writer_b,
                        tx_b,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "bingx WS chunk exited");
                    }
                });
            }
        }
        // PANCAKESWAP perp + ASTERDEX collectors. Both are Binance API
        // forks. AsterDex serves a real bookTicker firehose (proper BBO);
        // PancakeSwap only has a markPriceTicker arr (last/c only, no
        // BBO). We spawn one collector each — all-symbol streams, no
        // chunking needed. Enable with PANCAKE_WS_DIRECT=1 / ASTER_WS_DIRECT=1.
        let pancake_on = std::env::var("PANCAKE_WS_DIRECT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if pancake_on {
            tracing::info!("PANCAKE_WS_DIRECT=1 — subscribing to PancakeSwap perp markPriceTicker");
            let writer_p = writer.clone();
            let tx_p = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = collector::exchanges::run_collector(
                    collector::exchanges::pancake::PancakeCollector::new(Vec::new()),
                    Vec::new(),
                    writer_p,
                    tx_p,
                )
                .await
                {
                    tracing::warn!(error = %e, "pancake WS exited");
                }
            });
        }
        let aster_on = std::env::var("ASTER_WS_DIRECT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if aster_on {
            tracing::info!("ASTER_WS_DIRECT=1 — subscribing to AsterDex !bookTicker");
            let writer_a = writer.clone();
            let tx_a = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = collector::exchanges::run_collector(
                    collector::exchanges::aster::AsterCollector::new(Vec::new()),
                    Vec::new(),
                    writer_a,
                    tx_a,
                )
                .await
                {
                    tracing::warn!(error = %e, "aster WS exited");
                }
            });
        }
        let lighter_on = std::env::var("LIGHTER_WS_DIRECT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if lighter_on {
            tracing::info!("LIGHTER_WS_DIRECT=1 — subscribing to Lighter market_stats");
            let writer_l = writer.clone();
            let tx_l = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = collector::exchanges::run_collector(
                    collector::exchanges::lighter::LighterCollector::new(Vec::new()),
                    Vec::new(),
                    writer_l,
                    tx_l,
                )
                .await
                {
                    tracing::warn!(error = %e, "lighter WS exited");
                }
            });
        }
        // VARIATIONAL — subscribe with reverse-engineered protocol from the
        // 2026-05-06 JS bundle capture. Universe is curated (~35 majors)
        // because the WS doesn't expose a "give me all" mode and discovery
        // would require crawling the REST `/instruments` endpoint. Override
        // with VARIATIONAL_UNDERLYINGS=BTC,ETH,...
        let variational_on = std::env::var("VARIATIONAL_WS_DIRECT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if variational_on {
            let env_uls = std::env::var("VARIATIONAL_UNDERLYINGS").ok();
            let underlyings: Vec<String> = if let Some(raw) = env_uls.as_deref() {
                raw.split(',')
                    .map(|s| s.trim().to_uppercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            } else {
                Vec::new()
            };
            tracing::info!(
                count = underlyings.len(),
                "VARIATIONAL_WS_DIRECT=1 — subscribing to Variational /prices"
            );
            let writer_v = writer.clone();
            let tx_v = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = collector::exchanges::run_collector(
                    collector::exchanges::variational::VariationalCollector::new(underlyings),
                    Vec::new(),
                    writer_v,
                    tx_v,
                )
                .await
                {
                    tracing::warn!(error = %e, "variational WS exited");
                }
            });
        }
        tracing::info!(
            binance_direct = binance_ws_direct,
            pancake = pancake_on,
            aster = aster_on,
            lighter = lighter_on,
            variational = variational_on,
            "READ_FROM_QUESTDB=1 — using QDB poll for non-Binance feeds"
        );
        // Periodic flush safety net still useful for any ILP writes
        let flush_writer = writer.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                let _ = flush_writer.flush().await;
            }
        });
        // Park forever — paper bot / latency engine run as their own tasks.
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    }

    // Binance symbols: when env is unset/empty/"auto" we auto-fetch the full
    // intersection with flipster's perp universe. Otherwise honor the env list.
    let binance_override = std::env::var("BINANCE_SYMBOLS")
        .ok()
        .filter(|s| !s.trim().is_empty() && s.trim() != "auto");
    let binance_syms: Vec<String> = if let Some(raw) = binance_override {
        raw.split(',').map(|s| s.trim().to_string()).collect()
    } else {
        // Need flipster REST too, so compute the intersection.
        let bin_set = collector::exchanges::binance::fetch_usdt_perps()
            .await
            .unwrap_or_default();
        let mut flip_bases: std::collections::HashSet<String> = Default::default();
        if let Some(flip) = FlipsterCollector::from_env() {
            if let Ok(list) = flip.fetch_perp_symbols().await {
                for s in list {
                    if let Some(stripped) = s.strip_suffix(".PERP") {
                        flip_bases.insert(stripped.to_string());
                    }
                }
            }
        }
        let mut intersection: Vec<String> = bin_set.intersection(&flip_bases).cloned().collect();
        intersection.sort();
        tracing::info!(
            binance_n = bin_set.len(),
            flipster_n = flip_bases.len(),
            intersection_n = intersection.len(),
            "binance × flipster USDT perp intersection"
        );
        intersection
    };
    // Flipster symbols: if FLIPSTER_SYMBOLS is unset/empty/"auto", fetch the
    // full perpetualSwap list from the REST API using the same HMAC creds.
    let flipster_syms_override = std::env::var("FLIPSTER_SYMBOLS")
        .ok()
        .filter(|s| !s.trim().is_empty() && s.trim() != "auto");
    let flipster_syms: Vec<String> = if let Some(raw) = flipster_syms_override {
        raw.split(',').map(|s| s.trim().to_string()).collect()
    } else if let Some(flip) = FlipsterCollector::from_env() {
        match flip.fetch_perp_symbols().await {
            Ok(v) => {
                tracing::info!(count = v.len(), "flipster: auto-fetched perp symbols");
                v
            }
            Err(e) => {
                tracing::warn!(error = %e, "flipster: symbol fetch failed — using fallback");
                vec![
                    "BTCUSDT.PERP".into(),
                    "ETHUSDT.PERP".into(),
                    "SOLUSDT.PERP".into(),
                    "XRPUSDT.PERP".into(),
                    "BNBUSDT.PERP".into(),
                    "DOGEUSDT.PERP".into(),
                    "TRXUSDT.PERP".into(),
                ]
            }
        }
    } else {
        Vec::new()
    };
    let default_usdt = &[
        "BTCUSDT", "ETHUSDT", "SOLUSDT", "XRPUSDT", "BNBUSDT", "DOGEUSDT", "TRXUSDT", "ADAUSDT",
        "PAXGUSDT",
    ];
    let default_gate = &[
        "BTC_USDT",
        "ETH_USDT",
        "SOL_USDT",
        "XRP_USDT",
        "BNB_USDT",
        "DOGE_USDT",
        "TRX_USDT",
        "ADA_USDT",
        "PAXG_USDT",
    ];
    let bybit_syms = symbols_env("BYBIT_SYMBOLS", default_usdt);
    let bitget_syms = symbols_env("BITGET_SYMBOLS", default_usdt);
    let gate_syms = symbols_env("GATE_SYMBOLS", default_gate);

    // Binance combined stream throughput limits mean 50+ bookTicker streams
    // on one WS get RST'd after ~15s. Split into chunks of 10 symbols, one
    // WS per chunk.
    let mut tasks: Vec<tokio::task::JoinHandle<anyhow::Result<()>>> = Vec::new();
    for chunk in binance_syms.chunks(10) {
        let chunk: Vec<String> = chunk.to_vec();
        tasks.push(tokio::spawn(run_collector(
            BinanceCollector::new(chunk.clone()),
            chunk,
            writer.clone(),
            tx.clone(),
        )));
    }
    tasks.push(tokio::spawn(run_collector(
        BybitCollector,
        bybit_syms,
        writer.clone(),
        tx.clone(),
    )));
    tasks.push(tokio::spawn(run_collector(
        BitgetCollector,
        bitget_syms,
        writer.clone(),
        tx.clone(),
    )));
    // Gate WS rejects very large per-connection subscription counts (~200+).
    // Split into 100-symbol chunks, one WS connection per chunk.
    for chunk in gate_syms.chunks(100) {
        let chunk: Vec<String> = chunk.to_vec();
        tasks.push(tokio::spawn(run_collector(
            GateCollector,
            chunk,
            writer.clone(),
            tx.clone(),
        )));
    }
    match FlipsterCollector::from_env() {
        Some(flip) => {
            tracing::info!("flipster collector enabled (credentials found)");
            tasks.push(tokio::spawn(run_collector(
                flip,
                flipster_syms,
                writer.clone(),
                tx.clone(),
            )));
        }
        None => {
            tracing::warn!(
                "flipster collector disabled — set FLIPSTER_API_KEY and FLIPSTER_API_SECRET to enable"
            );
            let _ = flipster_syms;
        }
    }

    // Periodic flush safety net in case of idle streams.
    let flush_writer = writer.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            let _ = flush_writer.flush().await;
        }
    });

    for t in tasks {
        let _ = t.await;
    }
    Ok(())
}

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
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let qdb_host = std::env::var("QDB_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let qdb_port: u16 = std::env::var("QDB_ILP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9009);
    let writer = IlpWriter::connect(&qdb_host, qdb_port, 512)?;

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
            _ => "gate_bookticker",
        };
        let questdb_url = std::env::var("QUESTDB_HTTP_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9000".into());
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
            v.push(mk("A_baseline",   2.5, 0.3, 0.3, 100.0, None));
            v.push(mk("B_minstd2",    2.5, 0.3, 2.0, 100.0, None));
            v.push(mk("C_minstd3",    2.5, 0.3, 3.0, 100.0, None));
            v.push(mk("D_high_sig",   4.0, 0.3, 0.3, 100.0, None));
            v.push(mk("E_stop6",      2.5, 0.3, 2.0,   6.0, None));
            // ---- Fixed-size companions of the winning variants ----
            // F mirrors B with $200 notional per trade (fair sizing control).
            // G mirrors C with $200 (tighter min_std + same fixed size).
            // H mirrors B with $1000 notional (capital scaling test — is the
            // edge robust at 5x the base size given slippage we're not
            // modeling yet?).
            v.push(mk("F_minstd2_fix200",  2.5, 0.3, 2.0, 100.0, Some(200.0)));
            v.push(mk("G_minstd3_fix200",  2.5, 0.3, 3.0, 100.0, Some(200.0)));
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
            v.push(mk_improved("J_core",       0.0,  0,  false, 60_000));
            // Swap EWMA for a 30s rolling window (#4).
            v.push(mk_improved("K_roll",      30.0,  0,  false, 60_000));
            // Add per-symbol auto-blacklist (#5).
            v.push(mk_improved("L_autobl",     0.0, 20,  false, 60_000));
            // Add asymmetric exit + longer max_hold so the signal can play out (#6).
            v.push(mk_improved("M_asymexit",   0.0,  0,  true,  300_000));
            // Everything on — rolling + auto-bl + asym exit + long hold.
            v.push(mk_improved("N_full",      30.0, 20,  true,  300_000));

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
                mk_sweep("S01_minstd1",  1.0, 2.5, true,  300_000),
                mk_sweep("S02_minstd2",  2.0, 2.5, true,  300_000),
                mk_sweep("S03_pivot",    3.0, 2.5, true,  300_000), // pivot
                mk_sweep("S04_minstd4",  4.0, 2.5, true,  300_000),
                // ---- entry_sigma sweep ----
                mk_sweep("S05_es20",     3.0, 2.0, true,  300_000),
                mk_sweep("S06_es30",     3.0, 3.0, true,  300_000),
                // ---- max_hold sweep ----
                mk_sweep("S07_hold60",   3.0, 2.5, true,   60_000),
                mk_sweep("S08_hold120",  3.0, 2.5, true,  120_000),
                mk_sweep("S09_hold600",  3.0, 2.5, true,  600_000),
                // ---- asym_exit ablation ----
                mk_sweep("S10_noasym60", 3.0, 2.5, false,  60_000),  // = original I
                mk_sweep("S11_noasym300",3.0, 2.5, false, 300_000),
                // ---- T-series: untested optimal combos ----
                mk_sweep("T01_best",      4.0, 3.0, true,  300_000), // S04 min_std + S06 entry_sigma
                mk_sweep("T02_best_long", 4.0, 3.0, true,  600_000), // T01 + longer hold
                mk_sweep("T03_minstd5",   5.0, 3.0, true,  300_000), // push min_std further
                mk_sweep("T04_es35",      4.0, 3.5, true,  300_000), // push entry_sigma further
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
            let emitted = collector::questdb_reader::replay_merged(
                cfg,
                start_ts,
                end_ts,
                base.hedge_venue,
                hedge_table,
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
                "SELECT account_id, count() n, round(avg(pnl_bp),2) avg_bp, \
                 round(sum(pnl_bp*size/10000),2) sum_usd, \
                 round(sum(case when pnl_bp>0 then 1.0 else 0.0 end)/count()*100,1) win \
                 FROM position_log WHERE strategy='pairs' AND mode='{backtest_tag}' \
                 AND timestamp > '{start_iso}' AND timestamp <= '{end_iso}' \
                 GROUP BY account_id ORDER BY sum_usd DESC"
            );
            let client = reqwest::Client::new();
            let url = format!("{}/exec", questdb_http.trim_end_matches('/'));
            match client.get(&url).query(&[("query", sql)]).send().await {
                Ok(resp) => {
                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                        if let Some(ds) = v.get("dataset").and_then(|x| x.as_array()) {
                            println!("\n=== BACKTEST SUMMARY ({} → {}) ===", start_iso, end_iso);
                            println!("{:<28} {:>7} {:>9} {:>11} {:>7}",
                                     "variant", "n", "avg_bp", "sum_$", "win%");
                            for row in ds {
                                if let Some(a) = row.as_array() {
                                    let variant = a.first().and_then(|x| x.as_str()).unwrap_or("?");
                                    let n = a.get(1).and_then(|x| x.as_i64()).unwrap_or(0);
                                    let avg_bp = a.get(2).and_then(|x| x.as_f64()).unwrap_or(0.0);
                                    let sum_usd = a.get(3).and_then(|x| x.as_f64()).unwrap_or(0.0);
                                    let win = a.get(4).and_then(|x| x.as_f64()).unwrap_or(0.0);
                                    println!("{:<28} {:>7} {:>+9.2} {:>+11.2} {:>6.1}",
                                             variant, n, avg_bp, sum_usd, win);
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
    // we connect directly to the upstream data_publisher's ZMQ PUB sockets
    // and re-emit parsed bookticker frames into the same broadcast channel.
    // This eliminates the ~100ms average poll lag the questdb_reader has,
    // bringing live tick latency down to <10ms (sub-tick of the strategy
    // sweep interval). Endpoints default to the gate1 deployment but each
    // can be overridden via env: ZMQ_FLIPSTER, ZMQ_BINANCE, ZMQ_GATE.
    if std::env::var("USE_ZMQ")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        let flipster_addr = std::env::var("ZMQ_FLIPSTER")
            .unwrap_or_else(|_| "tcp://211.181.122.104:7000".into());
        let binance_addr = std::env::var("ZMQ_BINANCE")
            .unwrap_or_else(|_| "tcp://211.181.122.24:6000".into());
        let gate_addr = std::env::var("ZMQ_GATE")
            .unwrap_or_else(|_| "tcp://211.181.122.24:5559".into());
        collector::zmq_reader::spawn(flipster_addr, ExchangeName::Flipster, tx.clone());
        collector::zmq_reader::spawn(binance_addr, ExchangeName::Binance, tx.clone());
        collector::zmq_reader::spawn(gate_addr, ExchangeName::Gate, tx.clone());
        tracing::info!("USE_ZMQ=1 — direct ZMQ SUB feeds active, skipping WS + QuestDB poller");
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
        let http_url = std::env::var("QUESTDB_HTTP_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:9000".into());
        let poll_ms: u64 = std::env::var("QUESTDB_READ_POLL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        let cfg = collector::questdb_reader::ReaderConfig {
            http_url,
            poll_interval_ms: poll_ms,
            ..Default::default()
        };
        // Spawn one reader per table. Match the default set to what the paper
        // bot actually consumes: flipster + gate + binance (pairs strategy
        // and latency signal inputs).
        for (exchange, table) in [
            (ExchangeName::Flipster, "flipster_bookticker"),
            (ExchangeName::Gate,     "gate_bookticker"),
            (ExchangeName::Binance,  "binance_bookticker"),
        ] {
            collector::questdb_reader::spawn(exchange, table, cfg.clone(), tx.clone());
        }
        tracing::info!("READ_FROM_QUESTDB=1 — skipping all WS subscribers");
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
        let mut intersection: Vec<String> =
            bin_set.intersection(&flip_bases).cloned().collect();
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
        "BTCUSDT", "ETHUSDT", "SOLUSDT", "XRPUSDT", "BNBUSDT", "DOGEUSDT", "TRXUSDT",
        "ADAUSDT", "PAXGUSDT",
    ];
    let default_gate = &[
        "BTC_USDT", "ETH_USDT", "SOL_USDT", "XRP_USDT", "BNB_USDT", "DOGE_USDT", "TRX_USDT",
        "ADA_USDT", "PAXG_USDT",
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
    tasks.push(tokio::spawn(run_collector(BybitCollector, bybit_syms, writer.clone(), tx.clone())));
    tasks.push(tokio::spawn(run_collector(BitgetCollector, bitget_syms, writer.clone(), tx.clone())));
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

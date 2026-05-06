//! HL ↔ Binance pairs paper bot. Polls central QuestDB, runs 8 variants,
//! writes closed trades to position_log.

use anyhow::Result;
use hl_pairs::config::{variants, Globals};
use hl_pairs::exec::{agent_address, LiveCfg, LiveExec};
use hl_pairs::ilp::Writer;
use hl_pairs::reader::Reader;
use hl_pairs::strategy::Variant;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hl_pairs=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let g = Globals::from_env();
    info!("hl_pairs paper starting; globals={g:?}");

    // Live mode: HLP_LIVE_VARIANTS=hlp_T01_best (comma-separated). Empty/unset = paper-only.
    let live_set: HashSet<String> = std::env::var("HLP_LIVE_VARIANTS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let live_exec: Option<Arc<LiveExec>> = if !live_set.is_empty() {
        match LiveCfg::from_env() {
            Some(cfg) => {
                info!("LIVE mode: agent={} variants={:?} max_size=${} daily_cap=${} testnet={}",
                    agent_address(&cfg.agent_key)?, live_set, cfg.max_size_usd, cfg.daily_loss_cap_usd, cfg.testnet);
                Some(Arc::new(LiveExec::new(cfg).await?))
            }
            None => { warn!("HLP_LIVE_VARIANTS set but HLP_AGENT_KEY missing — staying paper-only"); None }
        }
    } else {
        info!("paper-only mode (HLP_LIVE_VARIANTS unset)");
        None
    };

    let writer = Writer::spawn(g.qdb_ilp_host.clone(), g.qdb_ilp_port)?;
    let mut reader = Reader::new(g.qdb_http_url.clone());
    let mut variants: Vec<Variant> = variants()
        .into_iter()
        .map(|c| {
            let mut v = Variant::new(c.clone(), g.clone(), writer.clone());
            if let Some(le) = &live_exec {
                if live_set.contains(c.id) {
                    v = v.with_live(le.clone());
                }
            }
            v
        })
        .collect();
    info!("spawned {} variants ({} LIVE): {:?}", variants.len(),
        variants.iter().filter(|v| v.is_live()).count(),
        variants.iter().map(|v| if v.is_live() { format!("{}*", v.cfg.id) } else { v.cfg.id.to_string() }).collect::<Vec<_>>());

    let interval = Duration::from_millis(g.poll_interval_ms);
    let mut last_summary = Instant::now();
    loop {
        tokio::time::sleep(interval).await;
        let ticks = match reader.poll(g.max_age_ms).await {
            Ok(t) => t,
            Err(e) => { warn!("poll err: {e}"); continue; }
        };
        if ticks.is_empty() { continue; }
        for v in variants.iter_mut() {
            v.on_tick_batch(&ticks);
        }
        if last_summary.elapsed() >= Duration::from_secs(15) {
            info!("polled {} live pairs", ticks.len());
            for v in &variants { v.log_summary(); }
            last_summary = Instant::now();
        }
    }
}

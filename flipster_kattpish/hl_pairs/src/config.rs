//! Per-variant config + global tunables loaded from env.

use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct Globals {
    pub qdb_http_url: String,
    pub qdb_ilp_host: String,
    pub qdb_ilp_port: u16,
    pub poll_interval_ms: u64,
    /// Round-trip fee in bp. HL: 4.5bp taker × 2 = 9bp.
    pub fee_bp: f64,
    /// Tau for spread EWMA (seconds).
    pub ewma_tau_sec: f64,
    /// Drop pairs whose latest tick is older than this.
    pub max_age_ms: i64,
    /// Per-trade USD notional (paper).
    pub size_usd: f64,
    /// Symbols to skip (comma-separated env: HLP_BLACKLIST).
    pub blacklist: HashSet<String>,
}

impl Globals {
    pub fn from_env() -> Self {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let qdb_host = env("QDB_HOST", "211.181.122.102");
        Self {
            qdb_http_url: env("QUESTDB_HTTP_URL", &format!("http://{qdb_host}:9000")),
            qdb_ilp_host: qdb_host.clone(),
            qdb_ilp_port: env("QDB_ILP_PORT", "9009").parse().unwrap_or(9009),
            poll_interval_ms: env("HLP_POLL_MS", "500").parse().unwrap_or(500),
            fee_bp: env("HLP_FEE_BP", "9.0").parse().unwrap_or(9.0),
            ewma_tau_sec: env("HLP_EWMA_TAU_SEC", "60.0").parse().unwrap_or(60.0),
            max_age_ms: env("HLP_MAX_AGE_MS", "5000").parse().unwrap_or(5000),
            size_usd: env("HLP_SIZE_USD", "200.0").parse().unwrap_or(200.0),
            blacklist: env("HLP_BLACKLIST", "")
                .split(',')
                .filter_map(|s| {
                    let t = s.trim();
                    if t.is_empty() { None } else { Some(t.to_uppercase()) }
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct VariantCfg {
    pub id: &'static str,
    pub entry_sigma: f64,
    pub exit_sigma: f64,
    pub stop_sigma: f64,
    pub min_std_bp: f64,
    pub min_hold_ms: i64,
    pub max_hold_ms: i64,
    /// Per-variant size override. None → use Globals.size_usd.
    pub size_usd: Option<f64>,
    /// Asymmetric exit: only close on convergence if projected net PnL > 0
    /// (otherwise keep holding until max_hold). Empirically the difference
    /// between Flipster's T01~T04 (with) and S10/S11 (without).
    pub asymmetric_exit: bool,
}

/// Variant grid. Includes:
///   - hlp_A~H: original 8-variant exploration (size = Globals.size_usd)
///   - hlp_T01_best..T04_es35: ports of Flipster's verified +5~7 bp/trade
///     T-series. All use min_std≥4, asym_exit, $1000 fixed size, hold 5–10m.
pub fn variants() -> Vec<VariantCfg> {
    vec![
        // Original 8-variant exploration grid
        VariantCfg { id: "hlp_A_baseline",   entry_sigma: 2.5, exit_sigma: 0.3, stop_sigma: 100.0, min_std_bp: 0.5, min_hold_ms: 5_000,  max_hold_ms:  60_000, size_usd: None,         asymmetric_exit: false },
        VariantCfg { id: "hlp_B_minstd3",    entry_sigma: 2.5, exit_sigma: 0.3, stop_sigma: 100.0, min_std_bp: 3.0, min_hold_ms: 5_000,  max_hold_ms:  60_000, size_usd: None,         asymmetric_exit: false },
        VariantCfg { id: "hlp_C_minstd5",    entry_sigma: 2.5, exit_sigma: 0.3, stop_sigma: 100.0, min_std_bp: 5.0, min_hold_ms: 5_000,  max_hold_ms:  60_000, size_usd: None,         asymmetric_exit: false },
        VariantCfg { id: "hlp_D_high_sig",   entry_sigma: 4.0, exit_sigma: 0.3, stop_sigma: 100.0, min_std_bp: 0.5, min_hold_ms: 5_000,  max_hold_ms:  60_000, size_usd: None,         asymmetric_exit: false },
        VariantCfg { id: "hlp_E_stop6",      entry_sigma: 2.5, exit_sigma: 0.3, stop_sigma:   6.0, min_std_bp: 3.0, min_hold_ms: 5_000,  max_hold_ms:  60_000, size_usd: None,         asymmetric_exit: false },
        VariantCfg { id: "hlp_F_long_hold",  entry_sigma: 2.5, exit_sigma: 0.3, stop_sigma: 100.0, min_std_bp: 3.0, min_hold_ms: 5_000,  max_hold_ms: 180_000, size_usd: None,         asymmetric_exit: false },
        VariantCfg { id: "hlp_G_short_hold", entry_sigma: 2.5, exit_sigma: 0.3, stop_sigma: 100.0, min_std_bp: 3.0, min_hold_ms: 2_000,  max_hold_ms:  20_000, size_usd: None,         asymmetric_exit: false },
        VariantCfg { id: "hlp_H_tight_exit", entry_sigma: 2.5, exit_sigma: 0.0, stop_sigma: 100.0, min_std_bp: 3.0, min_hold_ms: 5_000,  max_hold_ms:  60_000, size_usd: None,         asymmetric_exit: false },

        // T-series — Flipster's verified best-of-sweep (asym + $1000 + 5~10m hold)
        VariantCfg { id: "hlp_T01_best",      entry_sigma: 3.0, exit_sigma: 0.3, stop_sigma: 100.0, min_std_bp: 4.0, min_hold_ms: 5_000,  max_hold_ms: 300_000, size_usd: Some(1000.0), asymmetric_exit: true  },
        VariantCfg { id: "hlp_T02_best_long", entry_sigma: 3.0, exit_sigma: 0.3, stop_sigma: 100.0, min_std_bp: 4.0, min_hold_ms: 5_000,  max_hold_ms: 600_000, size_usd: Some(1000.0), asymmetric_exit: true  },
        VariantCfg { id: "hlp_T03_minstd5",   entry_sigma: 3.0, exit_sigma: 0.3, stop_sigma: 100.0, min_std_bp: 5.0, min_hold_ms: 5_000,  max_hold_ms: 300_000, size_usd: Some(1000.0), asymmetric_exit: true  },
        VariantCfg { id: "hlp_T04_es35",      entry_sigma: 3.5, exit_sigma: 0.3, stop_sigma: 100.0, min_std_bp: 4.0, min_hold_ms: 5_000,  max_hold_ms: 300_000, size_usd: Some(1000.0), asymmetric_exit: true  },
    ]
}

use anyhow::{anyhow, Result};
use arc_swap::ArcSwap;
use log::{info, warn};
use redis::Commands;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Redis blob shape from update_market_state.py (key: {prefix}:{base}:{quote}:{window_minutes}).
#[derive(Debug, Deserialize)]
struct RedisMarketGapBlob {
    #[serde(default)]
    base_exchange: String,
    #[serde(default)]
    quote_exchange: String,
    window_minutes: i64,
    updated_at_ms: i64,
    avg_mid_gap_by_symbol: HashMap<String, f64>,
    #[serde(default)]
    avg_spread_by_symbol: HashMap<String, f64>,
}

/// Redis key `{prefix}:gate_gate_web_spreads:{window}` from update_market_state.py.
#[derive(Debug, Deserialize)]
struct RedisGateWebSpreadPairBlob {
    window_minutes: i64,
    updated_at_ms: i64,
    #[serde(default)]
    avg_spread_gate_bookticker_by_symbol: HashMap<String, f64>,
    #[serde(default)]
    avg_spread_gate_webbookticker_by_symbol: HashMap<String, f64>,
}

/// Redis key `{mid_corr_prefix}:{quote}:{window}` from update_market_mid_correlation.py.
#[derive(Debug, Deserialize)]
struct RedisMidCorrBlob {
    #[serde(default)]
    quote_exchange: String,
    window_minutes: i64,
    updated_at_ms: i64,
    #[serde(default)]
    min_samples: i64,
    #[serde(default)]
    corr_gate_bookticker_vs_quote_by_symbol: HashMap<String, f64>,
    #[serde(default)]
    corr_gate_webbookticker_vs_quote_by_symbol: HashMap<String, f64>,
    #[serde(default)]
    n_samples_by_symbol: HashMap<String, i64>,
}

/// Redis key `{price_change_prefix}:{source}:{window}` from update_market_state.py.
#[derive(Debug, Deserialize)]
struct RedisPriceChangeBlob {
    #[serde(default)]
    source: String,
    window_minutes: i64,
    updated_at_ms: i64,
    #[serde(default)]
    price_change_by_symbol: HashMap<String, f64>,
}

/// Redis key `{prefix}:gate_gate_web_mid_gap:{window}` from update_market_state.py.
/// avg_mid_gap = (gate_mid - gate_web_mid) / gate_mid over aligned 1s samples.
#[derive(Debug, Deserialize)]
struct RedisGateGateWebMidGapBlob {
    window_minutes: i64,
    updated_at_ms: i64,
    #[serde(default)]
    avg_mid_gap_by_symbol: HashMap<String, f64>,
}

/// Window (minutes) for auxiliary 1m Redis blobs (gate:quote:1, gate_web:quote:1, spread pair).
const REDIS_AUX_1M_WINDOW: i64 = 1;

/// Window (minutes) for auxiliary 5m Redis gap blobs (gate:quote:5, gate_web:quote:5).
const REDIS_AUX_5M_WINDOW: i64 = 5;

/// Window (minutes) for auxiliary 30m Redis gap blobs.
const REDIS_AUX_30M_WINDOW: i64 = 30;

/// Window (minutes) for auxiliary 60m Redis gap blobs.
const REDIS_AUX_60M_WINDOW: i64 = 60;

/// Window (minutes) for auxiliary 240m (4h) Redis gap blobs.
const REDIS_AUX_240M_WINDOW: i64 = 240;

/// Window (minutes) for auxiliary 720m (12h) Redis gap blobs.
const REDIS_AUX_720M_WINDOW: i64 = 720;

/// Default prefix for mid-correlation Redis keys.
const DEFAULT_MID_CORR_PREFIX: &str = "gate_hft:market_mid_corr";

/// Default window (minutes) for mid-correlation Redis blobs. Override with MID_CORR_WINDOW_MINUTES.
const DEFAULT_MID_CORR_WINDOW: i64 = 30;

/// Default prefix for price change Redis keys.
const DEFAULT_PRICE_CHANGE_PREFIX: &str = "gate_hft:price_change";

/// Price change windows to load from Redis. Override with PRICE_CHANGE_WINDOWS (comma-separated).
const DEFAULT_PRICE_CHANGE_WINDOWS: &[i64] = &[1, 5, 15, 30, 60, 240, 1440];

/// Shared state for readers; lock-free reads, writer swaps atomically.
/// ArcSwap<T> stores Arc<T>; we store Arc<MarketGapState> (single indirection).
pub type SharedMarketGapState = Arc<ArcSwap<MarketGapState>>;

#[derive(Debug, Clone)]
pub struct MarketGapState {
    pub window_minutes: i64,
    pub updated_at_ms: i64,
    pub avg_mid_gap_by_symbol: HashMap<String, f64>,
    pub avg_spread_by_symbol: HashMap<String, f64>,
    /// 1m: `gate_bookticker` vs quote exchange mid gap (Redis `gate:{quote}:1`).
    pub gap_1m_gate_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_1m_gate_gap: i64,
    /// 1m: `gate_webbookticker` vs quote mid gap (Redis `gate_web:{quote}:1`).
    pub gap_1m_gate_web_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_1m_gate_web_gap: i64,
    /// 5m: `gate_bookticker` vs quote exchange mid gap (Redis `gate:{quote}:5`).
    pub gap_5m_gate_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_5m_gate_gap: i64,
    /// 5m: `gate_webbookticker` vs quote mid gap (Redis `gate_web:{quote}:5`).
    pub gap_5m_gate_web_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_5m_gate_web_gap: i64,
    /// 5m: mean relative spread on `gate_bookticker` (Redis spread pair blob).
    pub spread_5m_gate_bookticker_by_symbol: HashMap<String, f64>,
    /// 5m: mean relative spread on `gate_webbookticker`.
    pub spread_5m_gate_webbookticker_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_5m_spread_pair: i64,
    /// 30m: `gate_bookticker` vs quote mid gap (Redis `gate:{quote}:30`).
    pub gap_30m_gate_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_30m_gate_gap: i64,
    /// 30m: `gate_webbookticker` vs quote mid gap (Redis `gate_web:{quote}:30`).
    pub gap_30m_gate_web_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_30m_gate_web_gap: i64,
    /// 30m: mean relative spread on `gate_bookticker`.
    pub spread_30m_gate_bookticker_by_symbol: HashMap<String, f64>,
    /// 30m: mean relative spread on `gate_webbookticker`.
    pub spread_30m_gate_webbookticker_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_30m_spread_pair: i64,
    /// 60m: `gate_bookticker` vs quote mid gap (Redis `gate:{quote}:60`).
    pub gap_60m_gate_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_60m_gate_gap: i64,
    /// 60m: `gate_webbookticker` vs quote mid gap (Redis `gate_web:{quote}:60`).
    pub gap_60m_gate_web_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_60m_gate_web_gap: i64,
    /// 60m: mean relative spread on `gate_bookticker`.
    pub spread_60m_gate_bookticker_by_symbol: HashMap<String, f64>,
    /// 60m: mean relative spread on `gate_webbookticker`.
    pub spread_60m_gate_webbookticker_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_60m_spread_pair: i64,
    /// 240m (4h): `gate_bookticker` vs quote mid gap.
    pub gap_240m_gate_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_240m_gate_gap: i64,
    pub gap_240m_gate_web_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_240m_gate_web_gap: i64,
    pub spread_240m_gate_bookticker_by_symbol: HashMap<String, f64>,
    pub spread_240m_gate_webbookticker_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_240m_spread_pair: i64,
    /// 720m (12h): `gate_bookticker` vs quote mid gap.
    pub gap_720m_gate_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_720m_gate_gap: i64,
    pub gap_720m_gate_web_vs_quote_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_720m_gate_web_gap: i64,
    pub spread_720m_gate_bookticker_by_symbol: HashMap<String, f64>,
    pub spread_720m_gate_webbookticker_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_720m_spread_pair: i64,
    /// 1m: mean relative spread on `gate_bookticker` (aligned 1s; Redis spread pair blob).
    pub spread_1m_gate_bookticker_by_symbol: HashMap<String, f64>,
    /// 1m: mean relative spread on `gate_webbookticker`.
    pub spread_1m_gate_webbookticker_by_symbol: HashMap<String, f64>,
    pub updated_at_ms_1m_spread_pair: i64,
    /// Mid-price Pearson correlation: gate_bookticker vs quote (per window from Redis).
    pub corr_gate_bookticker_vs_quote_by_symbol: HashMap<String, f64>,
    /// Mid-price Pearson correlation: gate_webbookticker vs quote.
    pub corr_gate_webbookticker_vs_quote_by_symbol: HashMap<String, f64>,
    /// Sample counts for correlation (min of gate_bt and web counts).
    pub corr_n_samples_by_symbol: HashMap<String, i64>,
    pub updated_at_ms_mid_corr: i64,
    /// Per-symbol price change by window: window_minutes -> (symbol -> price_change_ratio).
    /// Sources: "gate" and "binance". Key: (source, window).
    pub price_change_gate_by_window: HashMap<i64, HashMap<String, f64>>,
    pub price_change_binance_by_window: HashMap<i64, HashMap<String, f64>>,
    /// gate_bookticker vs gate_webbookticker mean mid-price gap by window (1m,5m,10m,30m,60m).
    /// avg_mid_gap = (gate_mid - gate_web_mid) / gate_mid; Redis key `{prefix}:gate_gate_web_mid_gap:{window}`.
    pub gap_gate_vs_gate_web_by_window: HashMap<i64, HashMap<String, f64>>,
}

impl MarketGapState {
    pub fn new(window_minutes: i64) -> Self {
        Self {
            window_minutes,
            updated_at_ms: 0,
            avg_mid_gap_by_symbol: HashMap::new(),
            avg_spread_by_symbol: HashMap::new(),
            gap_1m_gate_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_1m_gate_gap: 0,
            gap_1m_gate_web_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_1m_gate_web_gap: 0,
            gap_5m_gate_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_5m_gate_gap: 0,
            gap_5m_gate_web_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_5m_gate_web_gap: 0,
            spread_5m_gate_bookticker_by_symbol: HashMap::new(),
            spread_5m_gate_webbookticker_by_symbol: HashMap::new(),
            updated_at_ms_5m_spread_pair: 0,
            gap_30m_gate_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_30m_gate_gap: 0,
            gap_30m_gate_web_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_30m_gate_web_gap: 0,
            spread_30m_gate_bookticker_by_symbol: HashMap::new(),
            spread_30m_gate_webbookticker_by_symbol: HashMap::new(),
            updated_at_ms_30m_spread_pair: 0,
            gap_60m_gate_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_60m_gate_gap: 0,
            gap_60m_gate_web_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_60m_gate_web_gap: 0,
            spread_60m_gate_bookticker_by_symbol: HashMap::new(),
            spread_60m_gate_webbookticker_by_symbol: HashMap::new(),
            updated_at_ms_60m_spread_pair: 0,
            gap_240m_gate_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_240m_gate_gap: 0,
            gap_240m_gate_web_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_240m_gate_web_gap: 0,
            spread_240m_gate_bookticker_by_symbol: HashMap::new(),
            spread_240m_gate_webbookticker_by_symbol: HashMap::new(),
            updated_at_ms_240m_spread_pair: 0,
            gap_720m_gate_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_720m_gate_gap: 0,
            gap_720m_gate_web_vs_quote_by_symbol: HashMap::new(),
            updated_at_ms_720m_gate_web_gap: 0,
            spread_720m_gate_bookticker_by_symbol: HashMap::new(),
            spread_720m_gate_webbookticker_by_symbol: HashMap::new(),
            updated_at_ms_720m_spread_pair: 0,
            spread_1m_gate_bookticker_by_symbol: HashMap::new(),
            spread_1m_gate_webbookticker_by_symbol: HashMap::new(),
            updated_at_ms_1m_spread_pair: 0,
            corr_gate_bookticker_vs_quote_by_symbol: HashMap::new(),
            corr_gate_webbookticker_vs_quote_by_symbol: HashMap::new(),
            corr_n_samples_by_symbol: HashMap::new(),
            updated_at_ms_mid_corr: 0,
            price_change_gate_by_window: HashMap::new(),
            price_change_binance_by_window: HashMap::new(),
            gap_gate_vs_gate_web_by_window: HashMap::new(),
        }
    }

    pub fn get_avg_gap(&self, symbol: &str) -> Option<f64> {
        self.avg_mid_gap_by_symbol.get(symbol).copied()
    }

    pub fn get_avg_spread(&self, symbol: &str) -> Option<f64> {
        self.avg_spread_by_symbol.get(symbol).copied()
    }

    pub fn get_gap_1m_gate_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_1m_gate_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_gap_1m_gate_web_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_1m_gate_web_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_gap_5m_gate_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_5m_gate_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_gap_5m_gate_web_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_5m_gate_web_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_spread_5m_gate_bookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_5m_gate_bookticker_by_symbol.get(symbol).copied()
    }

    pub fn get_spread_5m_gate_webbookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_5m_gate_webbookticker_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_gap_30m_gate_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_30m_gate_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_gap_30m_gate_web_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_30m_gate_web_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_spread_30m_gate_bookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_30m_gate_bookticker_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_spread_30m_gate_webbookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_30m_gate_webbookticker_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_gap_60m_gate_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_60m_gate_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_gap_60m_gate_web_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_60m_gate_web_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_spread_60m_gate_bookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_60m_gate_bookticker_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_spread_60m_gate_webbookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_60m_gate_webbookticker_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_gap_240m_gate_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_240m_gate_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_gap_240m_gate_web_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_240m_gate_web_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_spread_240m_gate_bookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_240m_gate_bookticker_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_spread_240m_gate_webbookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_240m_gate_webbookticker_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_gap_720m_gate_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_720m_gate_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_gap_720m_gate_web_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.gap_720m_gate_web_vs_quote_by_symbol.get(symbol).copied()
    }

    pub fn get_spread_720m_gate_bookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_720m_gate_bookticker_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_spread_720m_gate_webbookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_720m_gate_webbookticker_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_spread_1m_gate_bookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_1m_gate_bookticker_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_spread_1m_gate_webbookticker(&self, symbol: &str) -> Option<f64> {
        self.spread_1m_gate_webbookticker_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_corr_gate_bookticker_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.corr_gate_bookticker_vs_quote_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_corr_gate_webbookticker_vs_quote(&self, symbol: &str) -> Option<f64> {
        self.corr_gate_webbookticker_vs_quote_by_symbol
            .get(symbol)
            .copied()
    }

    pub fn get_corr_n_samples(&self, symbol: &str) -> Option<i64> {
        self.corr_n_samples_by_symbol.get(symbol).copied()
    }

    pub fn get_price_change_gate(&self, symbol: &str, window_minutes: i64) -> Option<f64> {
        self.price_change_gate_by_window
            .get(&window_minutes)
            .and_then(|m| m.get(symbol).copied())
    }

    pub fn get_price_change_binance(&self, symbol: &str, window_minutes: i64) -> Option<f64> {
        self.price_change_binance_by_window
            .get(&window_minutes)
            .and_then(|m| m.get(symbol).copied())
    }

    pub fn get_gap_gate_vs_gate_web(&self, symbol: &str, window_minutes: i64) -> Option<f64> {
        self.gap_gate_vs_gate_web_by_window
            .get(&window_minutes)
            .and_then(|m| m.get(symbol).copied())
    }
}

/// Windows (minutes) to load `gate_gate_web_mid_gap` blobs from Redis.
const GATE_GATE_WEB_MID_GAP_WINDOWS: &[i64] = &[1, 5, 10, 30, 60];

pub struct MarketWatcher {
    questdb_url: String,
    update_interval_secs: u64,
    window_minutes: i64,
    base_exchange: String,
    quote_exchange: String,
    redis_url: Option<String>,
    redis_prefix: String,
    client: Client,
    state: SharedMarketGapState,
}

impl MarketWatcher {
    pub fn new(
        questdb_url: String,
        update_interval_secs: u64,
        window_minutes: i64,
        state: SharedMarketGapState,
        base_exchange: String,
        quote_exchange: String,
        redis_url: Option<String>,
        redis_prefix: String,
    ) -> Self {
        Self {
            questdb_url,
            update_interval_secs: update_interval_secs.max(1),
            window_minutes: window_minutes.max(1),
            base_exchange: base_exchange.to_lowercase(),
            quote_exchange: quote_exchange.to_lowercase(),
            redis_url,
            redis_prefix: redis_prefix.trim_end_matches(':').to_string(),
            client: Client::new(),
            state,
        }
    }

    pub fn start(self) {
        let update_interval = self.update_interval_secs;
        let window_minutes = self.window_minutes;
        let base = self.base_exchange.clone();
        let quote = self.quote_exchange.clone();
        let redis_enabled = self.redis_url.is_some();
        let mut watcher = self;
        std::thread::spawn(move || loop {
            let got = if let Some(ref redis_url) = watcher.redis_url {
                try_load_from_redis(
                    redis_url,
                    &watcher.redis_prefix,
                    &watcher.base_exchange,
                    &watcher.quote_exchange,
                    watcher.window_minutes,
                )
            } else {
                None
            };
            match got {
                Some(state) => {
                    if !state.avg_mid_gap_by_symbol.is_empty() {
                        let top_summary = format_top_gaps(&state.avg_mid_gap_by_symbol, 5);
                        info!(
                            "[MarketWatcher] Redis gap {}:{}:{}m ({} symbols): {}",
                            watcher.base_exchange,
                            watcher.quote_exchange,
                            watcher.window_minutes,
                            state.avg_mid_gap_by_symbol.len(),
                            top_summary
                        );
                    }
                    watcher.state.store(Arc::new(state));
                }
                None => match watcher.query_avg_gap() {
                    Ok((gaps, spreads)) => {
                        let filtered_gaps = filter_valid_gaps(gaps);
                        let filtered_spreads: HashMap<String, f64> = filtered_gaps
                            .keys()
                            .filter_map(|s| spreads.get(s).map(|&v| (s.clone(), v)))
                            .collect();
                        if !filtered_gaps.is_empty() {
                            let top_summary = format_top_gaps(&filtered_gaps, 5);
                            info!(
                                "[MarketWatcher] QuestDB gap ({} symbols): {}",
                                filtered_gaps.len(),
                                top_summary
                            );
                        }
                        let mut m = MarketGapState::new(watcher.window_minutes);
                        m.updated_at_ms = now_ms();
                        m.avg_mid_gap_by_symbol = filtered_gaps;
                        m.avg_spread_by_symbol = filtered_spreads;
                        if let Some(ref redis_url) = watcher.redis_url {
                            if let Ok(client) = redis::Client::open(redis_url.as_str()) {
                                if let Ok(mut conn) = client.get_connection() {
                                    enrich_1m_from_redis(
                                        &mut conn,
                                        &watcher.redis_prefix,
                                        &watcher.quote_exchange,
                                        &mut m,
                                    );
                                }
                            }
                        }
                        watcher.state.store(Arc::new(m));
                    }
                    Err(err) => {
                        warn!("[MarketWatcher] Redis miss and QuestDB failed: {}", err);
                    }
                },
            }
            std::thread::sleep(Duration::from_secs(update_interval));
        });
        info!(
            "[MarketWatcher] Started: interval={}s, window={}m, base={}, quote={}, redis={}",
            update_interval, window_minutes, base, quote, redis_enabled
        );
    }

    fn query_avg_gap(&mut self) -> Result<(HashMap<String, f64>, HashMap<String, f64>)> {
        // Returns (avg_mid_gap_by_symbol, avg_spread_by_symbol); spread from base exchange (ask-bid)/mid.
        let query = build_avg_gap_query(
            &self.base_exchange,
            &self.quote_exchange,
            self.window_minutes,
        );
        let url = format!("{}/exec", self.questdb_url.trim_end_matches('/'));
        let response = self
            .client
            .get(&url)
            .query(&[("query", &query)])
            .send()
            .map_err(|e| anyhow!("QuestDB request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().unwrap_or_default();
            return Err(anyhow!(
                "QuestDB error status {}: {}",
                status.as_u16(),
                text
            ));
        }

        let json: Value = response
            .json()
            .map_err(|e| anyhow!("QuestDB invalid JSON response: {}", e))?;

        if let Some(error) = json.get("error") {
            return Err(anyhow!(
                "QuestDB error: {}",
                error.as_str().unwrap_or("unknown")
            ));
        }

        parse_gap_dataset(&json)
    }
}

fn redis_key(prefix: &str, base: &str, quote: &str, window_minutes: i64) -> String {
    format!("{}:{}:{}:{}", prefix, base, quote, window_minutes)
}

fn bookticker_table_for_gap_base(base: &str) -> String {
    match base.to_lowercase().as_str() {
        "gate_web" => "gate_webbookticker".to_string(),
        b => format!("{}_bookticker", b),
    }
}

fn enrich_1m_from_redis(
    conn: &mut redis::Connection,
    prefix: &str,
    quote_exchange: &str,
    state: &mut MarketGapState,
) {
    let w = REDIS_AUX_1M_WINDOW;
    let key_gate = redis_key(prefix, "gate", quote_exchange, w);
    if let Ok(raw) = conn.get::<_, String>(&key_gate) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_1m_gate_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_1m_gate_gap = blob.updated_at_ms;
        }
    }
    let key_gw = redis_key(prefix, "gate_web", quote_exchange, w);
    if let Ok(raw) = conn.get::<_, String>(&key_gw) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_1m_gate_web_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_1m_gate_web_gap = blob.updated_at_ms;
        }
    }
    // 5m gap blobs (gate and gate_web)
    let w5 = REDIS_AUX_5M_WINDOW;
    let key_gate_5m = redis_key(prefix, "gate", quote_exchange, w5);
    if let Ok(raw) = conn.get::<_, String>(&key_gate_5m) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_5m_gate_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_5m_gate_gap = blob.updated_at_ms;
        }
    }
    let key_gw_5m = redis_key(prefix, "gate_web", quote_exchange, w5);
    if let Ok(raw) = conn.get::<_, String>(&key_gw_5m) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_5m_gate_web_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_5m_gate_web_gap = blob.updated_at_ms;
        }
    }
    let key_sp_5m = format!("{}:gate_gate_web_spreads:{}", prefix, w5);
    if let Ok(raw) = conn.get::<_, String>(&key_sp_5m) {
        if let Ok(blob) = serde_json::from_str::<RedisGateWebSpreadPairBlob>(&raw) {
            state.spread_5m_gate_bookticker_by_symbol = blob.avg_spread_gate_bookticker_by_symbol;
            state.spread_5m_gate_webbookticker_by_symbol =
                blob.avg_spread_gate_webbookticker_by_symbol;
            state.updated_at_ms_5m_spread_pair = blob.updated_at_ms;
        }
    }
    // 30m gap blobs (gate and gate_web) + spread pair
    let w30 = REDIS_AUX_30M_WINDOW;
    let key_gate_30m = redis_key(prefix, "gate", quote_exchange, w30);
    if let Ok(raw) = conn.get::<_, String>(&key_gate_30m) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_30m_gate_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_30m_gate_gap = blob.updated_at_ms;
        }
    }
    let key_gw_30m = redis_key(prefix, "gate_web", quote_exchange, w30);
    if let Ok(raw) = conn.get::<_, String>(&key_gw_30m) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_30m_gate_web_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_30m_gate_web_gap = blob.updated_at_ms;
        }
    }
    let key_sp_30m = format!("{}:gate_gate_web_spreads:{}", prefix, w30);
    if let Ok(raw) = conn.get::<_, String>(&key_sp_30m) {
        if let Ok(blob) = serde_json::from_str::<RedisGateWebSpreadPairBlob>(&raw) {
            state.spread_30m_gate_bookticker_by_symbol = blob.avg_spread_gate_bookticker_by_symbol;
            state.spread_30m_gate_webbookticker_by_symbol =
                blob.avg_spread_gate_webbookticker_by_symbol;
            state.updated_at_ms_30m_spread_pair = blob.updated_at_ms;
        }
    }
    // 60m gap blobs (gate and gate_web) + spread pair
    let w60 = REDIS_AUX_60M_WINDOW;
    let key_gate_60m = redis_key(prefix, "gate", quote_exchange, w60);
    if let Ok(raw) = conn.get::<_, String>(&key_gate_60m) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_60m_gate_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_60m_gate_gap = blob.updated_at_ms;
        }
    }
    let key_gw_60m = redis_key(prefix, "gate_web", quote_exchange, w60);
    if let Ok(raw) = conn.get::<_, String>(&key_gw_60m) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_60m_gate_web_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_60m_gate_web_gap = blob.updated_at_ms;
        }
    }
    let key_sp_60m = format!("{}:gate_gate_web_spreads:{}", prefix, w60);
    if let Ok(raw) = conn.get::<_, String>(&key_sp_60m) {
        if let Ok(blob) = serde_json::from_str::<RedisGateWebSpreadPairBlob>(&raw) {
            state.spread_60m_gate_bookticker_by_symbol = blob.avg_spread_gate_bookticker_by_symbol;
            state.spread_60m_gate_webbookticker_by_symbol =
                blob.avg_spread_gate_webbookticker_by_symbol;
            state.updated_at_ms_60m_spread_pair = blob.updated_at_ms;
        }
    }
    // 240m and 720m gap + spread_pair blobs
    let w240 = REDIS_AUX_240M_WINDOW;
    let key_gate_240m = redis_key(prefix, "gate", quote_exchange, w240);
    if let Ok(raw) = conn.get::<_, String>(&key_gate_240m) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_240m_gate_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_240m_gate_gap = blob.updated_at_ms;
        }
    }
    let key_gw_240m = redis_key(prefix, "gate_web", quote_exchange, w240);
    if let Ok(raw) = conn.get::<_, String>(&key_gw_240m) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_240m_gate_web_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_240m_gate_web_gap = blob.updated_at_ms;
        }
    }
    let key_sp_240m = format!("{}:gate_gate_web_spreads:{}", prefix, w240);
    if let Ok(raw) = conn.get::<_, String>(&key_sp_240m) {
        if let Ok(blob) = serde_json::from_str::<RedisGateWebSpreadPairBlob>(&raw) {
            state.spread_240m_gate_bookticker_by_symbol = blob.avg_spread_gate_bookticker_by_symbol;
            state.spread_240m_gate_webbookticker_by_symbol =
                blob.avg_spread_gate_webbookticker_by_symbol;
            state.updated_at_ms_240m_spread_pair = blob.updated_at_ms;
        }
    }
    let w720 = REDIS_AUX_720M_WINDOW;
    let key_gate_720m = redis_key(prefix, "gate", quote_exchange, w720);
    if let Ok(raw) = conn.get::<_, String>(&key_gate_720m) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_720m_gate_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_720m_gate_gap = blob.updated_at_ms;
        }
    }
    let key_gw_720m = redis_key(prefix, "gate_web", quote_exchange, w720);
    if let Ok(raw) = conn.get::<_, String>(&key_gw_720m) {
        if let Ok(blob) = serde_json::from_str::<RedisMarketGapBlob>(&raw) {
            state.gap_720m_gate_web_vs_quote_by_symbol = blob.avg_mid_gap_by_symbol;
            state.updated_at_ms_720m_gate_web_gap = blob.updated_at_ms;
        }
    }
    let key_sp_720m = format!("{}:gate_gate_web_spreads:{}", prefix, w720);
    if let Ok(raw) = conn.get::<_, String>(&key_sp_720m) {
        if let Ok(blob) = serde_json::from_str::<RedisGateWebSpreadPairBlob>(&raw) {
            state.spread_720m_gate_bookticker_by_symbol = blob.avg_spread_gate_bookticker_by_symbol;
            state.spread_720m_gate_webbookticker_by_symbol =
                blob.avg_spread_gate_webbookticker_by_symbol;
            state.updated_at_ms_720m_spread_pair = blob.updated_at_ms;
        }
    }
    let key_sp = format!("{}:gate_gate_web_spreads:{}", prefix, w);
    if let Ok(raw) = conn.get::<_, String>(&key_sp) {
        if let Ok(blob) = serde_json::from_str::<RedisGateWebSpreadPairBlob>(&raw) {
            state.spread_1m_gate_bookticker_by_symbol = blob.avg_spread_gate_bookticker_by_symbol;
            state.spread_1m_gate_webbookticker_by_symbol =
                blob.avg_spread_gate_webbookticker_by_symbol;
            state.updated_at_ms_1m_spread_pair = blob.updated_at_ms;
        }
    }
    // gate vs gate_web mid gap per window (update_market_state.py)
    for &gw in GATE_GATE_WEB_MID_GAP_WINDOWS {
        let gw_key = format!("{}:gate_gate_web_mid_gap:{}", prefix, gw);
        if let Ok(raw) = conn.get::<_, String>(&gw_key) {
            if let Ok(blob) = serde_json::from_str::<RedisGateGateWebMidGapBlob>(&raw) {
                state
                    .gap_gate_vs_gate_web_by_window
                    .insert(gw, blob.avg_mid_gap_by_symbol);
            }
        }
    }
    // Price change per window (update_market_state.py)
    let pc_prefix = std::env::var("PRICE_CHANGE_REDIS_PREFIX")
        .unwrap_or_else(|_| DEFAULT_PRICE_CHANGE_PREFIX.to_string());
    let pc_windows = DEFAULT_PRICE_CHANGE_WINDOWS;
    for &pcw in pc_windows {
        for (source, target) in [
            ("gate", &mut state.price_change_gate_by_window),
            ("binance", &mut state.price_change_binance_by_window),
        ] {
            let pc_key = format!("{}:{}:{}", pc_prefix, source, pcw);
            if let Ok(raw) = conn.get::<_, String>(&pc_key) {
                if let Ok(blob) = serde_json::from_str::<RedisPriceChangeBlob>(&raw) {
                    target.insert(pcw, blob.price_change_by_symbol);
                }
            }
        }
    }
    // Mid-price correlation (update_market_mid_correlation.py)
    let mid_corr_prefix = std::env::var("MID_CORR_REDIS_PREFIX")
        .unwrap_or_else(|_| DEFAULT_MID_CORR_PREFIX.to_string());
    let mid_corr_window: i64 = std::env::var("MID_CORR_WINDOW_MINUTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MID_CORR_WINDOW);
    let corr_key = format!("{}:{}:{}", mid_corr_prefix, quote_exchange, mid_corr_window);
    if let Ok(raw) = conn.get::<_, String>(&corr_key) {
        if let Ok(blob) = serde_json::from_str::<RedisMidCorrBlob>(&raw) {
            state.corr_gate_bookticker_vs_quote_by_symbol =
                blob.corr_gate_bookticker_vs_quote_by_symbol;
            state.corr_gate_webbookticker_vs_quote_by_symbol =
                blob.corr_gate_webbookticker_vs_quote_by_symbol;
            state.corr_n_samples_by_symbol = blob.n_samples_by_symbol;
            state.updated_at_ms_mid_corr = blob.updated_at_ms;
        }
    }
}

fn try_load_from_redis(
    redis_url: &str,
    prefix: &str,
    base_exchange: &str,
    quote_exchange: &str,
    window_minutes: i64,
) -> Option<MarketGapState> {
    let client = redis::Client::open(redis_url).ok()?;
    let mut conn = client.get_connection().ok()?;
    let key = redis_key(prefix, base_exchange, quote_exchange, window_minutes);
    let raw: String = conn.get(&key).ok()?;
    let blob: RedisMarketGapBlob = serde_json::from_str(&raw).ok()?;
    let mut state = MarketGapState::new(blob.window_minutes.max(1));
    state.updated_at_ms = blob.updated_at_ms;
    state.avg_mid_gap_by_symbol = blob.avg_mid_gap_by_symbol;
    state.avg_spread_by_symbol = blob.avg_spread_by_symbol;
    enrich_1m_from_redis(&mut conn, prefix, quote_exchange, &mut state);
    Some(state)
}

fn build_avg_gap_query(base: &str, quote: &str, window_minutes: i64) -> String {
    let w = window_minutes.max(1);
    let base_tbl = bookticker_table_for_gap_base(base);
    let quote_tbl = format!("{}_bookticker", quote.to_lowercase());
    format!(
        "WITH base_cte AS (\
            SELECT symbol, \
                   last((ask_price + bid_price) / 2.0) AS base_mid, \
                   last((ask_price - bid_price) / ((ask_price + bid_price) / 2.0)) AS spread_1s, \
                   timestamp \
            FROM {} \
            WHERE timestamp > dateadd('m', -{}, now()) \
            SAMPLE BY 1s \
        ), \
        quote_cte AS (\
            SELECT symbol, last((ask_price + bid_price) / 2.0) AS quote_mid, timestamp \
            FROM {} \
            WHERE timestamp > dateadd('m', -{}, now()) \
            SAMPLE BY 1s \
        ), \
        gap AS (\
            SELECT b.symbol, \
                   avg((b.base_mid - q.quote_mid) / b.base_mid) AS avg_mid_gap, \
                   avg(b.spread_1s) AS avg_spread \
            FROM base_cte b \
            JOIN quote_cte q ON b.symbol = q.symbol AND b.timestamp = q.timestamp \
            GROUP BY b.symbol \
        ) \
        SELECT * FROM gap ORDER BY abs(avg_mid_gap) DESC;",
        base_tbl, w, quote_tbl, w,
    )
}

fn parse_gap_dataset(json: &Value) -> Result<(HashMap<String, f64>, HashMap<String, f64>)> {
    let columns = json
        .get("columns")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("QuestDB response missing columns"))?;
    let dataset = json
        .get("dataset")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("QuestDB response missing dataset"))?;

    let mut symbol_idx: Option<usize> = None;
    let mut gap_idx: Option<usize> = None;
    let mut spread_idx: Option<usize> = None;
    for (idx, column) in columns.iter().enumerate() {
        let name = column
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        match name {
            "symbol" => symbol_idx = Some(idx),
            "avg_mid_gap" => gap_idx = Some(idx),
            "avg_spread" => spread_idx = Some(idx),
            _ => {}
        }
    }

    let symbol_idx = symbol_idx.ok_or_else(|| anyhow!("QuestDB response missing symbol column"))?;
    let gap_idx = gap_idx.ok_or_else(|| anyhow!("QuestDB response missing avg_mid_gap column"))?;

    let mut gaps = HashMap::new();
    let mut spreads = HashMap::new();
    for row in dataset {
        let row = row
            .as_array()
            .ok_or_else(|| anyhow!("QuestDB dataset row invalid"))?;
        let symbol = row
            .get(symbol_idx)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if symbol.is_empty() {
            continue;
        }
        let gap = row.get(gap_idx).and_then(|v| v.as_f64()).unwrap_or(0.0);
        gaps.insert(symbol.clone(), gap);
        if let Some(si) = spread_idx {
            let spread = row.get(si).and_then(|v| v.as_f64()).unwrap_or(0.0);
            spreads.insert(symbol, spread);
        }
    }

    Ok((gaps, spreads))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn format_top_gaps(map: &HashMap<String, f64>, top_n: usize) -> String {
    let mut entries: Vec<(&String, &f64)> = map.iter().collect();
    entries.sort_by(|a, b| {
        b.1.abs()
            .partial_cmp(&a.1.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut parts = Vec::new();
    for (symbol, gap) in entries.into_iter().take(top_n.max(1)) {
        parts.push(format!("{}={:.6}", symbol, gap));
    }

    if parts.is_empty() {
        "no gaps".to_string()
    } else {
        parts.join(", ")
    }
}

fn filter_valid_gaps(map: HashMap<String, f64>) -> HashMap<String, f64> {
    const MAX_ABS_GAP: f64 = 0.1;
    map.into_iter()
        .filter(|(_, gap)| gap.abs() < MAX_ABS_GAP)
        .collect()
}

//! Poll central QuestDB for the latest BIN + HL bookticker per base symbol.
//!
//! Strategy: every `poll_interval_ms`, fetch the most recent quote per
//! symbol from each table since the last poll. ASOF-pair them by base.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use pairs_core::{base_of, ExchangeName};
use std::collections::HashMap;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct PairTick {
    pub base: String,
    pub bin_bid: f64,
    pub bin_ask: f64,
    pub bin_ts: DateTime<Utc>,
    pub hl_bid: f64,
    pub hl_ask: f64,
    pub hl_ts: DateTime<Utc>,
}

impl PairTick {
    pub fn bin_mid(&self) -> f64 { (self.bin_bid + self.bin_ask) * 0.5 }
    pub fn hl_mid(&self) -> f64 { (self.hl_bid + self.hl_ask) * 0.5 }
    pub fn spread_bp(&self) -> f64 {
        let bm = self.bin_mid();
        if bm <= 0.0 { return 0.0; }
        (self.hl_mid() - bm) / bm * 1e4
    }
}

#[derive(Debug, Clone)]
struct LatestQuote {
    bid: f64,
    ask: f64,
    ts: DateTime<Utc>,
}

pub struct Reader {
    http_url: String,
    client: reqwest::Client,
    last_bin_ts: DateTime<Utc>,
    last_hl_ts: DateTime<Utc>,
    /// Most-recent (bid, ask, ts) per base symbol seen.
    bin_latest: HashMap<String, LatestQuote>,
    hl_latest: HashMap<String, LatestQuote>,
}

impl Reader {
    pub fn new(qdb_http_url: String) -> Self {
        let now = Utc::now();
        Self {
            http_url: qdb_http_url,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest"),
            // First poll fetches last 30s so the cache populates quickly.
            last_bin_ts: now - chrono::Duration::seconds(30),
            last_hl_ts: now - chrono::Duration::seconds(30),
            bin_latest: HashMap::new(),
            hl_latest: HashMap::new(),
        }
    }

    /// Run one poll cycle. Returns currently-valid PairTicks (both legs fresh
    /// within `max_age_ms`).
    pub async fn poll(&mut self, max_age_ms: i64) -> Result<Vec<PairTick>> {
        // 1) Pull new BIN ticks
        let new_bin = self.fetch_since(
            ExchangeName::Binance,
            "binance_bookticker",
            self.last_bin_ts,
        ).await?;
        for (base, q) in new_bin.into_iter() {
            if q.ts > self.last_bin_ts { self.last_bin_ts = q.ts; }
            self.bin_latest.insert(base, q);
        }

        // 2) Pull new HL ticks
        let new_hl = self.fetch_since(
            ExchangeName::Hyperliquid,
            "hyperliquid_bookticker",
            self.last_hl_ts,
        ).await?;
        for (base, q) in new_hl.into_iter() {
            if q.ts > self.last_hl_ts { self.last_hl_ts = q.ts; }
            self.hl_latest.insert(base, q);
        }

        // 3) Pair by base, drop stale
        let now = Utc::now();
        let cutoff = chrono::Duration::milliseconds(max_age_ms);
        let mut out = Vec::new();
        for (base, hl) in self.hl_latest.iter() {
            let bin = match self.bin_latest.get(base) { Some(b) => b, None => continue };
            if (now - hl.ts).abs() > cutoff || (now - bin.ts).abs() > cutoff { continue; }
            out.push(PairTick {
                base: base.clone(),
                bin_bid: bin.bid, bin_ask: bin.ask, bin_ts: bin.ts,
                hl_bid: hl.bid, hl_ask: hl.ask, hl_ts: hl.ts,
            });
        }
        Ok(out)
    }

    async fn fetch_since(
        &self,
        exch: ExchangeName,
        table: &str,
        _since: DateTime<Utc>,
    ) -> Result<Vec<(String, LatestQuote)>> {
        // QuestDB LATEST ON returns one (latest) row per symbol — extremely
        // efficient on partitioned bookticker tables. We restrict the time
        // window to last 30s so the planner uses the partition index.
        let sql = format!(
            "SELECT symbol, bid_price, ask_price, timestamp FROM {table} \
             WHERE timestamp > dateadd('s', -30, now()) \
             LATEST ON timestamp PARTITION BY symbol"
        );
        let url = format!(
            "{}/exec?query={}",
            self.http_url,
            urlencoding::encode(&sql)
        );
        let resp = self.client.get(&url).send().await
            .with_context(|| format!("qdb fetch {table}"))?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!("qdb {table}: {body}");
            return Ok(Vec::new());
        }
        let v: serde_json::Value = resp.json().await?;
        let dataset = match v.get("dataset").and_then(|x| x.as_array()) {
            Some(a) => a,
            None => return Ok(Vec::new()),
        };
        let mut latest: HashMap<String, LatestQuote> = HashMap::new();
        for row in dataset {
            let arr = match row.as_array() { Some(a) => a, None => continue };
            if arr.len() < 4 { continue; }
            let sym = match arr[0].as_str() { Some(s) => s, None => continue };
            let bid = match arr[1].as_f64() { Some(x) if x > 0.0 => x, _ => continue };
            let ask = match arr[2].as_f64() { Some(x) if x > 0.0 => x, _ => continue };
            let ts_s = match arr[3].as_str() { Some(s) => s, None => continue };
            let ts = match DateTime::parse_from_rfc3339(ts_s)
                .ok()
                .or_else(|| DateTime::parse_from_str(&ts_s.replace('Z', "+00:00"), "%Y-%m-%dT%H:%M:%S%.f%z").ok())
            {
                Some(t) => t.with_timezone(&Utc),
                None => continue,
            };
            let base = match base_of(exch, sym) { Some(b) => b, None => continue };
            let entry = latest.entry(base).or_insert(LatestQuote { bid, ask, ts });
            if ts > entry.ts { *entry = LatestQuote { bid, ask, ts }; }
        }
        Ok(latest.into_iter().collect())
    }
}

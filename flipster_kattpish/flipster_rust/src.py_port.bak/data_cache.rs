// Thread-safe market data cache for Flipster — mirrors gate_hft_rust/data_cache.rs.
//
// Flipster has TWO feeds per symbol:
//   api_bt: trading-api.flipster.io  (HMAC auth, leads in latency)
//   web_bt: api.flipster.io stream   (cookie auth, broader market data)
//
// Both maintain independent (bid, ask, server_time) state and are queried as a
// single SymbolSnapshot in the hot path.

use crate::types::{BookTickerC, TradeC};
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const SNAPSHOT_HISTORY_MAX: usize = 2048;
const SNAPSHOT_HISTORY_MAX_AGE_MS: i64 = 30_000;
const SNAPSHOT_PUSH_MIN_INTERVAL_MS: i64 = 50;

#[derive(Clone)]
struct TickerEntry {
    ticker: BookTickerC,
    received_at: i64,
}

#[derive(Clone)]
struct TradeEntry {
    trade: TradeC,
    received_at: i64,
}

/// Per-symbol snapshot returned to strategy each tick (3 read()s instead of 6+).
#[derive(Clone, Debug)]
pub struct SymbolSnapshot {
    pub api_bt: BookTickerC,
    pub web_bt: BookTickerC,
    pub api_received_at_ms: i64,
    pub web_received_at_ms: i64,
    /// Latest web `market/tickers` midPrice + timestamp, if any.
    /// Mirrors Python STATE.web_ticker_mid — used for freshness heartbeat on illiquid
    /// symbols (orderbook-v2 is sparse) and for orderbook-stale divergence filter.
    pub web_ticker_mid: Option<(f64, i64)>,
}

pub struct DataCache {
    has_received_any: AtomicBool,
    symbol_interner: DashMap<String, Arc<str>>,
    api_booktickers: DashMap<Arc<str>, TickerEntry>,
    web_booktickers: DashMap<Arc<str>, TickerEntry>,
    /// Flipster `market/tickers` midPrice — (mid, server_ts_ms). ~5Hz per symbol.
    web_ticker_mids: DashMap<Arc<str>, (f64, i64)>,
    web_trades: DashMap<Arc<str>, TradeEntry>,
    snapshot_history: DashMap<Arc<str>, Mutex<VecDeque<(i64, SymbolSnapshot)>>>,
}

impl DataCache {
    pub fn new() -> Self {
        Self {
            has_received_any: AtomicBool::new(false),
            symbol_interner: DashMap::new(),
            api_booktickers: DashMap::new(),
            web_booktickers: DashMap::new(),
            web_ticker_mids: DashMap::new(),
            web_trades: DashMap::new(),
            snapshot_history: DashMap::new(),
        }
    }

    #[inline(always)]
    pub fn has_data(&self) -> bool {
        self.has_received_any.load(Ordering::Relaxed)
    }

    #[inline(always)]
    fn intern_symbol(&self, symbol: &str) -> Arc<str> {
        if let Some(arc) = self.symbol_interner.get(symbol) {
            return arc.clone();
        }
        let arc: Arc<str> = Arc::from(symbol);
        self.symbol_interner.insert(symbol.to_string(), arc.clone());
        arc
    }

    pub fn update_api_bookticker(&self, symbol: &str, ticker: BookTickerC, received_at_ms: i64) {
        let should_insert = self
            .api_booktickers
            .get(symbol)
            .map(|e| e.ticker.server_time < ticker.server_time)
            .unwrap_or(true);
        if should_insert {
            self.has_received_any.store(true, Ordering::Relaxed);
            let key = self.intern_symbol(symbol);
            self.api_booktickers.insert(
                key,
                TickerEntry {
                    ticker,
                    received_at: received_at_ms,
                },
            );
        }
    }

    pub fn update_web_bookticker(&self, symbol: &str, ticker: BookTickerC, received_at_ms: i64) {
        let should_insert = self
            .web_booktickers
            .get(symbol)
            .map(|e| e.ticker.server_time < ticker.server_time)
            .unwrap_or(true);
        if should_insert {
            self.has_received_any.store(true, Ordering::Relaxed);
            let key = self.intern_symbol(symbol);
            self.web_booktickers.insert(
                key,
                TickerEntry {
                    ticker,
                    received_at: received_at_ms,
                },
            );
        }
    }

    pub fn update_web_ticker_mid(&self, symbol: &str, mid: f64, server_ts_ms: i64) {
        if mid <= 0.0 {
            return;
        }
        let should = self
            .web_ticker_mids
            .get(symbol)
            .map(|e| e.1 < server_ts_ms)
            .unwrap_or(true);
        if should {
            self.has_received_any.store(true, Ordering::Relaxed);
            let key = self.intern_symbol(symbol);
            self.web_ticker_mids.insert(key, (mid, server_ts_ms));
        }
    }

    pub fn get_web_ticker_mid(&self, symbol: &str) -> Option<(f64, i64)> {
        self.web_ticker_mids.get(symbol).map(|e| *e)
    }

    pub fn update_web_trade(&self, symbol: &str, trade: TradeC, received_at_ms: i64) {
        let should_insert = self
            .web_trades
            .get(symbol)
            .map(|e| e.trade.id != trade.id)
            .unwrap_or(true);
        if should_insert {
            self.has_received_any.store(true, Ordering::Relaxed);
            let key = self.intern_symbol(symbol);
            self.web_trades.insert(
                key,
                TradeEntry {
                    trade,
                    received_at: received_at_ms,
                },
            );
        }
    }

    /// Assemble per-symbol snapshot; returns None if either feed is missing.
    pub fn get_snapshot(&self, symbol: &str) -> Option<SymbolSnapshot> {
        let api = self.api_booktickers.get(symbol)?;
        let web = self.web_booktickers.get(symbol)?;
        let web_ticker_mid = self.web_ticker_mids.get(symbol).map(|e| *e);
        Some(SymbolSnapshot {
            api_bt: api.ticker,
            web_bt: web.ticker,
            api_received_at_ms: api.received_at,
            web_received_at_ms: web.received_at,
            web_ticker_mid,
        })
    }

    pub fn push_snapshot_history(&self, symbol: &str, wall_ms: i64, snap: SymbolSnapshot) {
        let key = self.intern_symbol(symbol);
        let entry = self
            .snapshot_history
            .entry(key)
            .or_insert_with(|| Mutex::new(VecDeque::with_capacity(64)));
        let mut deque = entry.lock();
        if let Some((last_t, _)) = deque.back() {
            if *last_t == wall_ms {
                deque.pop_back();
            } else if wall_ms.saturating_sub(*last_t) < SNAPSHOT_PUSH_MIN_INTERVAL_MS {
                return;
            }
        }
        deque.push_back((wall_ms, snap));
        while deque.len() > SNAPSHOT_HISTORY_MAX {
            deque.pop_front();
        }
        let cutoff = wall_ms.saturating_sub(SNAPSHOT_HISTORY_MAX_AGE_MS);
        while let Some((t, _)) = deque.front() {
            if *t < cutoff {
                deque.pop_front();
            } else {
                break;
            }
        }
    }

    /// Most recent snapshot with wall_ms <= deadline_ms.
    pub fn get_snapshot_before(&self, symbol: &str, deadline_ms: i64) -> Option<SymbolSnapshot> {
        let deque = self.snapshot_history.get(symbol)?;
        let guard = deque.lock();
        guard
            .iter()
            .rev()
            .find(|(t, _)| *t <= deadline_ms)
            .map(|(_, s)| s.clone())
    }

    pub fn counts(&self) -> (usize, usize) {
        (self.api_booktickers.len(), self.web_booktickers.len())
    }

    pub fn get_api_bookticker(&self, symbol: &str) -> Option<BookTickerC> {
        self.api_booktickers.get(symbol).map(|e| e.ticker)
    }

    pub fn get_web_bookticker(&self, symbol: &str) -> Option<BookTickerC> {
        self.web_booktickers.get(symbol).map(|e| e.ticker)
    }

    pub fn get_web_trade(&self, symbol: &str) -> Option<TradeC> {
        self.web_trades.get(symbol).map(|e| e.trade)
    }
}

impl Default for DataCache {
    fn default() -> Self {
        Self::new()
    }
}

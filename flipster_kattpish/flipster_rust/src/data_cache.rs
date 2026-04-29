// Thread-safe data cache for market data.
// DashMap: per-key shard locking so updates for different symbols don't block each other.
// Symbol interning: one Arc<str> per unique symbol to avoid repeated String allocation in hot path.

use crate::types::{BookTickerC, TradeC};
use dashmap::DashMap;
use log::debug;
use parking_lot::Mutex;
use std::collections::{HashSet, VecDeque};
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// Process-wide reference to the active DataCache, published by runner at startup.
/// Lets non-runner code (e.g. OrderManagerClient) peek at the latest booktickers
/// without threading the Arc through every call path.
static GLOBAL: OnceLock<Arc<DataCache>> = OnceLock::new();

pub fn set_global(cache: Arc<DataCache>) {
    let _ = GLOBAL.set(cache);
}

pub fn global() -> Option<Arc<DataCache>> {
    GLOBAL.get().cloned()
}

/// Max samples per symbol for bookticker snapshot history (strategy loop ~10ms → ~5s at 500).
const BOOKTICKER_SNAPSHOT_HISTORY_MAX: usize = 2048;
/// Drop samples older than this many ms behind the latest push (wall clock).
const BOOKTICKER_SNAPSHOT_HISTORY_MAX_AGE_MS: i64 = 30_000;
/// Env: minimum wall-clock ms between history pushes per symbol (`0` = every call, default).
const BOOKTICKER_SNAPSHOT_HISTORY_PUSH_MIN_INTERVAL_MS: i64 = 50;

/// Gate bookticker: current + optional previous + received_at in one entry.
#[derive(Clone)]
struct GateBookTickerEntry {
    current: BookTickerC,
    previous: Option<BookTickerC>,
    received_at: i64,
}

/// Base/Web bookticker or trade: value + received_at in one entry (single lock).
#[derive(Clone)]
struct TickerWithReceivedAt {
    ticker: BookTickerC,
    received_at: i64,
}

#[derive(Clone)]
struct TradeWithReceivedAt {
    trade: TradeC,
    received_at: i64,
}

/// One snapshot per symbol: 3 read() calls total instead of 6+ separate get_*.
#[derive(Clone, Debug)]
pub struct SymbolBooktickerSnapshot {
    pub gate_bt: BookTickerC,
    pub gate_web_bt: BookTickerC,
    pub base_bt: Option<BookTickerC>,
    pub gate_previous_bt: Option<BookTickerC>,
    pub gate_bt_received_at_ms: i64,
    pub gate_web_bt_received_at_ms: i64,
    pub base_bt_received_at_ms: i64,
}

pub struct DataCache {
    base_exchange: String,
    /// If Some, debug-log webbookticker and trade updates only for these symbols (env: DATA_CACHE_LOG_WEBBOOKTICKER_SYMBOLS).
    log_webbookticker_symbols: Option<HashSet<String>>,
    /// Set to true when any bookticker/trade is first inserted; avoids calling counts() every tick in signal loop.
    has_received_any: AtomicBool,
    /// One Arc<str> per unique symbol; used as key for the maps below to avoid per-message String alloc.
    symbol_interner: DashMap<String, Arc<str>>,
    base_booktickers: DashMap<Arc<str>, TickerWithReceivedAt>,
    gate_booktickers: DashMap<Arc<str>, GateBookTickerEntry>,
    gate_webbooktickers: DashMap<Arc<str>, TickerWithReceivedAt>,
    gate_trades: DashMap<Arc<str>, TradeWithReceivedAt>,
    /// (wall_ms, assembled snapshot) per symbol for ~decide_order previous window.
    bookticker_snapshot_history:
        DashMap<Arc<str>, Mutex<VecDeque<(i64, SymbolBooktickerSnapshot)>>>,
    /// Do not push a new history sample if last push was within this many ms (wall clock).
    bookticker_snapshot_push_min_interval_ms: i64,
}

impl DataCache {
    pub fn new(base_exchange: String) -> Self {
        let log_webbookticker_symbols = env::var("DATA_CACHE_LOG_WEBBOOKTICKER_SYMBOLS")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect::<HashSet<_>>()
            })
            .filter(|set| !set.is_empty());
        let bookticker_snapshot_push_min_interval_ms =
            BOOKTICKER_SNAPSHOT_HISTORY_PUSH_MIN_INTERVAL_MS;
        Self {
            base_exchange,
            log_webbookticker_symbols,
            has_received_any: AtomicBool::new(false),
            symbol_interner: DashMap::new(),
            base_booktickers: DashMap::new(),
            gate_booktickers: DashMap::new(),
            gate_webbooktickers: DashMap::new(),
            gate_trades: DashMap::new(),
            bookticker_snapshot_history: DashMap::new(),
            bookticker_snapshot_push_min_interval_ms,
        }
    }

    /// Record an assembled bookticker snapshot tagged with strategy wall time (e.g. current tick ms).
    /// When `bookticker_snapshot_push_min_interval_ms` > 0, skips push if the previous sample is newer
    /// than that many ms (limits history sampling rate).
    pub fn push_bookticker_snapshot_history(
        &self,
        symbol: &str,
        wall_ms: i64,
        snap: SymbolBooktickerSnapshot,
    ) {
        let key = self.intern_symbol(symbol);
        let entry = self
            .bookticker_snapshot_history
            .entry(key)
            .or_insert_with(|| Mutex::new(VecDeque::with_capacity(64)));
        let mut deque = entry.lock();
        if let Some((last_t, _)) = deque.back() {
            if *last_t == wall_ms {
                deque.pop_back();
            } else if self.bookticker_snapshot_push_min_interval_ms > 0
                && wall_ms.saturating_sub(*last_t) < self.bookticker_snapshot_push_min_interval_ms
            {
                return;
            }
        }
        deque.push_back((wall_ms, snap));
        while deque.len() > BOOKTICKER_SNAPSHOT_HISTORY_MAX {
            deque.pop_front();
        }
        let cutoff = wall_ms.saturating_sub(BOOKTICKER_SNAPSHOT_HISTORY_MAX_AGE_MS);
        while let Some((t, _)) = deque.front() {
            if *t < cutoff {
                deque.pop_front();
            } else {
                break;
            }
        }
    }

    /// Latest snapshot whose recorded wall time is at or before `deadline_ms` (use `now - 1000` for ~1s ago).
    pub fn get_bookticker_snapshot_before_wall_ms(
        &self,
        symbol: &str,
        deadline_ms: i64,
    ) -> Option<SymbolBooktickerSnapshot> {
        let key = self.intern_symbol(symbol);
        let deque = self.bookticker_snapshot_history.get(&key)?;
        let guard = deque.lock();
        guard
            .iter()
            .rev()
            .find(|(t, _)| *t <= deadline_ms)
            .map(|(_, s)| s.clone())
    }

    /// True once any bookticker or trade has been written; used by signal loop to skip counts() when checking for data.
    #[inline(always)]
    pub fn has_data(&self) -> bool {
        self.has_received_any.load(Ordering::Relaxed)
    }

    /// Return shared Arc<str> for symbol; allocates at most once per unique symbol.
    #[inline(always)]
    fn intern_symbol(&self, symbol: &str) -> Arc<str> {
        if let Some(arc) = self.symbol_interner.get(symbol) {
            return arc.clone();
        }
        let arc: Arc<str> = Arc::from(symbol);
        self.symbol_interner.insert(symbol.to_string(), arc.clone());
        arc
    }

    /// In-place update: O(1) per write; DashMap locks only the key's shard, so different symbols don't block each other.
    pub fn update_bookticker(
        &self,
        exchange: &str,
        symbol: &str,
        ticker: BookTickerC,
        received_at_ms: i64,
    ) {
        if exchange == "gate" {
            let should_insert = self
                .gate_booktickers
                .get(symbol)
                .map(|e| e.current.server_time < ticker.server_time)
                .unwrap_or(true);
            if should_insert {
                self.has_received_any.store(true, Ordering::Relaxed);
                let key = self.intern_symbol(symbol);
                let previous = self.gate_booktickers.get(symbol).map(|e| e.current);
                self.gate_booktickers.insert(
                    key,
                    GateBookTickerEntry {
                        current: ticker,
                        previous,
                        received_at: received_at_ms,
                    },
                );
            }
        } else if exchange == self.base_exchange {
            let should_insert = self
                .base_booktickers
                .get(symbol)
                .map(|e| e.ticker.server_time < ticker.server_time)
                .unwrap_or(true);
            if should_insert {
                self.has_received_any.store(true, Ordering::Relaxed);
                self.base_booktickers.insert(
                    self.intern_symbol(symbol),
                    TickerWithReceivedAt {
                        ticker,
                        received_at: received_at_ms,
                    },
                );
            }
        }
    }

    pub fn update_webbookticker(&self, symbol: &str, ticker: BookTickerC, received_at_ms: i64) {
        let should_insert = self
            .gate_webbooktickers
            .get(symbol)
            .map(|e| e.ticker.server_time < ticker.server_time)
            .unwrap_or(true);
        if should_insert {
            self.has_received_any.store(true, Ordering::Relaxed);
            self.gate_webbooktickers.insert(
                self.intern_symbol(symbol),
                TickerWithReceivedAt {
                    ticker,
                    received_at: received_at_ms,
                },
            );
            // let latency = received_at_ms - ticker.server_time;
            // if self
            //     .log_webbookticker_symbols
            //     .as_ref()
            //     .map_or(false, |s| s.contains(symbol))
            // {
            //     debug!(
            //         "[DataCache] webbookticker updated: {} | ask: {} | bid: {} | latency: {}ms",
            //         symbol, ticker.ask_price, ticker.bid_price, latency
            //     );
            // }
        }
    }

    pub fn update_trade(&self, symbol: &str, trade: TradeC, received_at_ms: i64) {
        let should_insert = self
            .gate_trades
            .get(symbol)
            .map(|e| e.trade.id != trade.id)
            .unwrap_or(true);
        if should_insert {
            self.has_received_any.store(true, Ordering::Relaxed);
            self.gate_trades.insert(
                self.intern_symbol(symbol),
                TradeWithReceivedAt {
                    trade,
                    received_at: received_at_ms,
                },
            );
            if self
                .log_webbookticker_symbols
                .as_ref()
                .map_or(false, |s| s.contains(symbol))
            {
                let latency = received_at_ms - trade.create_time_ms;
                debug!(
                    "[DataCache] trade updated: {} | price: {} | size: {} | latency: {}ms",
                    symbol, trade.price, trade.size, latency
                );
            }
        }
    }

    /// Per-symbol reads: each get() locks only that key's shard. Returns None if gate_bt or gate_web_bt missing.
    pub fn get_symbol_bookticker_snapshot(
        &self,
        symbol: &str,
        current_time_ms: i64,
    ) -> Option<SymbolBooktickerSnapshot> {
        let gate_entry = self.gate_booktickers.get(symbol)?;
        let gate_bt = gate_entry.current;
        let gate_previous_bt = gate_entry.previous;
        let gate_bt_received_at_ms = gate_entry.received_at;

        let gate_web_entry = self.gate_webbooktickers.get(symbol)?;
        let gate_web_bt = gate_web_entry.ticker;
        let gate_web_bt_received_at_ms = gate_web_entry.received_at;

        let base_entry = self.base_booktickers.get(symbol);
        let (base_bt, base_bt_received_at_ms) = base_entry
            .as_ref()
            .map(|e| (Some(e.ticker), e.received_at))
            .unwrap_or((None, current_time_ms));

        Some(SymbolBooktickerSnapshot {
            gate_bt,
            gate_web_bt,
            base_bt,
            gate_previous_bt,
            gate_bt_received_at_ms,
            gate_web_bt_received_at_ms,
            base_bt_received_at_ms,
        })
    }

    pub fn counts(&self) -> (usize, usize) {
        (self.base_booktickers.len(), self.gate_booktickers.len())
    }

    pub fn get_gate_booktickers(&self, symbol: &str) -> Option<(BookTickerC, BookTickerC)> {
        let gate_bt = self.gate_booktickers.get(symbol).map(|e| e.current)?;
        let gate_web_bt = self.gate_webbooktickers.get(symbol).map(|e| e.ticker)?;
        Some((gate_bt, gate_web_bt))
    }

    pub fn get_gate_bookticker_full(
        &self,
        symbol: &str,
    ) -> Option<(BookTickerC, Option<BookTickerC>, i64)> {
        let e = self.gate_booktickers.get(symbol)?;
        Some((e.current, e.previous, e.received_at))
    }

    pub fn get_base_bookticker(&self, symbol: &str) -> Option<BookTickerC> {
        self.base_booktickers.get(symbol).map(|e| e.ticker)
    }

    pub fn get_gate_bookticker(&self, symbol: &str) -> Option<BookTickerC> {
        self.gate_booktickers.get(symbol).map(|e| e.current)
    }

    pub fn get_previous_gate_bookticker(&self, symbol: &str) -> Option<BookTickerC> {
        self.gate_booktickers.get(symbol).and_then(|e| e.previous)
    }

    pub fn get_gate_webbookticker(&self, symbol: &str) -> Option<BookTickerC> {
        self.gate_webbooktickers.get(symbol).map(|e| e.ticker)
    }

    pub fn get_gate_trade(&self, symbol: &str) -> Option<TradeC> {
        self.gate_trades.get(symbol).map(|e| e.trade)
    }

    pub fn get_gate_trade_received_at(&self, symbol: &str) -> Option<i64> {
        self.gate_trades.get(symbol).map(|e| e.received_at)
    }

    pub fn get_base_received_at(&self, symbol: &str) -> Option<i64> {
        self.base_booktickers.get(symbol).map(|e| e.received_at)
    }

    pub fn get_gate_bookticker_received_at(&self, symbol: &str) -> Option<i64> {
        self.gate_booktickers.get(symbol).map(|e| e.received_at)
    }

    pub fn get_gate_webbookticker_received_at(&self, symbol: &str) -> Option<i64> {
        self.gate_webbooktickers.get(symbol).map(|e| e.received_at)
    }

    pub fn base_count(&self) -> usize {
        self.base_booktickers.len()
    }

    pub fn base_exchange(&self) -> &str {
        &self.base_exchange
    }

    pub fn gate_count(&self) -> usize {
        self.gate_booktickers.len()
    }
}

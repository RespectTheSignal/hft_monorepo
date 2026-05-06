//! In-memory quote state per coin.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct MyQuote {
    /// HL order id from `place_alo`.
    pub oid: u64,
    pub price: f64,
    pub size: f64,
    pub placed_at: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct CoinState {
    pub bin_bid: f64,
    pub bin_ask: f64,
    pub hl_bid: f64,
    pub hl_ask: f64,
    /// Our resting orders (None means we have no quote on that side).
    pub my_bid: Option<MyQuote>,
    pub my_ask: Option<MyQuote>,
    /// Open net position (size in coin units; +long, -short).
    pub net_pos: f64,
    pub last_bin_tick: Option<Instant>,
    pub last_hl_tick: Option<Instant>,
}

#[derive(Default, Clone)]
pub struct QuoterState {
    inner: Arc<RwLock<HashMap<String, CoinState>>>,
}

impl QuoterState {
    pub fn new() -> Self { Self::default() }

    pub fn update_bin(&self, coin: &str, bid: f64, ask: f64) {
        let mut g = self.inner.write();
        let c = g.entry(coin.to_string()).or_default();
        c.bin_bid = bid;
        c.bin_ask = ask;
        c.last_bin_tick = Some(Instant::now());
    }

    pub fn update_hl(&self, coin: &str, bid: f64, ask: f64) {
        let mut g = self.inner.write();
        let c = g.entry(coin.to_string()).or_default();
        c.hl_bid = bid;
        c.hl_ask = ask;
        c.last_hl_tick = Some(Instant::now());
    }

    pub fn snapshot(&self, coin: &str) -> Option<CoinState> {
        self.inner.read().get(coin).cloned()
    }

    pub fn set_my_quote(&self, coin: &str, side: Side, q: Option<MyQuote>) {
        let mut g = self.inner.write();
        let c = g.entry(coin.to_string()).or_default();
        match side {
            Side::Bid => c.my_bid = q,
            Side::Ask => c.my_ask = q,
        }
    }

    pub fn add_to_pos(&self, coin: &str, sz: f64) {
        let mut g = self.inner.write();
        let c = g.entry(coin.to_string()).or_default();
        c.net_pos += sz;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side { Bid, Ask }

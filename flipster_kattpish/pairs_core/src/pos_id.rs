//! Process-monotonic position-ID generator.
//!
//! Why this exists: the executor dedupes incoming signals on
//! `(base, position_id)` and never garbage-collects the set. Each strategy
//! used to keep its own `next_id: u64` counter starting from 1, so every
//! collector restart re-issued IDs (BTC, 1), (BTC, 2), ... which the
//! executor silently dropped as duplicates. Symptom: paper bot
//! restarted → executor stops trading until *its* restart clears
//! `seen_entries`.
//!
//! Fix: seed all strategies' counters from `unix_nanos_at_startup`. After
//! any collector restart the new seed is strictly greater than every ID
//! the previous instance ever emitted, so collisions cannot happen
//! (clock skew aside — and the OS clock in this stack is NTP-synced).
//!
//! 64 bits is plenty: unix-nanos at 2026 is ~1.78×10¹⁸; u64 max is
//! 1.84×10¹⁹, leaving headroom through year 2554.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct PositionIdGen {
    next: AtomicU64,
}

impl PositionIdGen {
    pub fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self { next: AtomicU64::new(seed.max(1)) }
    }

    /// Allocate a fresh ID. Lock-free, monotonic within a process,
    /// strictly greater than any ID a prior process emitted (assuming
    /// NTP-synced clock).
    pub fn next(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for PositionIdGen {
    fn default() -> Self {
        Self::new()
    }
}

/// Single global generator shared by every strategy in the process. All
/// callers must use this to keep IDs unique across the whole collector,
/// not just within their own strategy.
pub fn global() -> &'static PositionIdGen {
    static G: OnceLock<PositionIdGen> = OnceLock::new();
    G.get_or_init(PositionIdGen::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_strictly_increase() {
        let g = PositionIdGen::new();
        let a = g.next();
        let b = g.next();
        let c = g.next();
        assert!(b > a);
        assert!(c > b);
    }

    #[test]
    fn seed_is_in_recent_unix_range() {
        let g = PositionIdGen::new();
        let id = g.next();
        // Sanity: between 2026 and 2200 in unix nanos.
        assert!(id > 1_700_000_000_000_000_000);
        assert!(id < 7_000_000_000_000_000_000);
    }

    #[test]
    fn global_is_stable() {
        let a = global().next();
        let b = global().next();
        assert!(b > a);
    }
}

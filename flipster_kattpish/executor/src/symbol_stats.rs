//! Re-exports of the canonical sym-stats store from `pairs_core`.
//!
//! Definition moved to `pairs_core::symbol_stats` (Phase 3a) so the
//! collector's `live_stats` consumer can use the same types and the same
//! filtering semantics. Existing call sites
//! (`use crate::symbol_stats::{...};`) keep working unchanged.

pub use pairs_core::symbol_stats::{
    FilterDecision, SkipReason, SymbolStats, SymbolStatsStore, COOLDOWN_SECS, MIN_SAMPLES,
    MIN_SAMPLES_FOR_SCALE, PAPER_THRESHOLD_BP, PNL_DOWNSIZE_BP, PNL_THRESHOLD_BP, PNL_UPSIZE_BP,
    PROBE_COUNT, SCALE_DOWN_FACTOR, SCALE_MAX, SCALE_MIN, SCALE_UP_FACTOR, SLIP_HIGH_BP,
    SLIP_LOW_BP, SPREAD_THRESHOLD_BP, WINDOW,
};

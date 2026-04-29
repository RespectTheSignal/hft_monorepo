//! Re-exports of the canonical sym-stats store from `pairs_core`.
//!
//! Definition moved to `pairs_core::symbol_stats` (Phase 3a) so the
//! collector's `live_stats` consumer can use the same types and the same
//! filtering semantics. Existing call sites
//! (`use crate::symbol_stats::{...};`) keep working unchanged.

// Only the items the executor actually references after Phase 3b's
// filter migration. Other constants (`PNL_*`, `SCALE_*`, `WINDOW`, ...)
// stay in `pairs_core::symbol_stats` and are reachable via the qualified
// path if a future caller needs them.
pub use pairs_core::symbol_stats::{FilterDecision, SymbolStatsStore};

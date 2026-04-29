//! Re-exports of the canonical tick types from `pairs_core`.
//!
//! Historically defined here. Moved to `pairs_core::tick` so the executor
//! crate can share the same `BookTick` / `ExchangeName` / `Quote` /
//! `base_of` without depending on collector internals. Existing call sites
//! (`use crate::model::{BookTick, ExchangeName};`) continue to compile
//! unchanged.

pub use pairs_core::tick::{base_of, BookTick, ExchangeName, Quote};

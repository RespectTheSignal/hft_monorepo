//! Position-side helpers: which price gets used for entry/exit and how PnL is
//! computed.
//!
//! These small functions were previously inlined in three places each across
//! `collector::strategies::sweep_exits` (pair, two-leg), `spread_revert` /
//! `gate_lead` (single-leg Flipster only). Centralizing them eliminates the
//! "long → bid? long → ask?" reasoning at every call site.
//!
//! Convention: `side = +1` means LONG flipster (we bought to open). `side =
//! -1` means SHORT flipster.

use crate::tick::Quote;

/// Price at which a side enters its Flipster leg given the BBO at decision
/// time. Long crosses up to the ask; short crosses down to the bid.
pub fn flipster_entry_price(side: i32, q: &Quote) -> f64 {
    if side > 0 {
        q.ask
    } else {
        q.bid
    }
}

/// Price at which a side closes its Flipster leg given the BBO. Inverse of
/// entry: long closes by selling (bid); short closes by buying (ask).
pub fn flipster_exit_price(side: i32, q: &Quote) -> f64 {
    if side > 0 {
        q.bid
    } else {
        q.ask
    }
}

/// Single-leg PnL in basis points, signed by side. Caller subtracts fees.
/// Formula: `pnl_bp = (exit - entry) / entry * 1e4 * side`.
pub fn single_leg_pnl_bp(entry: f64, exit_price: f64, side: i32) -> f64 {
    if entry <= 0.0 {
        return 0.0;
    }
    (exit_price - entry) / entry * 10_000.0 * (side as f64)
}

/// Two-leg PnL for a delta-neutral pair (Flipster + hedge venue), in basis
/// points, before fees.
///
/// `flipster_side = +1` means long Flipster / short hedge; the hedge leg
/// flips sign accordingly.
pub fn pair_pnl_bp(
    flipster_entry: f64,
    flipster_exit: f64,
    hedge_entry: f64,
    hedge_exit: f64,
    flipster_side: i32,
) -> f64 {
    let f_leg = single_leg_pnl_bp(flipster_entry, flipster_exit, flipster_side);
    let h_leg = single_leg_pnl_bp(hedge_entry, hedge_exit, -flipster_side);
    f_leg + h_leg
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn q(bid: f64, ask: f64) -> Quote {
        Quote { bid, ask, ts: Utc::now() }
    }

    #[test]
    fn entry_exit_directions() {
        let q = q(100.0, 100.2);
        assert_eq!(flipster_entry_price(1, &q), 100.2);
        assert_eq!(flipster_entry_price(-1, &q), 100.0);
        assert_eq!(flipster_exit_price(1, &q), 100.0);
        assert_eq!(flipster_exit_price(-1, &q), 100.2);
    }

    #[test]
    fn single_leg_signs() {
        // Long: price moved up 1% → +10000 bp * 1 = positive.
        assert!((single_leg_pnl_bp(100.0, 101.0, 1) - 100.0).abs() < 1e-6);
        // Short: price moved up 1% → -100 bp.
        assert!((single_leg_pnl_bp(100.0, 101.0, -1) + 100.0).abs() < 1e-6);
    }

    #[test]
    fn pair_pnl_neutral() {
        // Both legs move 1% in same direction → delta-neutral pair returns 0.
        let r = pair_pnl_bp(100.0, 101.0, 100.0, 101.0, 1);
        assert!(r.abs() < 1e-6);
    }

    #[test]
    fn pair_pnl_divergence() {
        // Flipster +1%, hedge flat, long flipster → +100bp gross.
        let r = pair_pnl_bp(100.0, 101.0, 100.0, 100.0, 1);
        assert!((r - 100.0).abs() < 1e-6);
    }
}

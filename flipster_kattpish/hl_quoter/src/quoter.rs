//! Core quoter logic — decide when to (re)quote vs cancel.

use crate::state::{CoinState, MyQuote, Side};

#[derive(Debug, Clone)]
pub struct QuoterCfg {
    pub size_usd: f64,
    /// bp behind HL BBO. 1 = 1 tick behind (safe). 0 = at BBO (cross risk).
    pub place_offset_bp: f64,
    /// Cancel when BIN through our quote by this many bp.
    pub safety_bp: f64,
    /// Min time between place attempts per side (ms). Hard rate limit.
    pub min_replace_ms: u64,
    pub max_inventory_coin: f64,
    /// Cooldown after rejected place before retrying same side (ms).
    pub fail_cooldown_ms: u64,
}

impl Default for QuoterCfg {
    fn default() -> Self {
        Self {
            size_usd: 12.0,
            place_offset_bp: 1.0,
            safety_bp: 1.0,
            min_replace_ms: 500,
            max_inventory_coin: 0.0,
            fail_cooldown_ms: 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Place { side: Side, price: f64, size: f64 },
    Cancel { side: Side, oid: u64 },
    Hold,
}

/// Sentinels in MyQuote.oid:
///   0          = in-flight place (awaiting SDK response)
///   u64::MAX   = recent failed place (cooldown active)
///   u64::MAX-1 = in-flight cancel
///   any other  = real resting order id
pub const SENTINEL_INFLIGHT_PLACE: u64 = 0;
pub const SENTINEL_FAILED_PLACE: u64 = u64::MAX;
pub const SENTINEL_INFLIGHT_CANCEL: u64 = u64::MAX - 1;

pub fn decide(cfg: &QuoterCfg, cs: &CoinState, side: Side) -> Action {
    let (bin_ref, my, target_px) = match side {
        Side::Bid => (cs.bin_bid, &cs.my_bid, cs.hl_bid * (1.0 - cfg.place_offset_bp / 1e4)),
        Side::Ask => (cs.bin_ask, &cs.my_ask, cs.hl_ask * (1.0 + cfg.place_offset_bp / 1e4)),
    };
    if bin_ref <= 0.0 || cs.hl_bid <= 0.0 || cs.hl_ask <= 0.0 || cs.bin_bid <= 0.0 || cs.bin_ask <= 0.0 {
        return Action::Hold;
    }
    // Inventory cap
    if cfg.max_inventory_coin > 0.0 {
        if cs.net_pos >= cfg.max_inventory_coin && side == Side::Bid {
            return match my {
                Some(q) if q.oid != SENTINEL_INFLIGHT_PLACE && q.oid != SENTINEL_INFLIGHT_CANCEL && q.oid != SENTINEL_FAILED_PLACE
                    => Action::Cancel { side, oid: q.oid },
                _ => Action::Hold,
            };
        }
        if cs.net_pos <= -cfg.max_inventory_coin && side == Side::Ask {
            return match my {
                Some(q) if q.oid != SENTINEL_INFLIGHT_PLACE && q.oid != SENTINEL_INFLIGHT_CANCEL && q.oid != SENTINEL_FAILED_PLACE
                    => Action::Cancel { side, oid: q.oid },
                _ => Action::Hold,
            };
        }
    }
    let need_place = match my {
        None => true,
        Some(q) if q.oid == SENTINEL_INFLIGHT_PLACE => return Action::Hold,
        Some(q) if q.oid == SENTINEL_INFLIGHT_CANCEL => return Action::Hold,
        Some(q) if q.oid == SENTINEL_FAILED_PLACE => {
            if q.placed_at.elapsed().as_millis() < cfg.fail_cooldown_ms as u128 {
                return Action::Hold;
            }
            true
        }
        Some(q) => {
            // Real resting order. Stale = BIN moved THROUGH our quote
            // (= about to be hit by an informed taker tracking BIN).
            // Ask stale: BIN ask climbed above our ask + safety.
            // Bid stale: BIN bid dropped below our bid - safety.
            let stale = match side {
                Side::Ask => cs.bin_ask > q.price * (1.0 + cfg.safety_bp / 1e4),
                Side::Bid => cs.bin_bid < q.price * (1.0 - cfg.safety_bp / 1e4),
            };
            if stale { return Action::Cancel { side, oid: q.oid }; }
            return Action::Hold;
        }
    };
    if !need_place { return Action::Hold; }
    let price = round_to_sig(target_px, 5);
    let size = cfg.size_usd / price;
    Action::Place { side, price, size }
}

fn round_to_sig(x: f64, sig: i32) -> f64 {
    if x == 0.0 { return 0.0; }
    let mag = x.abs().log10().floor() as i32;
    let scale = 10f64.powi(sig - 1 - mag);
    (x * scale).round() / scale
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn cs(bid: f64, ask: f64) -> CoinState {
        CoinState { bin_bid: bid, bin_ask: ask, hl_bid: bid, hl_ask: ask, ..Default::default() }
    }

    #[test]
    fn place_when_no_quote() {
        let c = cs(99.0, 101.0);
        let cfg = QuoterCfg::default();
        match decide(&cfg, &c, Side::Bid) {
            // 99 * (1 - 1bp) = 98.9901 → round to 5 sig figs = 98.99
            Action::Place { price, .. } => assert!((price - 98.99).abs() < 1e-4, "got {price}"),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn hold_inflight_place() {
        let mut c = cs(100.0, 101.0);
        c.my_ask = Some(MyQuote { oid: SENTINEL_INFLIGHT_PLACE, price: 0.0, size: 0.0, placed_at: Instant::now() });
        assert_eq!(decide(&QuoterCfg::default(), &c, Side::Ask), Action::Hold);
    }

    #[test]
    fn hold_inflight_cancel() {
        let mut c = cs(100.0, 101.0);
        c.my_ask = Some(MyQuote { oid: SENTINEL_INFLIGHT_CANCEL, price: 101.0, size: 0.1, placed_at: Instant::now() });
        assert_eq!(decide(&QuoterCfg::default(), &c, Side::Ask), Action::Hold);
    }

    #[test]
    fn hold_after_fail_within_cooldown() {
        let mut c = cs(100.0, 101.0);
        c.my_ask = Some(MyQuote { oid: SENTINEL_FAILED_PLACE, price: 101.0, size: 0.1, placed_at: Instant::now() });
        assert_eq!(decide(&QuoterCfg::default(), &c, Side::Ask), Action::Hold);
    }

    #[test]
    fn cancel_when_bin_breaches_above_our_ask() {
        // Our ask=101, bin_ask=102 → bin > 101*(1+1bp)=101.0101 → stale
        let mut c = cs(100.0, 102.0);
        c.my_ask = Some(MyQuote { oid: 42, price: 101.0, size: 0.1, placed_at: Instant::now() });
        assert!(matches!(decide(&QuoterCfg::default(), &c, Side::Ask), Action::Cancel { oid: 42, .. }));
    }

    #[test]
    fn hold_when_bin_below_our_ask() {
        // Our ask=101, bin_ask=100.9 → not stale → Hold
        let mut c = cs(100.0, 100.9);
        c.my_ask = Some(MyQuote { oid: 42, price: 101.0, size: 0.1, placed_at: Instant::now() });
        assert_eq!(decide(&QuoterCfg::default(), &c, Side::Ask), Action::Hold);
    }
}

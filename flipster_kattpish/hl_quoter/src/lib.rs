//! HL latency-arb maker quoter.
//!
//! Strategy: place passive limit orders on both sides of HL's book at
//! prices tracking Binance BBO. When Binance moves and our HL quote
//! becomes stale (= about to be adversely filled by an informed taker),
//! cancel + replace at the new fair price. When unhit, accumulate maker
//! rebate + spread.
//!
//! Built on the BBO tick lag analysis showing HL follows Binance with
//! median 250 ms on SUI/ARB — fast enough that we can cancel before
//! adverse fill if our reaction is < 200 ms end-to-end.

pub mod state;
pub mod quoter;

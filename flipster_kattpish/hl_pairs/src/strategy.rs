//! One paper-trading variant. Maintains per-symbol EWMA + open position.

use crate::config::{Globals, VariantCfg};
use crate::ewma::Ewma;
use crate::exec::LiveExec;
use crate::ilp::{ClosedTrade, Writer};
use crate::reader::PairTick;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
struct State {
    ewma: Ewma,
    last_tick_ts: DateTime<Utc>,
    /// (side, entry_spread_bp, entry_hl_mid, entry_time) — None means flat.
    open: Option<OpenPos>,
}

#[derive(Debug, Clone)]
struct OpenPos {
    side: &'static str, // "long" | "short" — referring to HL position direction
    entry_spread_bp: f64,
    entry_hl_mid: f64,
    entry_time: DateTime<Utc>,
}

pub struct Variant {
    pub cfg: VariantCfg,
    globals: Globals,
    writer: Writer,
    states: HashMap<String, State>,
    n_opened: u64,
    n_closed: u64,
    /// Some(_) → also fire IOC orders to HL via this client. Trades still
    /// recorded to position_log paper-style for comparison.
    live_exec: Option<Arc<LiveExec>>,
}

impl Variant {
    pub fn new(cfg: VariantCfg, globals: Globals, writer: Writer) -> Self {
        Self { cfg, globals, writer, states: HashMap::new(), n_opened: 0, n_closed: 0, live_exec: None }
    }

    pub fn with_live(mut self, exec: Arc<LiveExec>) -> Self {
        self.live_exec = Some(exec);
        self
    }
    pub fn is_live(&self) -> bool { self.live_exec.is_some() }

    pub fn on_tick_batch(&mut self, ticks: &[PairTick]) {
        let now = Utc::now();
        for t in ticks {
            if self.globals.blacklist.contains(&t.base) { continue; }
            self.on_tick(t, now);
        }
    }

    fn on_tick(&mut self, t: &PairTick, now: DateTime<Utc>) {
        let spread = t.spread_bp();
        let hl_mid = t.hl_mid();
        let st = self.states.entry(t.base.clone()).or_insert_with(|| State {
            ewma: Ewma::new(self.globals.ewma_tau_sec),
            last_tick_ts: now,
            open: None,
        });

        // Update EWMA with elapsed since last tick
        let dt = (now - st.last_tick_ts).num_milliseconds() as f64 / 1000.0;
        st.last_tick_ts = now;
        if dt > 0.0 && dt < 30.0 {
            st.ewma.update(spread, dt);
        } else if dt >= 30.0 {
            // gap → just reset to current value to avoid huge alpha
            st.ewma.update(spread, 1.0);
        }

        let z = match st.ewma.z(spread) { Some(v) => v, None => return };
        let std_bp = st.ewma.std();

        // ---- Check exits first ----
        if let Some(pos) = st.open.clone() {
            let elapsed = (now - pos.entry_time).num_milliseconds();
            if elapsed < self.cfg.min_hold_ms { return; }

            // stop loss: |z| against position direction exceeds stop_sigma
            let against = match pos.side {
                "short" => z.max(0.0), // short loses if z grew further positive
                "long"  => -z.min(0.0),
                _ => 0.0,
            };
            let stop_hit = against >= self.cfg.stop_sigma;
            // exit by mean-revert: |z| crossed back inside exit_sigma band
            let mut revert_hit = z.abs() <= self.cfg.exit_sigma;
            let timeout_hit = elapsed >= self.cfg.max_hold_ms;

            // Asymmetric exit (Flipster T-series trick): on convergence,
            // only close if projected net PnL > 0. Otherwise wait until
            // a real profitable exit (or timeout) — don't lock in the fee
            // hit at break-even.
            if revert_hit && self.cfg.asymmetric_exit {
                let gross_peek = match pos.side {
                    "long"  => (hl_mid - pos.entry_hl_mid) / pos.entry_hl_mid * 1e4,
                    "short" => (pos.entry_hl_mid - hl_mid) / pos.entry_hl_mid * 1e4,
                    _ => 0.0,
                };
                if gross_peek - self.globals.fee_bp <= 0.0 {
                    revert_hit = false;
                }
            }

            let reason: Option<&'static str> =
                if stop_hit { Some("stop") }
                else if revert_hit { Some("revert") }
                else if timeout_hit { Some("timeout") }
                else { None };

            if let Some(reason) = reason {
                self.close(t.base.clone(), pos, hl_mid, spread, reason, now);
            }
            return;
        }

        // ---- No open position: check entry ----
        if std_bp < self.cfg.min_std_bp { return; }
        // Need significant deviation
        let side: Option<&'static str> =
            if z >  self.cfg.entry_sigma { Some("short") } // HL rich → short HL
            else if z < -self.cfg.entry_sigma { Some("long") }
            else { None };
        let side = match side { Some(s) => s, None => return };
        st.open = Some(OpenPos {
            side,
            entry_spread_bp: spread,
            entry_hl_mid: hl_mid,
            entry_time: now,
        });
        self.n_opened += 1;
        debug!(
            "{}: OPEN {} {} z={:.2} std={:.2}bp spread={:.2}bp",
            self.cfg.id, t.base, side, z, std_bp, spread
        );
        // ---- LIVE: fire IOC entry order (fire-and-forget) ----
        if let Some(le) = &self.live_exec {
            self.fire_live(t.base.clone(), side, hl_mid, false, /*reduce_only=*/false, le.clone());
        }
    }

    fn close(
        &mut self,
        base: String,
        pos: OpenPos,
        exit_hl_mid: f64,
        exit_spread_bp: f64,
        reason: &'static str,
        now: DateTime<Utc>,
    ) {
        // Single-leg PnL on HL only, in bp of HL_mid.
        // Long: profit if HL went up. Short: profit if HL went down.
        let gross_bp = match pos.side {
            "long"  => (exit_hl_mid - pos.entry_hl_mid) / pos.entry_hl_mid * 1e4,
            "short" => (pos.entry_hl_mid - exit_hl_mid) / pos.entry_hl_mid * 1e4,
            _ => 0.0,
        };
        let net_bp = gross_bp - self.globals.fee_bp;
        let size_usd = self.cfg.size_usd.unwrap_or(self.globals.size_usd);
        let pnl_usd = size_usd * net_bp / 1e4;
        if let Some(st) = self.states.get_mut(&base) { st.open = None; }
        self.n_closed += 1;
        debug!(
            "{}: CLOSE {} {} reason={} entry={:.4} exit={:.4} gross={:.2}bp net={:.2}bp (${:.2})",
            self.cfg.id, base, pos.side, reason, pos.entry_hl_mid, exit_hl_mid, gross_bp, net_bp, pnl_usd
        );
        let _ = exit_spread_bp;
        // ---- LIVE: fire reduce-only IOC to flatten ----
        if let Some(le) = &self.live_exec {
            // Closing a "long" → SELL; closing a "short" → BUY.
            let opp = match pos.side { "long" => "short", "short" => "long", _ => "long" };
            self.fire_live(base.clone(), opp, exit_hl_mid, true, /*reduce_only=*/true, le.clone());
            le.record_pnl(pnl_usd);
        }
        self.writer.submit(ClosedTrade {
            account_id: self.cfg.id.to_string(),
            symbol: base,
            side: pos.side,
            exit_reason: reason,
            entry_price: pos.entry_hl_mid,
            exit_price: exit_hl_mid,
            size_usd,
            pnl_bp: net_bp,
            entry_time: pos.entry_time,
            exit_time: now,
        });
    }

    /// Fire-and-forget IOC. Does not block paper logic. Failures are logged
    /// but do not affect tracked paper PnL.
    fn fire_live(
        &self,
        coin: String,
        side: &'static str,    // "long"|"short" — DESIRED net position
        ref_mid: f64,
        is_close: bool,
        reduce_only: bool,
        le: Arc<LiveExec>,
    ) {
        let is_buy = side == "long";
        let cushion = 0.005; // 50 bp slippage cushion
        let limit_px = if is_buy { ref_mid * (1.0 + cushion) } else { ref_mid * (1.0 - cushion) };
        // Round to 4 sig digits — HL accepts most price ticks for major coins.
        let limit_px = (limit_px * 10000.0).round() / 10000.0;
        // Variant size $, capped to LiveCfg.max_size_usd
        let variant_size = self.cfg.size_usd.unwrap_or(self.globals.size_usd);
        let live_size_usd = variant_size.min(le.max_size_usd());
        let raw_sz = live_size_usd / ref_mid;
        let sz = match le.round_size(&coin, raw_sz) {
            Some(s) if s > 0.0 => s,
            _ => {
                debug!("LIVE {} {} {}: size {:.6} below precision floor — skip", self.cfg.id, coin, if is_close {"EXIT"} else {"ENTRY"}, raw_sz);
                return;
            }
        };
        // HL minimum order value = $10. Pad to 1.1x to clear it after fill price drift.
        if sz * ref_mid < 11.0 {
            debug!("LIVE {} {}: notional ${:.2} < $11 min — skip", self.cfg.id, coin, sz * ref_mid);
            return;
        }
        let id = self.cfg.id;
        let tag = if is_close { "EXIT" } else { "ENTRY" };
        tokio::spawn(async move {
            match le.place_ioc(&coin, is_buy, limit_px, sz, reduce_only).await {
                Ok((fpx, fsz)) => info!(
                    "LIVE {} {} {} {} sz={:.5} fill_px={:.4}",
                    id, tag, coin, if is_buy {"BUY"} else {"SELL"}, fsz, fpx
                ),
                Err(e) => warn!("LIVE {} {} {} FAIL: {}", id, tag, coin, e),
            }
        });
    }

    pub fn log_summary(&self) {
        let n_open = self.states.values().filter(|s| s.open.is_some()).count();
        info!(
            "{}: tracked={} open={} opened_total={} closed_total={}",
            self.cfg.id, self.states.len(), n_open, self.n_opened, self.n_closed
        );
    }
}

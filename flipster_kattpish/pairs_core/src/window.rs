//! Rolling time window with O(1) running mean / variance.
//!
//! Previously defined inline in `collector::strategies::RollingWindow` and
//! reimplemented as ad-hoc `VecDeque<Sample>` loops in
//! `collector::spread_revert` and `collector::gate_lead`. The shape exposed
//! here matches `strategies::RollingWindow` exactly so its callers migrate
//! without diff.

use chrono::{DateTime, Utc};
use std::collections::VecDeque;

#[derive(Debug)]
pub struct RollingWindow {
    pub pts: VecDeque<(DateTime<Utc>, f64)>,
    pub window_sec: f64,
    pub sum: f64,
    pub sum_sq: f64,
}

impl RollingWindow {
    pub fn new(window_sec: f64) -> Self {
        Self {
            pts: VecDeque::with_capacity(2048),
            window_sec,
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    pub fn push(&mut self, ts: DateTime<Utc>, x: f64) {
        self.pts.push_back((ts, x));
        self.sum += x;
        self.sum_sq += x * x;
        let cutoff_ms = (self.window_sec * 1000.0) as i64;
        while let Some(&(t, v)) = self.pts.front() {
            if (ts - t).num_milliseconds() > cutoff_ms {
                self.pts.pop_front();
                self.sum -= v;
                self.sum_sq -= v * v;
            } else {
                break;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.pts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pts.is_empty()
    }

    pub fn mean(&self) -> Option<f64> {
        if self.pts.is_empty() {
            None
        } else {
            Some(self.sum / self.pts.len() as f64)
        }
    }

    pub fn std(&self) -> Option<f64> {
        let n = self.pts.len();
        if n < 2 {
            return None;
        }
        let m = self.sum / n as f64;
        let var = (self.sum_sq / n as f64 - m * m).max(0.0);
        Some(var.sqrt())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn evicts_old_points() {
        let mut w = RollingWindow::new(1.0);
        let t0 = Utc::now();
        w.push(t0, 10.0);
        w.push(t0 + Duration::milliseconds(500), 20.0);
        assert_eq!(w.len(), 2);
        assert!((w.mean().unwrap() - 15.0).abs() < 1e-9);
        // At ts=1.2s, only the t=500ms point survives (cutoff is anything
        // older than `window_sec`); push lands as the 2nd survivor.
        w.push(t0 + Duration::milliseconds(1_200), 30.0);
        assert_eq!(w.len(), 2);
        assert!((w.mean().unwrap() - 25.0).abs() < 1e-9);
        // Jumping further out evicts everything but the freshly-pushed point.
        w.push(t0 + Duration::milliseconds(5_000), 40.0);
        assert_eq!(w.len(), 1);
        assert!((w.mean().unwrap() - 40.0).abs() < 1e-9);
    }
}

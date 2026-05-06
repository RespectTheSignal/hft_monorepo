//! Exponentially-weighted mean & variance for the spread time series.
//! Tau = decay constant in seconds. Update with `(value, dt_secs)`.

#[derive(Debug, Clone)]
pub struct Ewma {
    tau_sec: f64,
    pub mean: f64,
    pub var: f64,
    initialized: bool,
    pub n_updates: u64,
}

impl Ewma {
    pub fn new(tau_sec: f64) -> Self {
        Self { tau_sec, mean: 0.0, var: 0.0, initialized: false, n_updates: 0 }
    }

    pub fn update(&mut self, value: f64, dt_secs: f64) {
        self.n_updates += 1;
        if !self.initialized {
            self.mean = value;
            self.var = 0.0;
            self.initialized = true;
            return;
        }
        let alpha = 1.0 - (-dt_secs / self.tau_sec).exp();
        let alpha = alpha.clamp(1e-6, 1.0);
        let diff = value - self.mean;
        self.mean += alpha * diff;
        self.var = (1.0 - alpha) * (self.var + alpha * diff * diff);
    }

    pub fn std(&self) -> f64 {
        self.var.max(0.0).sqrt()
    }

    pub fn z(&self, value: f64) -> Option<f64> {
        let s = self.std();
        if !self.initialized || s < 1e-9 || self.n_updates < 5 {
            None
        } else {
            Some((value - self.mean) / s)
        }
    }
}

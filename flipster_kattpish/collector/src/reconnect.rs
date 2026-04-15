use rand::Rng;
use std::time::Duration;

/// Exponential backoff with jitter, capped at `max_secs` seconds.
pub struct Backoff {
    attempt: u32,
    max_secs: u64,
}

impl Backoff {
    pub fn new(max_secs: u64) -> Self {
        Self { attempt: 0, max_secs }
    }
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
    pub fn next_delay(&mut self) -> Duration {
        self.attempt = (self.attempt + 1).min(12);
        let base = 2u64.saturating_pow(self.attempt).min(self.max_secs);
        // up to 1s of jitter so simultaneous reconnect attempts spread out
        let jitter_ms: u64 = rand::thread_rng().gen_range(0..1000);
        Duration::from_millis(base * 1000 + jitter_ms)
    }
}

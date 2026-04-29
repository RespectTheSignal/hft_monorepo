// Process-wide Flipster cookie store.
// Initialized at startup by the runner; consumed by order_manager_client when
// issuing HTTP requests to Flipster.

use anyhow::Result;
use arc_swap::ArcSwap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub struct FlipsterCookieStore {
    cookies: ArcSwap<Vec<(String, String)>>,
    cdp_port: u16,
}

impl FlipsterCookieStore {
    pub fn new(cdp_port: u16, initial: Vec<(String, String)>) -> Arc<Self> {
        Arc::new(Self {
            cookies: ArcSwap::from_pointee(initial),
            cdp_port,
        })
    }

    pub fn cookies(&self) -> Vec<(String, String)> {
        (**self.cookies.load()).clone()
    }

    pub fn cookie_header(&self) -> String {
        self.cookies
            .load()
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn cdp_port(&self) -> u16 {
        self.cdp_port
    }

    /// Refresh cookies via CDP and replace the current store atomically.
    pub async fn refresh(&self) -> Result<usize> {
        let fresh = crate::flipster_cookies::fetch_flipster_cookies(self.cdp_port).await?;
        let n = fresh.len();
        self.cookies.store(Arc::new(fresh));
        Ok(n)
    }
}

static GLOBAL: OnceLock<Arc<FlipsterCookieStore>> = OnceLock::new();

/// Initialize the global cookie store by fetching from CDP. Safe to call multiple times;
/// subsequent calls are no-ops.
pub async fn init_global(cdp_port: u16) -> Result<Arc<FlipsterCookieStore>> {
    if let Some(s) = GLOBAL.get() {
        return Ok(s.clone());
    }
    let initial = crate::flipster_cookies::fetch_flipster_cookies(cdp_port).await?;
    let store = FlipsterCookieStore::new(cdp_port, initial);
    let _ = GLOBAL.set(store.clone()); // ignore if race set it first
    Ok(GLOBAL.get().unwrap().clone())
}

pub fn global() -> Option<Arc<FlipsterCookieStore>> {
    GLOBAL.get().cloned()
}

/// Spawn a background task that refreshes cookies every `interval_secs`.
pub fn spawn_refresher(rt: tokio::runtime::Handle, interval_secs: u64) {
    let Some(store) = global() else {
        log::warn!("[cookie-refresher] global store not initialized, skipping");
        return;
    };
    rt.spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
            match store.refresh().await {
                Ok(n) => log::info!("[cookies] refreshed {} cookies", n),
                Err(e) => log::warn!("[cookies] refresh failed: {}", e),
            }
        }
    });
}

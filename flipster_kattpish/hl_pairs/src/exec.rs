//! Live execution against Hyperliquid via `hyperliquid_rust_sdk`.
//!
//! Uses an **agent wallet** (api wallet) — not the master wallet. The
//! agent's private key only allows trading; a stolen key cannot withdraw
//! funds. Set `HLP_AGENT_KEY=<0x…>` in `.env` (NEVER commit it).
//!
//! Operating mode:
//!   - Entry (paper signal fires): place an `Alo` (Add-Liquidity-Only) limit
//!     at the best opposing-side price. Alo rejects if it would take, so we
//!     are guaranteed maker-only fills (= -0.5 bp rebate, not 4.5 bp taker).
//!   - Exit: same — place an opposing Alo at best price.
//!   - Cancel timeout: if not filled within `entry_fill_timeout_ms`, cancel
//!     and skip the trade.
//!   - Safety: per-trade size cap, daily loss cap, hard variant disable on
//!     `n` consecutive losses.

use anyhow::{bail, Context, Result};
use ethers::signers::LocalWallet;
use hyperliquid_rust_sdk::{
    BaseUrl, ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest,
    ExchangeClient, ExchangeDataStatus, ExchangeResponseStatus, InfoClient,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct LiveCfg {
    /// 0x… 64-char hex private key of the agent wallet.
    pub agent_key: String,
    /// `true` → testnet (no real funds). `false` → mainnet.
    pub testnet: bool,
    /// Hard cap on per-trade USD notional. Acts as the last-line-of-defence
    /// override of variant-level `size_usd`.
    pub max_size_usd: f64,
    /// Stop the live executor for the rest of the day if cumulative paper
    /// (= projected) loss exceeds this in USD.
    pub daily_loss_cap_usd: f64,
}

impl LiveCfg {
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("HLP_AGENT_KEY").ok()?;
        if key.is_empty() {
            return None;
        }
        Some(Self {
            agent_key: key,
            testnet: std::env::var("HLP_TESTNET")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(true), // default = testnet for safety
            max_size_usd: std::env::var("HLP_MAX_SIZE_USD")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(50.0),
            daily_loss_cap_usd: std::env::var("HLP_DAILY_LOSS_CAP_USD")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(20.0),
        })
    }
}

#[derive(Debug)]
struct State {
    realized_loss_usd: f64,
    killed: bool,
}

pub struct LiveExec {
    cfg: LiveCfg,
    exchange: ExchangeClient,
    state: Arc<Mutex<State>>,
    /// Per-coin size precision (decimal places). HL rejects orders that
    /// don't match `szDecimals` from /info/meta.
    sz_decimals: HashMap<String, u32>,
}

impl LiveExec {
    pub async fn new(cfg: LiveCfg) -> Result<Self> {
        let key = cfg.agent_key.trim_start_matches("0x");
        let wallet = LocalWallet::from_str(key)
            .context("invalid HLP_AGENT_KEY (expect 0x-prefixed 64-char hex)")?;
        let base = if cfg.testnet { BaseUrl::Testnet } else { BaseUrl::Mainnet };
        info!("LiveExec connecting (network={})", if cfg.testnet { "TESTNET" } else { "MAINNET" });
        let exchange = ExchangeClient::new(None, wallet, Some(base), None, None)
            .await
            .context("ExchangeClient init")?;

        // Fetch szDecimals per coin from HL meta (raw POST — SDK doesn't expose it).
        let sz_decimals = fetch_sz_decimals(cfg.testnet).await
            .context("fetch szDecimals")?;
        info!("LiveExec ready: {} coins szDecimals loaded", sz_decimals.len());

        Ok(Self {
            cfg,
            exchange,
            state: Arc::new(Mutex::new(State { realized_loss_usd: 0.0, killed: false })),
            sz_decimals,
        })
    }

    /// Round size to the coin's allowed precision. Returns None if too small.
    pub fn round_size(&self, coin: &str, sz: f64) -> Option<f64> {
        let dp = self.sz_decimals.get(coin).copied().unwrap_or(0);
        let mult = 10f64.powi(dp as i32);
        let rounded = (sz * mult).round() / mult;
        if rounded <= 0.0 { None } else { Some(rounded) }
    }

    pub fn killed(&self) -> bool {
        self.state.lock().killed
    }

    pub fn max_size_usd(&self) -> f64 { self.cfg.max_size_usd }

    /// Record realized PnL. Trips the kill switch when daily loss exceeds cap.
    pub fn record_pnl(&self, pnl_usd: f64) {
        let mut s = self.state.lock();
        if pnl_usd < 0.0 {
            s.realized_loss_usd += -pnl_usd;
            if s.realized_loss_usd >= self.cfg.daily_loss_cap_usd && !s.killed {
                s.killed = true;
                warn!("LIVE KILL SWITCH: realized_loss=${:.2} >= cap=${:.2}",
                    s.realized_loss_usd, self.cfg.daily_loss_cap_usd);
            }
        }
    }

    /// Place a maker-only (Alo) limit. `coin` = HL asset name (e.g. "BTC").
    /// `sz` = base coin units (NOT USD). Returns the resting order id, or
    /// Err if rejected/filled-as-taker.
    pub async fn place_alo(
        &self,
        coin: &str,
        is_buy: bool,
        limit_px: f64,
        sz: f64,
        reduce_only: bool,
    ) -> Result<u64> {
        if self.killed() {
            bail!("kill switch active — order refused");
        }
        let order = ClientOrderRequest {
            asset: coin.to_string(),
            is_buy,
            reduce_only,
            limit_px,
            sz,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit { tif: "Alo".to_string() }),
        };
        let resp = self.exchange.order(order, None).await
            .context("exchange.order")?;
        let inner = match resp {
            ExchangeResponseStatus::Ok(r) => r,
            ExchangeResponseStatus::Err(e) => bail!("exchange err: {e}"),
        };
        let status = inner.data
            .ok_or_else(|| anyhow::anyhow!("no data in response"))?
            .statuses
            .into_iter().next()
            .ok_or_else(|| anyhow::anyhow!("no status"))?;
        match status {
            ExchangeDataStatus::Resting(o) => Ok(o.oid),
            // Alo should never fill as taker, but the SDK reports "Filled"
            // if HL accepted it as maker that immediately matched at our
            // price — treat as success.
            ExchangeDataStatus::Filled(o) => Ok(o.oid),
            other => bail!("alo rejected: {other:?}"),
        }
    }

    /// Place a marketable IOC limit (taker, instant fill or cancel).
    /// `limit_px` should already include slippage cushion (e.g. ±50 bp).
    /// Returns (fill_price, fill_sz). Errors if not filled at all.
    pub async fn place_ioc(
        &self,
        coin: &str,
        is_buy: bool,
        limit_px: f64,
        sz: f64,
        reduce_only: bool,
    ) -> Result<(f64, f64)> {
        if self.killed() {
            bail!("kill switch active — order refused");
        }
        let order = ClientOrderRequest {
            asset: coin.to_string(),
            is_buy,
            reduce_only,
            limit_px,
            sz,
            cloid: None,
            order_type: ClientOrder::Limit(ClientLimit { tif: "Ioc".to_string() }),
        };
        let resp = self.exchange.order(order, None).await
            .context("exchange.order(ioc)")?;
        let inner = match resp {
            ExchangeResponseStatus::Ok(r) => r,
            ExchangeResponseStatus::Err(e) => bail!("exchange err: {e}"),
        };
        let status = inner.data
            .ok_or_else(|| anyhow::anyhow!("no data in response"))?
            .statuses
            .into_iter().next()
            .ok_or_else(|| anyhow::anyhow!("no status"))?;
        match status {
            ExchangeDataStatus::Filled(o) => {
                let fpx: f64 = o.avg_px.parse().unwrap_or(limit_px);
                let fsz: f64 = o.total_sz.parse().unwrap_or(sz);
                Ok((fpx, fsz))
            }
            other => bail!("ioc not filled: {other:?}"),
        }
    }

    /// Set leverage for a coin (cross by default). HL clamps to the
    /// per-coin max so passing 50 just rounds down to whatever HL allows.
    pub async fn set_leverage(&self, coin: &str, leverage: u32, is_cross: bool) -> Result<()> {
        let resp = self.exchange.update_leverage(leverage, coin, is_cross, None).await
            .context("update_leverage")?;
        match resp {
            ExchangeResponseStatus::Ok(_) => Ok(()),
            ExchangeResponseStatus::Err(e) => bail!("update_leverage err: {e}"),
        }
    }

    /// Apply max leverage from HL meta to a list of coins.
    /// Returns count of successful updates.
    pub async fn set_max_leverage_for(&self, coins: &[&str]) -> usize {
        let url = if self.cfg.testnet {
            "https://api.hyperliquid-testnet.xyz/info"
        } else {
            "https://api.hyperliquid.xyz/info"
        };
        let body = serde_json::json!({"type":"meta"});
        let client = reqwest::Client::new();
        let v: serde_json::Value = match client.post(url).json(&body).send().await {
            Ok(r) => match r.json().await { Ok(v) => v, Err(_) => return 0 },
            Err(_) => return 0,
        };
        let universe = match v.get("universe").and_then(|x| x.as_array()) {
            Some(u) => u, None => return 0,
        };
        let mut max_by_coin = std::collections::HashMap::new();
        for c in universe {
            let name = c.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let lev = c.get("maxLeverage").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            if !name.is_empty() && lev > 0 {
                max_by_coin.insert(name.to_string(), lev);
            }
        }
        let mut ok = 0;
        for &coin in coins {
            let lev = match max_by_coin.get(coin) { Some(&v) => v, None => continue };
            match self.set_leverage(coin, lev, true).await {
                Ok(()) => { ok += 1; info!("leverage {} → {}x cross", coin, lev); }
                Err(e) => warn!("leverage {} fail: {}", coin, e),
            }
        }
        ok
    }

    pub async fn cancel(&self, coin: &str, oid: u64) -> Result<()> {
        let req = ClientCancelRequest { asset: coin.to_string(), oid };
        let resp = self.exchange.cancel(req, None).await
            .context("exchange.cancel")?;
        match resp {
            ExchangeResponseStatus::Ok(_) => Ok(()),
            ExchangeResponseStatus::Err(e) => bail!("cancel err: {e}"),
        }
    }
}

/// Fetch HL meta and return {coin: szDecimals}. Used to round order sizes
/// to each coin's allowed precision.
async fn fetch_sz_decimals(testnet: bool) -> Result<HashMap<String, u32>> {
    let url = if testnet {
        "https://api.hyperliquid-testnet.xyz/info"
    } else {
        "https://api.hyperliquid.xyz/info"
    };
    let body = serde_json::json!({"type":"meta"});
    let client = reqwest::Client::new();
    let v: serde_json::Value = client.post(url).json(&body).send().await?.json().await?;
    let universe = v.get("universe").and_then(|x| x.as_array())
        .ok_or_else(|| anyhow::anyhow!("no universe"))?;
    let mut out = HashMap::new();
    for c in universe {
        let name = c.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let dp = c.get("szDecimals").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        if !name.is_empty() {
            out.insert(name.to_string(), dp);
        }
    }
    Ok(out)
}

/// Read the wallet/agent address derived from HLP_AGENT_KEY (for logging).
pub fn agent_address(key_hex: &str) -> Result<String> {
    let key = key_hex.trim_start_matches("0x");
    let wallet = LocalWallet::from_str(key)?;
    Ok(format!("0x{:x}", ethers::signers::Signer::address(&wallet)))
}

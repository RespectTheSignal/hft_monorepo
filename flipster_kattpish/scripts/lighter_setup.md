# Lighter onboarding

Lighter is a zk-rollup CLOB perp DEX with **0 maker / 0 taker fees** on Standard accounts. To trade you need:

1. An L1 (Ethereum) wallet with USDC
2. A Lighter account registered with that wallet
3. API keys derived from the wallet (40-byte ECgFp5 private keys)

## Step 1 — register on Lighter

1. Visit https://app.lighter.xyz with MetaMask connected
2. Sign the registration message
3. Bridge USDC from L1 to Lighter (start small, $100-500 for testing). Bridge gas: $5-15 on Ethereum mainnet, less on L2s.

## Step 2 — generate API keys

The Python SDK has a `system_setup.py` example that generates 5 API keys and registers them.

```bash
cd /tmp
git clone --depth 1 https://github.com/elliottech/lighter-python.git
cd lighter-python
pip install -e .
pip install eth_account
cd examples
# Edit system_setup.py:
#   ETH_PRIVATE_KEY = "<your L1 wallet private key>"
#   BASE_URL = "https://mainnet.zklighter.elliot.ai"   (or testnet)
python system_setup.py
```

This creates `api_key_config.json` in the cwd. **Move it to** `~/.config/flipster_kattpish/lighter_keys.json` and `chmod 600`.

## Step 3 — start the executor

```bash
cd /home/gate1/projects/quant/hft_monorepo/flipster_kattpish

# DRY RUN (paper, no funds needed) — uses Lighter's PaperClient
LIGHTER_DRY_RUN=1 python3 executor/python/lighter_executor.py

# LIVE
LIGHTER_DRY_RUN=0 \
  LIGHTER_API_KEY_CONFIG=~/.config/flipster_kattpish/lighter_keys.json \
  LIGHTER_VARIANT=LH_LIVE_v1 \
  LIGHTER_SIZE_USD=20 \
  python3 executor/python/lighter_executor.py
```

## Step 4 — wire signal source

The executor subscribes to the same ZMQ signal socket BingX uses by default
(`ipc:///tmp/flipster_kattpish_signal_live.sock`). To send Lighter-only
signals from a strategy, add a `lighter_lead` variant in `bingx_lead.rs` /
strategy code with `account_id = "LH_LIVE_v1"`.

For first paper test you can reuse the BingX strategy by setting
`LIGHTER_VARIANT=BX_LIVE_v1` (it'll fire on every BingX signal, in parallel).

## Caveats

- Standard accounts have **300ms taker / 200ms maker** latency. If you need
  faster, opt into Premium ($LIT staking required).
- Lighter `base_amount` is integer steps per market — we currently hardcode
  10^4 scale. For low-priced alts (e.g. SHIB) this may be wrong; fetch the
  per-market `base_amount_decimal` from `/api/v1/orderBookDetails`.
- Withdrawals from Lighter back to L1 cost gas on the destination chain.
- API key file is sensitive — `chmod 600` and never commit.

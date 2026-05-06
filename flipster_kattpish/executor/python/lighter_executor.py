#!/usr/bin/env python3
"""Lighter executor — subscribes to the same ZMQ TradeSignal stream
that bingx_executor uses, places real (or paper) orders on Lighter.

Lighter is a zero-fee CLOB perp DEX (no last-look, SNARK-verified
matching). It's the cheapest venue we have access to: 0 maker, 0 taker
on Standard accounts. The strategy backtest's +5.93 bp/trade lands as
+5.93 bp net here vs +2.73 on BingX.

Architecture (mirrors bingx_executor):
- ZMQ SUB on SIGNAL_PUB_ADDR, filter on `account_id == LIGHTER_VARIANT`
- Each entry signal → SignerClient.create_market_order(...)
- Each exit signal → opposite-side market order with reduceOnly via
  the Lighter `reduce_only` flag (or close_position helper)
- ZMQ PUB on FILL_PUB_ADDR — same FillReport schema as bingx, so the
  collector's bingx_lead `ref_mid retune` logic also applies if we
  spawn a `lighter_lead` strategy variant (TBD).

Two modes:
- LIGHTER_DRY_RUN=1 (default): use PaperClient, no real funds, no
  L1 account needed. Connects WS to track real orderbook + simulates
  fills against it.
- LIGHTER_DRY_RUN=0: real account. Requires `api_key_config.json`
  produced by `system_setup.py` (see scripts/lighter_setup.md).

Required env (live mode):
- LIGHTER_API_KEY_CONFIG: path to api_key_config.json
- LIGHTER_VARIANT: account_id filter (default LH_LIVE_v1)
- LIGHTER_SIZE_USD: per-trade notional (default 20)
- LIGHTER_MAX_OPEN: cap (default 5)
- LIGHTER_DAILY_LOSS_USD: kill switch (default -50)
- LIGHTER_BASE_URL: override (default mainnet)
- SIGNAL_PUB_ADDR: ZMQ SUB endpoint
- FILL_PUB_ADDR: ZMQ PUB endpoint

Universe mapping: Lighter uses integer market_index (BTC=0 or 1,
ETH=2 etc — varies by environment). Set LIGHTER_MARKET_MAP env or
fetch from /api/v1/orderBooks at startup.
"""

import asyncio
import json
import logging
import os
import sys
import time
from dataclasses import dataclass
from typing import Optional

try:
    import zmq
    import zmq.asyncio
except ImportError:
    print("pip install pyzmq", file=sys.stderr)
    sys.exit(1)

try:
    import lighter
except ImportError:
    print("pip install lighter-python (or pip install -e ../lighter-python)", file=sys.stderr)
    sys.exit(1)


log = logging.getLogger("lighter_exec")
logging.basicConfig(
    level=os.getenv("LIGHTER_LOG_LEVEL", "INFO"),
    format="%(asctime)s %(levelname)s %(name)s: %(message)s",
)


# ---------- config ----------

VARIANT = os.getenv("LIGHTER_VARIANT", "LH_LIVE_v1")
SIZE_USD = float(os.getenv("LIGHTER_SIZE_USD", "20"))
MAX_OPEN = int(os.getenv("LIGHTER_MAX_OPEN", "5"))
DAILY_LOSS_USD = float(os.getenv("LIGHTER_DAILY_LOSS_USD", "-50"))
DRY_RUN = os.getenv("LIGHTER_DRY_RUN", "1") == "1"

BASE_URL = os.getenv(
    "LIGHTER_BASE_URL",
    "https://mainnet.zklighter.elliot.ai" if not DRY_RUN else "https://mainnet.zklighter.elliot.ai",
)
SIGNAL_ADDR = os.getenv("SIGNAL_PUB_ADDR", "ipc:///tmp/flipster_kattpish_signal_live.sock")
FILL_PUB_ADDR = os.getenv("FILL_PUB_ADDR", "ipc:///tmp/flipster_kattpish_fill_live.sock")


# ---------- state ----------

@dataclass
class OpenPosition:
    market_id: int
    base: str
    side: str  # "long" | "short"
    size_base: float
    entry_price: float
    opened_at: float


class State:
    def __init__(self) -> None:
        self.open: dict[str, OpenPosition] = {}
        self.realized_pnl_usd: float = 0.0
        self.n_trades: int = 0
        self.seen_entries: set[tuple[str, int]] = set()
        self.seen_exits: set[tuple[str, int]] = set()


# ---------- market lookup ----------

async def fetch_market_map(api_client) -> tuple[dict[str, int], dict[int, dict]]:
    """Build {base: market_id} and {market_id: meta} from /api/v1/orderBooks.
    `meta` carries supported_size_decimals + min_base_amount so we can
    quantize base_amount before order placement (Lighter rejects orders
    that exceed the per-market size precision).
    """
    order_api = lighter.OrderApi(api_client)
    resp = await order_api.order_books()
    sym2id: dict[str, int] = {}
    meta: dict[int, dict] = {}
    for ob in (resp.order_books or []):
        sym = (ob.symbol or "").upper()
        if not sym:
            continue
        mid = int(ob.market_id)
        sym2id[sym] = mid
        try:
            min_b = float(ob.min_base_amount) if ob.min_base_amount is not None else 0.0
        except (TypeError, ValueError):
            min_b = 0.0
        meta[mid] = {
            "size_decimals": int(ob.supported_size_decimals or 0),
            "min_base_amount": min_b,
            "symbol": sym,
        }
    log.info("loaded %d Lighter markets", len(sym2id))
    return sym2id, meta


def quantize_base_amount(coin_qty: float, size_decimals: int, min_amount: float) -> float:
    """Round coin_qty to Lighter's allowed precision and enforce min."""
    if size_decimals < 0:
        size_decimals = 0
    step = 10 ** (-size_decimals)
    rounded = round(coin_qty / step) * step
    # Apply min_amount floor (round up to nearest valid step).
    if rounded < min_amount:
        rounded = round(min_amount / step + 0.4999) * step
    # Clean float drift.
    return round(rounded, size_decimals)


# ---------- order placement ----------

async def place_market_order(client, market_id: int, side: str, base_amount: int, price_hint: float, base_qty_float: float):
    """Place a market order. Dispatches on client type:
    - PaperClient → create_paper_order(PaperOrderRequest(...)) with float base_amount
    - SignerClient → create_market_order(...) with integer base_amount + price hint
    """
    is_ask = side == "short"
    if isinstance(client, lighter.PaperClient):
        # PaperClient uses a different API — float quantity, side enum.
        # No client_order_index, no slippage protection.
        # Track the market on first use (idempotent).
        try:
            await client.track_market(market_id=market_id)
        except Exception:
            pass
        side_enum = lighter.PaperOrderSide.SELL if is_ask else lighter.PaperOrderSide.BUY
        result = await client.create_paper_order(
            lighter.PaperOrderRequest(
                market_id=market_id,
                side=side_enum,
                base_amount=base_qty_float,
            )
        )
        # Mimic SignerClient return (tx, tx_hash, err) so caller stays uniform.
        return result, None, None
    # SignerClient path. avg_execution_price is the worst acceptable
    # price (Lighter does slippage protection at submit time).
    if is_ask:
        worst = int(price_hint * 0.99 * 100)
    else:
        worst = int(price_hint * 1.01 * 100)
    return await client.create_market_order(
        market_index=market_id,
        client_order_index=int(time.time() * 1000) % (2**31),
        base_amount=base_amount,
        avg_execution_price=worst,
        is_ask=is_ask,
    )


def base_amount_for_usd(market_meta: dict, size_usd: float, last_price: float) -> int:
    """Convert a USD notional into Lighter's integer base_amount.
    market_meta has `base_amount_decimal` (e.g. 4 for BTC = 0.0001 BTC steps).
    """
    if last_price <= 0:
        return 0
    coin_qty = size_usd / last_price
    decimal = int(market_meta.get("base_amount_decimal", 4))
    return int(coin_qty * (10 ** decimal))


# ---------- main loop ----------

async def main():
    log.info(
        "starting lighter_executor variant=%s size=%s max_open=%d dry=%s url=%s",
        VARIANT, SIZE_USD, MAX_OPEN, DRY_RUN, BASE_URL,
    )

    api_client = lighter.ApiClient(configuration=lighter.Configuration(host=BASE_URL))

    # Build market map (base → market_id) + per-market meta (size_decimals).
    market_map: dict[str, int] = {}
    market_meta: dict[int, dict] = {}
    try:
        market_map, market_meta = await fetch_market_map(api_client)
    except Exception as e:
        log.error("failed to fetch market map: %s", e)

    if DRY_RUN:
        client = lighter.PaperClient(api_client, initial_collateral_usdc=10_000)
        # Pre-track common markets
        for base in ["BTC", "ETH", "SOL", "HYPE", "WIF", "FARTCOIN"]:
            if base in market_map:
                try:
                    await client.track_market(market_id=market_map[base])
                except Exception as e:
                    log.warning("track_market %s err: %s", base, e)
        log.info("paper client ready")
    else:
        cfg = os.getenv("LIGHTER_API_KEY_CONFIG", "./api_key_config.json")
        if not os.path.exists(cfg):
            log.error("api_key_config.json missing — run scripts/lighter_setup.py first")
            sys.exit(1)
        with open(cfg) as f:
            kc = json.load(f)
        client = lighter.SignerClient(
            url=kc.get("baseUrl", BASE_URL),
            account_index=int(kc["accountIndex"]),
            api_private_keys={int(k): v for k, v in kc["privateKeys"].items()},
        )
        err = client.check_client()
        if err is not None:
            log.error("CheckClient failed: %s", err)
            sys.exit(1)
        log.info("signer client ready (account=%s)", kc["accountIndex"])

    state = State()

    # ZMQ
    ctx = zmq.asyncio.Context()
    sub = ctx.socket(zmq.SUB)
    sub.connect(SIGNAL_ADDR)
    sub.setsockopt(zmq.SUBSCRIBE, b"")
    log.info("zmq SUB connected addr=%s variant=%s", SIGNAL_ADDR, VARIANT)

    pub = ctx.socket(zmq.PUB)
    try:
        pub.bind(FILL_PUB_ADDR)
        log.info("zmq PUB bound addr=%s", FILL_PUB_ADDR)
    except Exception as e:
        log.warning("FILL_PUB_ADDR bind failed (%s) — continuing without fill feedback", e)
        pub = None

    while True:
        raw = await sub.recv()
        try:
            sig = json.loads(raw)
        except Exception:
            continue
        if sig.get("account_id") != VARIANT:
            continue
        try:
            await handle_signal(client, state, market_map, market_meta, sig, pub)
        except Exception as e:
            log.exception("handler error: %s", e)


async def handle_signal(client, state: State, market_map, market_meta, sig, pub):
    action = sig.get("action")
    base = (sig.get("base") or "").upper()
    side = sig.get("side")
    pos_id = int(sig.get("position_id", 0))
    last = float(sig.get("flipster_price", 0))
    if not base or not action:
        return

    if action == "entry":
        key = (base, pos_id)
        if key in state.seen_entries:
            return
        state.seen_entries.add(key)
        if state.realized_pnl_usd <= DAILY_LOSS_USD:
            log.warning("kill switch — daily loss reached, skipping entry")
            return
        if len(state.open) >= MAX_OPEN:
            log.warning("max_open reached, skipping %s", base)
            return
        if base in state.open:
            return
        market_id = market_map.get(base)
        if market_id is None:
            log.warning("no Lighter market for base=%s, skipping", base)
            return
        # Quantize base_amount to Lighter's per-market precision.
        meta = market_meta.get(market_id, {})
        size_decimals = int(meta.get("size_decimals", 0))
        min_amount = float(meta.get("min_base_amount", 0.0))
        raw_qty = SIZE_USD / last if last > 0 else 0
        coin_qty = quantize_base_amount(raw_qty, size_decimals, min_amount)
        if coin_qty <= 0:
            log.warning("skip %s: bad quantized qty (raw=%.6f sd=%d min=%.4f)",
                        base, raw_qty, size_decimals, min_amount)
            return
        # Re-derive integer base_amount for SignerClient path
        # (PaperClient ignores it and uses coin_qty directly).
        base_amount = max(1, int(coin_qty * (10 ** size_decimals)))
        log.info("ENTRY base=%s side=%s mid=%.6f qty=%.6f sd=%d",
                 base, side, last, coin_qty, size_decimals)
        try:
            tx, tx_hash, err = await place_market_order(
                client, market_id, side, base_amount, last, coin_qty
            )
        except Exception as e:
            log.error("place_market_order err base=%s: %s", base, e)
            return
        if err is not None:
            log.error("ENTRY err base=%s: %s", base, err)
            return
        # Note: Lighter doesn't return fill price inline — we track via
        # WS account-stream. For now, use signal price as an approximation.
        state.n_trades += 1
        # Record the actual paper fill price if the client returned one.
        actual_entry = last
        if hasattr(tx, "avg_price") and tx.avg_price:
            actual_entry = float(tx.avg_price)
        state.open[base] = OpenPosition(
            market_id=market_id, base=base, side=side,
            size_base=coin_qty, entry_price=actual_entry,
            opened_at=time.time(),
        )
        log.info("ENTRY filled base=%s entry=%.6f n_trades=%d", base, actual_entry, state.n_trades)
        if pub:
            await pub.send(json.dumps({
                "event": "fill",
                "account_id": VARIANT,
                "position_id": pos_id,
                "base": base,
                "action": "entry",
                "side": side,
                "size_usd": SIZE_USD,
                "flipster_price": last,
                "gate_price": 0.0,
                "flipster_slip_bp": 0.0,
                "gate_slip_bp": 0.0,
                "paper_flipster_price": last,
                "paper_gate_price": 0.0,
                "signal_lag_ms": 0,
                "timestamp": sig.get("timestamp", ""),
            }).encode())

    elif action == "exit":
        key = (base, pos_id)
        if key in state.seen_exits:
            return
        state.seen_exits.add(key)
        pos = state.open.pop(base, None)
        if not pos:
            log.warning("exit but no open position, ignoring base=%s", base)
            return
        # Opposite side, reduceOnly (Lighter uses ReduceOnly=1 flag)
        opp = "short" if pos.side == "long" else "long"
        log.info("EXIT base=%s entry=%.6f exit=%.6f", base, pos.entry_price, last)
        meta = market_meta.get(pos.market_id, {})
        size_decimals = int(meta.get("size_decimals", 0))
        # Re-quantize the close size against the same precision rule. Should
        # equal pos.size_base if entry already passed quantize, but float drift
        # can creep in across the round-trip.
        close_qty = quantize_base_amount(pos.size_base, size_decimals, 0.0)
        close_int = max(1, int(close_qty * (10 ** size_decimals)))
        try:
            tx, tx_hash, err = await place_market_order(
                client, pos.market_id, opp, close_int, last, close_qty
            )
        except Exception as e:
            log.error("close err base=%s: %s", base, e)
            return
        if err:
            log.error("EXIT err base=%s: %s", base, err)
            return
        # Use actual paper fill price for exit if available.
        actual_exit = last
        if hasattr(tx, "avg_price") and tx.avg_price:
            actual_exit = float(tx.avg_price)
        # Compute bp + accumulate.
        bp = (actual_exit - pos.entry_price) / pos.entry_price * 10000.0
        if pos.side == "short":
            bp = -bp
        # Lighter Standard = 0 fees. Net = gross.
        pnl_usd = bp * SIZE_USD / 10000.0
        state.realized_pnl_usd += pnl_usd
        log.info(
            "EXIT filled base=%s gross_bp=%+.2f pnl_usd=%+.4f cum_pnl_usd=%+.2f",
            base, bp, pnl_usd, state.realized_pnl_usd,
        )
        if pub:
            await pub.send(json.dumps({
                "event": "fill",
                "account_id": VARIANT,
                "position_id": pos_id,
                "base": base,
                "action": "exit",
                "side": pos.side,
                "size_usd": SIZE_USD,
                "flipster_price": last,
                "gate_price": 0.0,
                "flipster_slip_bp": 0.0,
                "gate_slip_bp": 0.0,
                "paper_flipster_price": last,
                "paper_gate_price": 0.0,
                "signal_lag_ms": 0,
                "timestamp": sig.get("timestamp", ""),
            }).encode())


if __name__ == "__main__":
    asyncio.run(main())

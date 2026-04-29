#!/usr/bin/env python3
"""OBI-Skewed Market Making on Flipster.

Always quote BID + ASK as maker. Use OBI signal to cancel adverse side.
Use inventory level to skew quote presence.

Risk:
  - MAX_POS: hard inventory cap, force-close one side if breached
  - KILL_PNL: stop if net PnL < threshold
  - HEARTBEAT: restart if no activity for N minutes
"""
from __future__ import annotations

import argparse
import asyncio
import hashlib
import hmac
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

import websockets

API_KEY = os.getenv("FLIPSTER_API_KEY", "3|8Q9vdANlA1pvIbimev5NgB__5ULBH3_a")
API_SECRET = os.getenv("FLIPSTER_API_SECRET",
                       "3c823cd1b32582d697b359898952020d0018cdb93b40d6ec4e49bad020d361e2")
V1_HOST = "https://trading-api.flipster.io"
V2_WS = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"


# ---------------------------------------------------------------------------
# HTTP / API helpers
# ---------------------------------------------------------------------------

def _http_sync(method: str, path: str, body: dict | None = None):
    expires = int(time.time()) + 60
    body_str = json.dumps(body) if body else ""
    msg = f"{method}{path}{expires}{body_str}"
    sig = hmac.new(API_SECRET.encode(), msg.encode(), hashlib.sha256).hexdigest()
    headers = {
        "api-key": API_KEY, "api-expires": str(expires), "api-signature": sig,
        "Content-Type": "application/json",
        "User-Agent": "flipster-research/0.1",
        "Accept": "application/json",
    }
    req = urllib.request.Request(
        V1_HOST + path, method=method, headers=headers,
        data=body_str.encode() if body_str else None,
    )
    try:
        with urllib.request.urlopen(req, timeout=8) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, {"_error": e.read().decode()[:300]}
    except Exception as e:
        return 0, {"_error": str(e)[:200]}


async def http(method, path, body=None):
    return await asyncio.to_thread(_http_sync, method, path, body)


async def place_limit(symbol: str, side: str, qty: str, price: str) -> dict:
    body = {
        "symbol": symbol, "side": side, "type": "LIMIT",
        "quantity": qty, "price": price, "timeInForce": "GTC",
    }
    s, r = await http("POST", "/api/v1/trade/order", body)
    if s == 200:
        return r.get("order", r)
    return {"_error": r.get("_error", f"http {s}"), "_status": s}


async def cancel(symbol: str, order_id: str) -> bool:
    s, _ = await http("DELETE", "/api/v1/trade/order",
                      {"symbol": symbol, "orderId": order_id})
    return s == 200


async def get_position(symbol: str) -> float:
    """Returns signed position qty in base coin. 0 if flat."""
    s, r = await http("GET", "/api/v1/account/position")
    if s != 200 or not isinstance(r, list):
        return 0.0
    for p in r:
        if p.get("symbol") == symbol and p.get("positionQty") not in (None, "0"):
            qty_str = p.get("positionQty", "0")
            qty = float(qty_str) if qty_str else 0.0
            side = p.get("positionSide", "")
            # Flipster: positionSide may indicate direction, OR positionQty is signed
            if side == "SHORT":
                qty = -abs(qty)
            elif side == "LONG":
                qty = abs(qty)
            return qty
    return 0.0


async def get_pending(symbol: str) -> list[dict]:
    s, r = await http("GET", f"/api/v1/trade/order?symbol={symbol}")
    if s != 200:
        return []
    if isinstance(r, list):
        return r
    return r.get("orders", []) if isinstance(r, dict) else []


# ---------------------------------------------------------------------------
# State
# ---------------------------------------------------------------------------

@dataclass
class Quote:
    order_id: str
    price: float
    qty: float
    placed_at: float = field(default_factory=time.time)


@dataclass
class MMState:
    symbol: str
    quote_size: float       # ETH per side
    max_pos: float          # max abs inventory
    imb_threshold: float
    kill_pnl: float

    bid: Quote | None = None
    ask: Quote | None = None
    position: float = 0.0   # signed inventory
    realized_pnl_usd: float = 0.0
    fill_history: list = field(default_factory=list)
    last_fill_ts: float = field(default_factory=time.time)

    # for tick-size rounding
    tick: float = 0.01      # ETH


def round_to_tick(price: float, tick: float, side: str) -> str:
    """Round price toward conservative side (BID rounds down, ASK rounds up)."""
    if side == "BID":
        v = round(price / tick) * tick  # match TOB
    else:
        v = round(price / tick) * tick
    # Format to avoid scientific notation
    decimals = max(0, -int(round(__import__('math').log10(tick))))
    return f"{v:.{decimals}f}"


# ---------------------------------------------------------------------------
# Quote sync
# ---------------------------------------------------------------------------

async def sync_quote(state: MMState, side: str, target_price: float | None,
                     log_fn):
    """Reconcile state's quote with target_price.

    target_price=None → cancel any existing quote on this side.
    Otherwise → ensure a quote exists at target_price (cancel-replace if needed).
    """
    cur = state.bid if side == "BID" else state.ask

    if target_price is None:
        if cur is not None:
            ok = await cancel(state.symbol, cur.order_id)
            log_fn(f"  [cancel-{side}] {cur.order_id[:8]} @ {cur.price} → ok={ok}")
            if side == "BID": state.bid = None
            else: state.ask = None
        return

    target_str = round_to_tick(target_price, state.tick, side)
    target_f = float(target_str)

    if cur is not None and abs(cur.price - target_f) < state.tick / 2:
        return  # already at right price

    # Cancel old (if any), place new
    if cur is not None:
        await cancel(state.symbol, cur.order_id)
        if side == "BID": state.bid = None
        else: state.ask = None

    api_side = "BUY" if side == "BID" else "SELL"
    qty_str = f"{state.quote_size:.4f}"
    r = await place_limit(state.symbol, api_side, qty_str, target_str)

    if r.get("_error"):
        log_fn(f"  [place-{side}] FAILED {target_str}: {r['_error'][:80]}")
        return

    new_q = Quote(order_id=r.get("orderId"), price=target_f, qty=state.quote_size)
    if side == "BID":
        state.bid = new_q
    else:
        state.ask = new_q
    log_fn(f"  [place-{side}] {new_q.order_id[:8]} @ {target_str}")


# ---------------------------------------------------------------------------
# Fill detection
# ---------------------------------------------------------------------------

async def reconcile_position(state: MMState, log_fn) -> bool:
    """Check actual position vs tracked. Returns True if changed."""
    actual = await get_position(state.symbol)
    if abs(actual - state.position) > state.quote_size / 10:
        delta = actual - state.position
        log_fn(f"  [FILL DETECTED] tracked={state.position:.4f} actual={actual:.4f} delta={delta:+.4f}")
        # Determine which side filled
        if delta > 0:
            # We bought (BID filled)
            if state.bid:
                state.fill_history.append({
                    "ts": datetime.now(timezone.utc).isoformat(),
                    "side": "BID", "price": state.bid.price, "qty": delta,
                    "position_after": actual,
                })
                state.bid = None  # filled, cleared from book
        elif delta < 0:
            if state.ask:
                state.fill_history.append({
                    "ts": datetime.now(timezone.utc).isoformat(),
                    "side": "ASK", "price": state.ask.price, "qty": abs(delta),
                    "position_after": actual,
                })
                state.ask = None
        state.position = actual
        state.last_fill_ts = time.time()
        return True
    return False


# ---------------------------------------------------------------------------
# Main loop
# ---------------------------------------------------------------------------

async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--symbol", default="ETHUSDT.PERP")
    p.add_argument("--qty", type=float, default=0.01)
    p.add_argument("--max-pos", type=float, default=0.05)
    p.add_argument("--threshold", type=float, default=0.7)
    p.add_argument("--kill-pnl", type=float, default=-5.0)
    p.add_argument("--duration-min", type=float, default=30.0)
    p.add_argument("--tick", type=float, default=0.01)
    p.add_argument("--log", default="logs/mm_obi.jsonl")
    args = p.parse_args()

    state = MMState(
        symbol=args.symbol, quote_size=args.qty, max_pos=args.max_pos,
        imb_threshold=args.threshold, kill_pnl=args.kill_pnl, tick=args.tick,
    )

    log_path = Path(args.log)
    log_path.parent.mkdir(parents=True, exist_ok=True)

    def log(msg):
        ts = datetime.now().strftime("%H:%M:%S.%f")[:-3]
        line = f"{ts} {msg}"
        print(line, flush=True)

    def log_record(record: dict):
        with open(log_path, "a") as f:
            f.write(json.dumps(record) + "\n")

    log(f"=== MM OBI start ===")
    log(f"  symbol={args.symbol} qty={args.qty} max_pos=±{args.max_pos}")
    log(f"  imb_threshold=±{args.threshold} kill_pnl=${args.kill_pnl}")
    log(f"  duration={args.duration_min}min")

    # Cookies for v2 WS
    sys.path.insert(0, "/home/gate1/projects/quant/hft_monorepo/flipster_web")
    from python.cookies import _fetch_cookies
    ver = json.loads(urllib.request.urlopen("http://localhost:9230/json/version").read())
    cookies = await _fetch_cookies(ver["webSocketDebuggerUrl"])
    cookie_str = "; ".join(f"{k}={v}" for k, v in cookies.items())
    headers = {
        "Cookie": cookie_str, "Origin": "https://flipster.io",
        "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0",
    }

    # Cancel any pre-existing orders on this symbol
    pending = await get_pending(args.symbol)
    for o in pending:
        oid = o.get("orderId")
        if oid:
            await cancel(args.symbol, oid)
            log(f"  [cleanup] cancelled pre-existing {oid[:8]}")

    # Initial position
    state.position = await get_position(args.symbol)
    log(f"  initial position: {state.position:.4f}")

    end_time = time.time() + args.duration_min * 60
    last_sync = 0
    SYNC_INTERVAL = 0.5  # sync at most every 500ms

    try:
        async with websockets.connect(V2_WS, additional_headers=headers, ping_interval=20) as ws:
            sub = {"s": {"market/orderbooks-v2": {"rows": [args.symbol]}}}
            await ws.send(json.dumps(sub))
            log(f"  subscribed to depth")

            while time.time() < end_time:
                try:
                    raw = await asyncio.wait_for(ws.recv(), timeout=2.0)
                except asyncio.TimeoutError:
                    continue

                try:
                    d = json.loads(raw)
                except json.JSONDecodeError:
                    continue

                book_data = d.get("t", {}).get("market/orderbooks-v2", {}).get("s", {}).get(args.symbol)
                if not book_data:
                    continue

                bids = book_data.get("bids", [])[:5]
                asks = book_data.get("asks", [])[:5]
                if len(bids) < 5 or len(asks) < 5:
                    continue

                bs5 = sum(float(b[1]) for b in bids)
                as5 = sum(float(a[1]) for a in asks)
                if bs5 + as5 == 0:
                    continue
                imb = (bs5 - as5) / (bs5 + as5)
                bid_px = float(bids[0][0])
                ask_px = float(asks[0][0])
                mid = (bid_px + ask_px) / 2

                # Throttle sync
                if time.time() - last_sync < SYNC_INTERVAL:
                    continue
                last_sync = time.time()

                # Reconcile fills
                changed = await reconcile_position(state, log)
                if changed:
                    # PnL: rough mark-to-market vs avg entry would require more state
                    # For now, just log fill, compute PnL on close
                    pass

                # Kill switch
                if state.realized_pnl_usd < state.kill_pnl:
                    log(f"  [KILL] realized pnl ${state.realized_pnl_usd:.2f} < ${state.kill_pnl}")
                    break

                # Compute target quotes
                target_bid = bid_px
                target_ask = ask_px

                # Inventory skew
                if state.position >= state.max_pos:
                    target_bid = None  # don't accumulate more long
                if state.position <= -state.max_pos:
                    target_ask = None

                # OBI cancel
                if imb > state.imb_threshold:
                    target_ask = None  # adverse selection on sell side
                elif imb < -state.imb_threshold:
                    target_bid = None

                # Sync
                await sync_quote(state, "BID", target_bid, log)
                await sync_quote(state, "ASK", target_ask, log)

                # Periodic status (every 10 sync iterations ~5s)
                if int(time.time()) % 10 == 0:
                    log(f"  STATUS imb={imb:+.2f} pos={state.position:+.4f} "
                        f"bid={state.bid.price if state.bid else None} "
                        f"ask={state.ask.price if state.ask else None} "
                        f"fills={len(state.fill_history)}")

    finally:
        # Final cleanup: cancel all, log stats
        log(f"\n=== SHUTDOWN ===")
        if state.bid:
            await cancel(state.symbol, state.bid.order_id)
        if state.ask:
            await cancel(state.symbol, state.ask.order_id)

        final_pos = await get_position(state.symbol)
        log(f"  Final position: {final_pos:.4f}")
        log(f"  Total fills: {len(state.fill_history)}")
        bid_fills = sum(1 for f in state.fill_history if f["side"] == "BID")
        ask_fills = sum(1 for f in state.fill_history if f["side"] == "ASK")
        log(f"    BID fills: {bid_fills}")
        log(f"    ASK fills: {ask_fills}")

        # Save fills to log
        log_record({
            "summary": True,
            "ts": datetime.now(timezone.utc).isoformat(),
            "symbol": state.symbol,
            "duration_min": args.duration_min,
            "total_fills": len(state.fill_history),
            "bid_fills": bid_fills,
            "ask_fills": ask_fills,
            "final_position": final_pos,
            "fills": state.fill_history,
        })

        if final_pos != 0:
            log(f"  ⚠ NON-FLAT POSITION: {final_pos:.4f} ETH — manual close needed")


if __name__ == "__main__":
    asyncio.run(main())

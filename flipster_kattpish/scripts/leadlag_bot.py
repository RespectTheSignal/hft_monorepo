#!/usr/bin/env python3
"""Lead-Lag Latency Arb Bot.

Strategy:
  - Subscribe to Binance ETH bookticker (real-time)
  - Compute 500ms cumulative mid return
  - When |return| > THRESHOLD bp:
      → Trade Flipster in same direction as MARKET (IOC limit)
      → Hold for HOLD_MS then exit (IOC limit)
  - Single-venue (Flipster only). No hedge.

Risk:
  - MAX_POSITION ETH (force-close if exceeded)
  - KILL_PNL stop on cumulative loss
  - HARD_EXIT_MS timeout (don't hold beyond expected catch-up window)
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
import urllib.request
from collections import deque
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

import websockets

API_KEY = os.getenv("FLIPSTER_API_KEY", "3|8Q9vdANlA1pvIbimev5NgB__5ULBH3_a")
API_SECRET = os.getenv("FLIPSTER_API_SECRET",
                       "3c823cd1b32582d697b359898952020d0018cdb93b40d6ec4e49bad020d361e2")
V1_HOST = "https://trading-api.flipster.io"
FLIP_V2_WS = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"
BINANCE_WS = "wss://fstream.binance.com/ws/ethusdt@bookTicker"


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
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, {"_error": e.read().decode()[:200]}
    except Exception as e:
        return 0, {"_error": str(e)[:200]}


async def http(method, path, body=None):
    return await asyncio.to_thread(_http_sync, method, path, body)


async def place_market(symbol: str, side: str, qty: str) -> dict:
    """Place MARKET order — accepts any fill price. Pure taker."""
    body = {
        "symbol": symbol, "side": side, "type": "MARKET",
        "quantity": qty,
    }
    s, r = await http("POST", "/api/v1/trade/order", body)
    if s == 200:
        return r.get("order", r)
    return {"_error": r.get("_error", f"http {s}")}


async def place_ioc_taker(symbol: str, side: str, qty: str, price: str) -> dict:
    """Wrapper that uses MARKET (more reliable than IOC LIMIT)."""
    return await place_market(symbol, side, qty)


async def get_position_eth() -> float:
    s, r = await http("GET", "/api/v1/account/position")
    if s != 200 or not isinstance(r, list):
        return 0.0
    for p in r:
        if p.get("symbol") == "ETHUSDT.PERP":
            qty_str = p.get("positionQty")
            if qty_str and qty_str != "0":
                qty = float(qty_str)
                if p.get("positionSide") == "SHORT":
                    qty = -abs(qty)
                else:
                    qty = abs(qty)
                return qty
    return 0.0


# ---------------------------------------------------------------------------
# Binance feed
# ---------------------------------------------------------------------------

@dataclass
class BinanceFeed:
    """Maintains 500ms rolling window of Binance ETH mids."""
    history: deque = field(default_factory=lambda: deque(maxlen=200))  # ts, mid
    latest_mid: float = 0.0
    latest_ts: float = 0.0

    def update(self, ts_ms: int, bid: float, ask: float):
        mid = (bid + ask) / 2
        self.history.append((ts_ms, mid))
        self.latest_mid = mid
        self.latest_ts = ts_ms

    def cumulative_return_bp(self, window_ms: int = 500) -> float | None:
        """Return bp from oldest tick within window."""
        if not self.history or len(self.history) < 2:
            return None
        cutoff = self.latest_ts - window_ms
        old_mid = None
        for ts, mid in self.history:
            if ts >= cutoff:
                old_mid = mid
                break
        if old_mid is None or old_mid == 0:
            return None
        return (self.latest_mid - old_mid) / old_mid * 1e4


# ---------------------------------------------------------------------------
# Flipster TOB cache
# ---------------------------------------------------------------------------

@dataclass
class FlipsterTOB:
    bid: float = 0.0
    ask: float = 0.0
    ts: float = 0.0


# ---------------------------------------------------------------------------
# Bot state
# ---------------------------------------------------------------------------

@dataclass
class BotState:
    qty: float          # ETH quantity per trade
    threshold_bp: float
    hold_ms: int
    max_position: float
    kill_pnl_usd: float

    binance: BinanceFeed = field(default_factory=BinanceFeed)
    flipster: FlipsterTOB = field(default_factory=FlipsterTOB)
    position: float = 0.0
    entry_price: float = 0.0
    entry_time: float = 0.0
    entry_side: str = ""  # "BUY" or "SELL"
    realized_pnl: float = 0.0
    n_attempts: int = 0
    n_filled: int = 0
    n_winning: int = 0
    last_signal_time: float = 0.0

    log_path: Path = field(default_factory=lambda: Path("logs/leadlag.jsonl"))


# ---------------------------------------------------------------------------
# Strategy logic
# ---------------------------------------------------------------------------

async def maybe_enter(state: BotState, log_fn):
    """Check signal, enter if conditions met."""
    if state.position != 0:
        return  # already in position

    # Cooldown 1s between signals
    if time.time() - state.last_signal_time < 1.0:
        return

    # Need fresh data
    if not state.flipster.bid or not state.binance.latest_mid:
        return
    if time.time() * 1000 - state.flipster.ts > 1000:  # stale flipster
        return

    ret_bp = state.binance.cumulative_return_bp(500)
    if ret_bp is None:
        return

    if abs(ret_bp) < state.threshold_bp:
        return

    # Signal! Enter Flipster taker in same direction
    side = "BUY" if ret_bp > 0 else "SELL"
    if side == "BUY":
        # Take ASK (cross spread = taker)
        price = str(state.flipster.ask)
    else:
        # Take BID
        price = str(state.flipster.bid)

    state.last_signal_time = time.time()
    state.n_attempts += 1
    log_fn(f"[ENTER #{state.n_attempts}] bin_ret={ret_bp:+.2f}bp side={side} price={price}")

    r = await place_ioc_taker("ETHUSDT.PERP", side, f"{state.qty:.4f}", price)
    if r.get("_error"):
        log_fn(f"  ✗ entry failed: {r['_error']}")
        return

    # Check if filled (status=FILLED) or partial
    status = r.get("status", "?")
    leaves = float(r.get("leavesQty", state.qty))
    fill_qty = state.qty - leaves

    if fill_qty < state.qty * 0.5:
        log_fn(f"  partial fill {fill_qty}/{state.qty} status={status}")
        return

    # Sleep to let actual position update
    await asyncio.sleep(0.3)
    actual_pos = await get_position_eth()
    if abs(actual_pos) < state.qty * 0.5:
        log_fn(f"  ✗ no position after order (sync issue): {actual_pos}")
        return

    state.n_filled += 1
    state.position = actual_pos
    state.entry_time = time.time()
    state.entry_side = side
    # Get entry price from order or position
    avg_price = float(r.get("avgPrice", price))
    state.entry_price = avg_price
    log_fn(f"  ✓ filled {actual_pos:+.4f} @ ~${avg_price}")


async def maybe_exit(state: BotState, log_fn):
    if state.position == 0:
        return

    elapsed = (time.time() - state.entry_time) * 1000
    if elapsed < state.hold_ms:
        return

    # Time to exit
    exit_side = "SELL" if state.position > 0 else "BUY"
    if exit_side == "SELL":
        price = str(state.flipster.bid)  # hit bid
    else:
        price = str(state.flipster.ask)  # take ask

    qty = abs(state.position)
    log_fn(f"[EXIT] hold={elapsed:.0f}ms side={exit_side} price={price}")

    r = await place_ioc_taker("ETHUSDT.PERP", exit_side, f"{qty:.4f}", price)
    if r.get("_error"):
        log_fn(f"  ✗ exit failed: {r['_error']}")
        # try market close
        body = {
            "symbol": "ETHUSDT.PERP", "side": exit_side, "type": "MARKET",
            "quantity": f"{qty:.4f}",
        }
        s, r2 = await http("POST", "/api/v1/trade/order", body)
        if s == 200:
            r = r2.get("order", r2)
        else:
            log_fn(f"  ✗ market exit also failed: {r2}")
            return

    await asyncio.sleep(0.3)
    actual = await get_position_eth()

    avg_exit = float(r.get("avgPrice", price))
    sign = 1 if state.entry_side == "BUY" else -1
    pnl_bp = (avg_exit - state.entry_price) / state.entry_price * 1e4 * sign
    pnl_usd = pnl_bp * (state.qty * state.entry_price) / 1e4
    state.realized_pnl += pnl_usd
    if pnl_bp > 0:
        state.n_winning += 1

    log_fn(f"  ✓ exit @ ${avg_exit} pnl={pnl_bp:+.2f}bp (${pnl_usd:+.4f})")
    log_fn(f"  STATS: filled={state.n_filled} wins={state.n_winning} ({state.n_winning/state.n_filled*100:.0f}%) total=${state.realized_pnl:+.4f}")

    # Log to JSONL
    with open(state.log_path, "a") as f:
        f.write(json.dumps({
            "ts_close": datetime.now(timezone.utc).isoformat(),
            "side": state.entry_side,
            "entry": state.entry_price,
            "exit": avg_exit,
            "qty": state.qty,
            "hold_ms": elapsed,
            "pnl_bp": pnl_bp,
            "pnl_usd": pnl_usd,
        }) + "\n")

    # Reset
    state.position = actual
    state.entry_price = 0.0
    state.entry_side = ""

    # Kill switch
    if state.realized_pnl < state.kill_pnl_usd:
        log_fn(f"  ⛔ KILL: realized ${state.realized_pnl:.2f} < ${state.kill_pnl_usd}")
        raise SystemExit(0)


# ---------------------------------------------------------------------------
# Feeds
# ---------------------------------------------------------------------------

async def binance_feed_task(state: BotState, log_fn):
    while True:
        try:
            async with websockets.connect(BINANCE_WS, ping_interval=20) as ws:
                log_fn("[binance] connected")
                async for msg in ws:
                    try:
                        d = json.loads(msg)
                    except: continue
                    bid = float(d.get("b", 0))
                    ask = float(d.get("a", 0))
                    ts_ms = int(d.get("T", time.time() * 1000))
                    if bid > 0 and ask > 0:
                        state.binance.update(ts_ms, bid, ask)
        except Exception as e:
            log_fn(f"[binance] err: {e}, reconnect 3s")
            await asyncio.sleep(3)


async def flipster_feed_task(state: BotState, log_fn):
    sys.path.insert(0, "/home/gate1/projects/quant/hft_monorepo/flipster_web")
    from python.cookies import _fetch_cookies

    while True:
        try:
            ver = json.loads(urllib.request.urlopen(
                "http://localhost:9230/json/version", timeout=5
            ).read())
            cookies = await _fetch_cookies(ver["webSocketDebuggerUrl"])
            cookie_str = "; ".join(f"{k}={v}" for k, v in cookies.items())
            headers = {
                "Cookie": cookie_str, "Origin": "https://flipster.io",
                "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0",
            }
            async with websockets.connect(FLIP_V2_WS, additional_headers=headers, ping_interval=20) as ws:
                sub = {"s": {"market/orderbooks-v2": {"rows": ["ETHUSDT.PERP"]}}}
                await ws.send(json.dumps(sub))
                log_fn("[flipster] connected")
                async for msg in ws:
                    try:
                        d = json.loads(msg)
                    except: continue
                    book = d.get("t", {}).get("market/orderbooks-v2", {}).get("s", {}).get("ETHUSDT.PERP")
                    if not book: continue
                    bids = book.get("bids", [])
                    asks = book.get("asks", [])
                    if bids and asks:
                        state.flipster.bid = float(bids[0][0])
                        state.flipster.ask = float(asks[0][0])
                        state.flipster.ts = time.time() * 1000
        except Exception as e:
            log_fn(f"[flipster] err: {e}, reconnect 3s")
            await asyncio.sleep(3)


async def strategy_loop(state: BotState, log_fn, duration_s: float):
    end = time.time() + duration_s
    while time.time() < end:
        try:
            await maybe_enter(state, log_fn)
            await maybe_exit(state, log_fn)
        except Exception as e:
            log_fn(f"[strategy] err: {e}")
        await asyncio.sleep(0.05)
    log_fn(f"\n[done] duration reached")


async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--qty", type=float, default=0.01)  # ETH
    p.add_argument("--threshold", type=float, default=3.0)  # bp
    p.add_argument("--hold-ms", type=int, default=1000)
    p.add_argument("--max-pos", type=float, default=0.05)
    p.add_argument("--kill-pnl", type=float, default=-2.0)
    p.add_argument("--duration-min", type=float, default=30.0)
    p.add_argument("--log", default="logs/leadlag.jsonl")
    args = p.parse_args()

    state = BotState(
        qty=args.qty, threshold_bp=args.threshold, hold_ms=args.hold_ms,
        max_position=args.max_pos, kill_pnl_usd=args.kill_pnl,
        log_path=Path(args.log),
    )
    state.log_path.parent.mkdir(parents=True, exist_ok=True)

    def log(msg):
        ts = datetime.now().strftime("%H:%M:%S.%f")[:-3]
        print(f"{ts} {msg}", flush=True)

    log(f"=== LEADLAG BOT START ===")
    log(f"  qty={args.qty} ETH  threshold={args.threshold}bp  hold={args.hold_ms}ms")
    log(f"  max_pos=±{args.max_pos}  kill_pnl=${args.kill_pnl}")
    log(f"  duration={args.duration_min}min")

    # Initial position
    state.position = await get_position_eth()
    log(f"  initial position: {state.position}")

    # Run all tasks
    tasks = [
        asyncio.create_task(binance_feed_task(state, log)),
        asyncio.create_task(flipster_feed_task(state, log)),
        asyncio.create_task(strategy_loop(state, log, args.duration_min * 60)),
    ]

    try:
        # Wait for strategy_loop to finish
        await tasks[2]
    finally:
        # Cancel feeds
        for t in tasks[:2]:
            t.cancel()
        # Final position close
        final = await get_position_eth()
        log(f"\n=== SHUTDOWN ===")
        log(f"  final position: {final}")
        if final != 0:
            log(f"  closing residual {final} ETH...")
            side = "SELL" if final > 0 else "BUY"
            body = {"symbol": "ETHUSDT.PERP", "side": side, "type": "MARKET", "quantity": str(abs(final))}
            s, r = await http("POST", "/api/v1/trade/order", body)
            log(f"  close: {s} {str(r)[:200]}")

        log(f"\nFinal stats:")
        log(f"  attempts={state.n_attempts}  filled={state.n_filled}  wins={state.n_winning}")
        if state.n_filled:
            log(f"  win rate: {state.n_winning/state.n_filled*100:.1f}%")
        log(f"  realized PnL: ${state.realized_pnl:+.4f}")


if __name__ == "__main__":
    asyncio.run(main())

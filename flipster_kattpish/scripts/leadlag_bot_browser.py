#!/usr/bin/env python3
"""Lead-Lag Latency Arb Bot — BROWSER account version.

Same strategy as leadlag_bot.py but uses Flipster v2 (browser cookies + proxies)
instead of HMAC v1 API. Allows trading on the browser-logged-in account.

Differences from leadlag_bot.py:
  - Uses FlipsterClient (cookies + proxies) for orders
  - amount_usd (USD notional) instead of quantity (ETH)
  - Side.LONG / Side.SHORT instead of BUY/SELL
  - close_position uses slot from open response
  - Leverage 10x for capital efficiency on small browser balance
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import time
import urllib.request
from collections import deque
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

import websockets

MONOREPO = Path(__file__).resolve().parent.parent.parent
sys.path.insert(0, str(MONOREPO / "flipster_web"))
from python.client import FlipsterClient
from python.browser import BrowserManager as FBM
from python.order import OrderParams, Side, OrderType
from python.cookies import _fetch_cookies

FLIP_V2_WS = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"
BINANCE_WS = "wss://fstream.binance.com/ws/ethusdt@bookTicker"


@dataclass
class BinanceFeed:
    history: deque = field(default_factory=lambda: deque(maxlen=200))
    latest_mid: float = 0.0
    latest_ts: float = 0.0

    def update(self, ts_ms: int, bid: float, ask: float):
        mid = (bid + ask) / 2
        self.history.append((ts_ms, mid))
        self.latest_mid = mid
        self.latest_ts = ts_ms

    def cumulative_return_bp(self, window_ms: int = 500) -> float | None:
        if not self.history or len(self.history) < 2:
            return None
        cutoff = self.latest_ts - window_ms
        old = None
        for ts, mid in self.history:
            if ts >= cutoff:
                old = mid
                break
        if old is None or old == 0:
            return None
        return (self.latest_mid - old) / old * 1e4


@dataclass
class FlipsterTOB:
    bid: float = 0.0
    ask: float = 0.0
    ts: float = 0.0


@dataclass
class BotState:
    amount_usd: float
    leverage: int
    threshold_bp: float
    hold_ms: int
    kill_pnl_usd: float

    binance: BinanceFeed = field(default_factory=BinanceFeed)
    flipster: FlipsterTOB = field(default_factory=FlipsterTOB)

    in_position: bool = False
    entry_slot: int | None = None
    entry_price: float = 0.0
    entry_time: float = 0.0
    entry_side: Side | None = None

    realized_pnl: float = 0.0
    n_attempts: int = 0
    n_filled: int = 0
    n_winning: int = 0
    last_signal_time: float = 0.0

    log_path: Path = field(default_factory=lambda: Path("logs/leadlag_browser.jsonl"))
    fc: FlipsterClient = None


# ---------------------------------------------------------------------------
# Strategy
# ---------------------------------------------------------------------------

async def maybe_enter(state: BotState, log_fn):
    if state.in_position:
        return
    if time.time() - state.last_signal_time < 1.0:
        return
    if not state.flipster.bid or not state.binance.latest_mid:
        return
    if time.time() * 1000 - state.flipster.ts > 1000:
        return

    ret_bp = state.binance.cumulative_return_bp(500)
    if ret_bp is None or abs(ret_bp) < state.threshold_bp:
        return

    side = Side.LONG if ret_bp > 0 else Side.SHORT
    ref_price = state.flipster.ask if side == Side.LONG else state.flipster.bid

    state.last_signal_time = time.time()
    state.n_attempts += 1
    log_fn(f"[ENTER #{state.n_attempts}] bin_ret={ret_bp:+.2f}bp side={side.value} ref=${ref_price}")

    try:
        params = OrderParams(side=side, amount_usd=state.amount_usd,
                             order_type=OrderType.MARKET, leverage=state.leverage)
        r = await asyncio.to_thread(state.fc.place_order, "ETHUSDT.PERP", params, ref_price)
    except Exception as e:
        log_fn(f"  ✗ entry failed: {e}")
        return

    pos = r.get("position", {})
    slot = pos.get("slot")
    avg = pos.get("avgPrice")
    if slot is None or avg is None:
        log_fn(f"  ✗ unexpected response: {str(r)[:200]}")
        return

    state.in_position = True
    state.entry_slot = slot
    state.entry_price = float(avg)
    state.entry_time = time.time()
    state.entry_side = side
    state.n_filled += 1
    log_fn(f"  ✓ filled slot={slot} @ ${avg}")


async def maybe_exit(state: BotState, log_fn):
    if not state.in_position:
        return
    elapsed_ms = (time.time() - state.entry_time) * 1000
    if elapsed_ms < state.hold_ms:
        return

    # Try close at current bid/ask, retry with offsets if InsufficientLiquidity
    ref = state.flipster.bid if state.entry_side == Side.LONG else state.flipster.ask
    close_ok = False
    avg_close = None
    offsets = [1.0, 0.999, 1.001, 0.995, 1.005] if state.entry_side == Side.LONG else [1.0, 1.001, 0.999, 1.005, 0.995]

    for off_mul in offsets:
        try:
            try_px = ref * off_mul
            r = await asyncio.to_thread(state.fc.close_position, "ETHUSDT.PERP", state.entry_slot, try_px)
            order = r.get("order", {})
            avg_close = float(order.get("avgPrice", try_px))
            close_ok = True
            log_fn(f"[EXIT] hold={elapsed_ms:.0f}ms closed @ ${avg_close} (off_mul={off_mul:.3f})")
            break
        except Exception as e:
            if "InsufficientLiquidity" in str(e):
                continue
            log_fn(f"  ✗ close err: {str(e)[:100]}")
            break

    if not close_ok:
        log_fn(f"  ⚠ ALL CLOSE ATTEMPTS FAILED — slot={state.entry_slot} still open!")
        # Don't reset state, let next iteration retry
        return

    sign = 1 if state.entry_side == Side.LONG else -1
    pnl_bp = (avg_close - state.entry_price) / state.entry_price * 1e4 * sign
    pnl_usd = pnl_bp * state.amount_usd / 1e4
    state.realized_pnl += pnl_usd
    if pnl_bp > 0: state.n_winning += 1

    log_fn(f"  pnl={pnl_bp:+.2f}bp (${pnl_usd:+.4f})  total=${state.realized_pnl:+.4f}  wins={state.n_winning}/{state.n_filled}")

    with open(state.log_path, "a") as f:
        f.write(json.dumps({
            "ts": datetime.now(timezone.utc).isoformat(),
            "side": state.entry_side.value,
            "entry": state.entry_price, "exit": avg_close,
            "amount_usd": state.amount_usd, "leverage": state.leverage,
            "hold_ms": elapsed_ms, "pnl_bp": pnl_bp, "pnl_usd": pnl_usd,
        }) + "\n")

    state.in_position = False
    state.entry_slot = None
    state.entry_side = None

    if state.realized_pnl < state.kill_pnl_usd:
        log_fn(f"  ⛔ KILL: ${state.realized_pnl:.2f} < ${state.kill_pnl_usd}")
        raise SystemExit(0)


# ---------------------------------------------------------------------------
# Feeds
# ---------------------------------------------------------------------------

async def binance_feed(state: BotState, log):
    while True:
        try:
            async with websockets.connect(BINANCE_WS, ping_interval=20) as ws:
                log("[binance] connected")
                async for msg in ws:
                    try: d = json.loads(msg)
                    except: continue
                    bid = float(d.get("b", 0))
                    ask = float(d.get("a", 0))
                    ts_ms = int(d.get("T", time.time() * 1000))
                    if bid > 0 and ask > 0:
                        state.binance.update(ts_ms, bid, ask)
        except Exception as e:
            log(f"[binance] err {e}, retry 3s")
            await asyncio.sleep(3)


async def flipster_feed(state: BotState, log):
    while True:
        try:
            ver = json.loads(urllib.request.urlopen(
                "http://localhost:9230/json/version", timeout=5).read())
            cookies = await _fetch_cookies(ver["webSocketDebuggerUrl"])
            cookie_str = "; ".join(f"{k}={v}" for k, v in cookies.items())
            headers = {
                "Cookie": cookie_str, "Origin": "https://flipster.io",
                "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0",
            }
            async with websockets.connect(FLIP_V2_WS, additional_headers=headers, ping_interval=20) as ws:
                await ws.send(json.dumps({"s": {"market/orderbooks-v2": {"rows": ["ETHUSDT.PERP"]}}}))
                log("[flipster] connected")
                async for msg in ws:
                    try: d = json.loads(msg)
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
            log(f"[flipster] err {e}, retry 3s")
            await asyncio.sleep(3)


async def strategy_loop(state: BotState, log, duration_s: float):
    end = time.time() + duration_s
    while time.time() < end:
        try:
            await maybe_enter(state, log)
            await maybe_exit(state, log)
        except SystemExit:
            raise
        except Exception as e:
            log(f"[strategy] err: {e}")
        await asyncio.sleep(0.05)


async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--amount-usd", type=float, default=10.0)
    p.add_argument("--leverage", type=int, default=10)
    p.add_argument("--threshold", type=float, default=3.0)
    p.add_argument("--hold-ms", type=int, default=1000)
    p.add_argument("--kill-pnl", type=float, default=-2.0)
    p.add_argument("--duration-min", type=float, default=30.0)
    args = p.parse_args()

    proxies = (Path(__file__).parent / "proxies.txt").read_text().splitlines()
    proxies = [p for p in proxies if p.strip()]
    fc = FlipsterClient(FBM(), proxies=proxies)
    # Async cookie extraction (sync version conflicts with running event loop)
    ver = json.loads(urllib.request.urlopen("http://localhost:9230/json/version", timeout=5).read())
    cookies = await _fetch_cookies(ver["webSocketDebuggerUrl"])
    import requests
    fc._cookies = cookies
    fc._session = requests.Session()
    fc._session.cookies.update(cookies)
    fc._session.headers.update({
        "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0",
        "Accept": "application/json, text/plain, */*",
        "Referer": "https://flipster.io/",
        "Content-Type": "application/json",
        "X-Prex-Client-Platform": "web",
        "X-Prex-Client-Version": "release-web-3.15.110",
        "Origin": "https://flipster.io",
    })

    state = BotState(
        amount_usd=args.amount_usd, leverage=args.leverage,
        threshold_bp=args.threshold, hold_ms=args.hold_ms,
        kill_pnl_usd=args.kill_pnl, fc=fc,
    )
    state.log_path.parent.mkdir(parents=True, exist_ok=True)

    def log(msg):
        ts = datetime.now().strftime("%H:%M:%S.%f")[:-3]
        print(f"{ts} {msg}", flush=True)

    log(f"=== LEADLAG BOT (BROWSER ACCT) START ===")
    log(f"  amount=${args.amount_usd} leverage={args.leverage}x  threshold={args.threshold}bp  hold={args.hold_ms}ms")
    log(f"  notional=${args.amount_usd * args.leverage}  kill_pnl=${args.kill_pnl}")
    log(f"  duration={args.duration_min}min  proxies={len(proxies)}")

    tasks = [
        asyncio.create_task(binance_feed(state, log)),
        asyncio.create_task(flipster_feed(state, log)),
        asyncio.create_task(strategy_loop(state, log, args.duration_min * 60)),
    ]

    try:
        await tasks[2]
    finally:
        for t in tasks[:2]: t.cancel()
        log(f"\n=== SHUTDOWN ===")
        log(f"  attempts={state.n_attempts} filled={state.n_filled} wins={state.n_winning}")
        if state.n_filled:
            log(f"  win rate: {state.n_winning/state.n_filled*100:.1f}%")
        log(f"  realized PnL: ${state.realized_pnl:+.4f}")
        if state.in_position:
            log(f"  ⚠ position still open: slot={state.entry_slot}")


if __name__ == "__main__":
    asyncio.run(main())

#!/usr/bin/env python3
"""Slow Stat-Arb on Flipster.

Strategy:
  - Compute consensus mid from Binance + Gate + Bitget (3 exchanges)
  - Compare to Flipster mid
  - When |Flipster - consensus| > THRESHOLD bp:
      - Sustained for >= MIN_DURATION_S seconds (filter noise)
      - Trade Flipster in direction of CONVERGENCE (back to consensus)
  - Exit conditions:
      - |dev| < EXIT_THRESHOLD bp (converged)
      - HOLD_MAX_S timeout
      - KILL_PNL drawdown

This avoids HFT competition by waiting for SUSTAINED deviations,
not microsecond moves. 30-60s hold gives Flipster time to actually catch up.
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
BINANCE_WS = "wss://fstream.binance.com/ws"
GATE_WS = "wss://fx-ws.gateio.ws/v4/ws/usdt"
BITGET_WS = "wss://ws.bitget.com/v2/ws/public"

# (base, binance_lower, gate, bitget, flipster)
SYMBOLS_CFG = {
    "BTC":  ("btcusdt",  "BTC_USDT",  "BTCUSDT",  "BTCUSDT.PERP"),
    "ETH":  ("ethusdt",  "ETH_USDT",  "ETHUSDT",  "ETHUSDT.PERP"),
    "SOL":  ("solusdt",  "SOL_USDT",  "SOLUSDT",  "SOLUSDT.PERP"),
    "XRP":  ("xrpusdt",  "XRP_USDT",  "XRPUSDT",  "XRPUSDT.PERP"),
    "DOGE": ("dogeusdt", "DOGE_USDT", "DOGEUSDT", "DOGEUSDT.PERP"),
    "LINK": ("linkusdt", "LINK_USDT", "LINKUSDT", "LINKUSDT.PERP"),
    "AVAX": ("avaxusdt", "AVAX_USDT", "AVAXUSDT", "AVAXUSDT.PERP"),
    "ARB":  ("arbusdt",  "ARB_USDT",  "ARBUSDT",  "ARBUSDT.PERP"),
    "LTC":  ("ltcusdt",  "LTC_USDT",  "LTCUSDT",  "LTCUSDT.PERP"),
    "INJ":  ("injusdt",  "INJ_USDT",  "INJUSDT",  "INJUSDT.PERP"),
    "SUI":  ("suiusdt",  "SUI_USDT",  "SUIUSDT",  "SUIUSDT.PERP"),
    "TON":  ("tonusdt",  "TON_USDT",  "TONUSDT",  "TONUSDT.PERP"),
    "APT":  ("aptusdt",  "APT_USDT",  "APTUSDT",  "APTUSDT.PERP"),
}


@dataclass
class Mid:
    val: float = 0.0
    ts: float = 0.0


@dataclass
class SymbolState:
    base: str
    flip: Mid = field(default_factory=Mid)
    binance: Mid = field(default_factory=Mid)
    gate: Mid = field(default_factory=Mid)
    bitget: Mid = field(default_factory=Mid)
    # Track sustained deviation
    dev_history: deque = field(default_factory=lambda: deque(maxlen=10))  # last 10 sec
    # Position
    in_position: bool = False
    entry_slot: int | None = None
    entry_price: float = 0.0
    entry_time: float = 0.0
    entry_side: Side | None = None
    entry_dev: float = 0.0
    last_signal_time: float = 0.0
    _close_cooldown: float = 0.0

    def consensus(self) -> float | None:
        """Avg of 3 external venues. Returns None if any missing/stale."""
        now = time.time()
        for m in [self.binance, self.gate, self.bitget]:
            if m.val == 0 or now - m.ts > 5.0:  # 5s stale tolerance
                return None
        return (self.binance.val + self.gate.val + self.bitget.val) / 3

    def deviation_bp(self) -> float | None:
        if self.flip.val == 0 or time.time() - self.flip.ts > 2.0:
            return None
        cons = self.consensus()
        if cons is None: return None
        return (self.flip.val - cons) / cons * 1e4


@dataclass
class BotState:
    symbols: list[str]
    amount_usd: float
    leverage: int
    threshold_bp: float
    exit_threshold_bp: float
    sustain_secs: float
    hold_max_s: float
    kill_pnl_usd: float

    sym: dict = field(default_factory=dict)
    realized_pnl: float = 0.0
    n_signals: int = 0
    n_filled: int = 0
    n_wins: int = 0
    fc: FlipsterClient = None
    log_path: Path = field(default_factory=lambda: Path("logs/slow_statarb.jsonl"))


# ---------------------------------------------------------------------------
# Strategy
# ---------------------------------------------------------------------------

async def maybe_enter(state: BotState, base: str, log):
    s = state.sym[base]
    if s.in_position: return
    if time.time() - s.last_signal_time < 5.0: return

    dev = s.deviation_bp()
    if dev is None: return

    # Track for sustain check
    s.dev_history.append((time.time(), dev))

    if abs(dev) < state.threshold_bp:
        return

    # Check sustained (multiple recent samples agree)
    cutoff = time.time() - state.sustain_secs
    recent = [d for t, d in s.dev_history if t >= cutoff]
    if len(recent) < 2: return  # need at least 2 samples
    same_sign = (sum(1 for d in recent if (d > 0) == (dev > 0)) / len(recent))
    avg_dev = sum(recent) / len(recent)
    if same_sign < 0.7 or abs(avg_dev) < state.threshold_bp:
        return  # not sustained

    # Direction: Flipster > consensus → Flipster overpriced → SHORT
    side = Side.SHORT if dev > 0 else Side.LONG
    flip_sym = SYMBOLS_CFG[base][3]
    ref = s.flip.val

    s.last_signal_time = time.time()
    state.n_signals += 1
    log(f"[{base} ENTER #{state.n_signals}] dev={dev:+.2f}bp avg={avg_dev:+.2f} side={side.value} ref=${ref}")

    try:
        params = OrderParams(side=side, amount_usd=state.amount_usd,
                             order_type=OrderType.MARKET, leverage=state.leverage)
        r = await asyncio.to_thread(state.fc.place_order, flip_sym, params, ref)
    except Exception as e:
        log(f"  ✗ entry: {str(e)[:120]}")
        return

    pos = r.get("position", {})
    slot = pos.get("slot")
    avg = pos.get("avgPrice")
    if slot is None or avg is None:
        log(f"  ✗ bad response: {str(r)[:200]}")
        return

    s.in_position = True
    s.entry_slot = slot
    s.entry_price = float(avg)
    s.entry_time = time.time()
    s.entry_side = side
    s.entry_dev = dev
    state.n_filled += 1
    log(f"  ✓ filled slot={slot} @ ${avg}")


async def maybe_exit(state: BotState, base: str, log):
    s = state.sym[base]
    if not s.in_position: return
    elapsed = time.time() - s.entry_time

    dev_now = s.deviation_bp()
    cur_abs = abs(dev_now) if dev_now is not None else 99
    timeout = elapsed > state.hold_max_s
    converged = cur_abs < state.exit_threshold_bp

    if not (timeout or converged):
        return

    # Per-symbol close cooldown — if recent 429 / failure, wait
    if hasattr(s, '_close_cooldown') and time.time() < s._close_cooldown:
        return

    reason = "timeout" if timeout else "converged"
    flip_sym = SYMBOLS_CFG[base][3]
    ref = s.flip.val
    close_ok = False
    avg_close = None
    # Only TWO offsets, NOT five — fast fail
    offsets = [1.0, 0.997] if s.entry_side == Side.LONG else [1.0, 1.003]
    rate_limited = False
    for off in offsets:
        try:
            try_px = ref * off
            r = await asyncio.to_thread(state.fc.close_position, flip_sym, s.entry_slot, try_px)
            avg_close = float(r.get("order", {}).get("avgPrice", try_px))
            close_ok = True
            break
        except Exception as e:
            err = str(e)[:200]
            if "429" in err or "rate limit" in err.lower():
                rate_limited = True
                break  # don't try more, will only worsen
            if "UnknownPosition" in err:
                # Position already closed somehow
                log(f"  [{base}] position already closed (UnknownPosition)")
                close_ok = True
                avg_close = ref
                break
            if "InsufficientLiquidity" in err:
                await asyncio.sleep(0.5)  # wait between price retries
                continue
            log(f"  ✗ close: {err[:100]}")
            break

    if not close_ok:
        if rate_limited:
            s._close_cooldown = time.time() + 30  # 30 sec cooldown on 429
            log(f"  ⚠ {base} 429 — cooldown 30s before retry")
        else:
            log(f"  ⚠ {base} close failed (will retry next loop)")
        return

    sign = 1 if s.entry_side == Side.LONG else -1
    pnl_bp = (avg_close - s.entry_price) / s.entry_price * 1e4 * sign
    pnl_usd = pnl_bp * state.amount_usd / 1e4
    state.realized_pnl += pnl_usd
    if pnl_bp > 0: state.n_wins += 1

    dev_str = f"{dev_now:+.2f}" if dev_now is not None else "N/A"
    log(f"[{base} EXIT-{reason}] hold={elapsed:.0f}s entry=${s.entry_price} exit=${avg_close} entry_dev={s.entry_dev:+.2f}→exit_dev={dev_str} pnl={pnl_bp:+.2f}bp (${pnl_usd:+.4f})")
    log(f"  TOTAL: filled={state.n_filled} wins={state.n_wins} ({state.n_wins/state.n_filled*100:.0f}%) pnl=${state.realized_pnl:+.4f}")

    with open(state.log_path, "a") as f:
        f.write(json.dumps({
            "ts": datetime.now(timezone.utc).isoformat(),
            "base": base, "side": s.entry_side.value, "reason": reason,
            "entry": s.entry_price, "exit": avg_close,
            "entry_dev_bp": s.entry_dev, "exit_dev_bp": dev_now,
            "amount_usd": state.amount_usd, "hold_s": elapsed,
            "pnl_bp": pnl_bp, "pnl_usd": pnl_usd,
        }) + "\n")

    s.in_position = False
    s.entry_slot = None
    s.entry_side = None

    if state.realized_pnl < state.kill_pnl_usd:
        log(f"⛔ KILL: ${state.realized_pnl:.2f}")
        raise SystemExit(0)


# ---------------------------------------------------------------------------
# Feeds
# ---------------------------------------------------------------------------

async def binance_feed(state: BotState, log):
    streams = "/".join(f"{SYMBOLS_CFG[b][0]}@bookTicker" for b in state.symbols)
    url = f"{BINANCE_WS}/{streams}"
    while True:
        try:
            async with websockets.connect(url, ping_interval=20) as ws:
                log("[binance] connected")
                async for msg in ws:
                    try: d = json.loads(msg)
                    except: continue
                    sym = d.get("s", "").lower()
                    base = next((b for b, c in SYMBOLS_CFG.items() if c[0] == sym), None)
                    if not base: continue
                    bid = float(d.get("b", 0)); ask = float(d.get("a", 0))
                    if bid > 0 and ask > 0:
                        state.sym[base].binance.val = (bid + ask) / 2
                        state.sym[base].binance.ts = time.time()
        except Exception as e:
            log(f"[binance] err {str(e)[:50]}, retry 3s")
            await asyncio.sleep(3)


async def gate_feed(state: BotState, log):
    while True:
        try:
            async with websockets.connect(GATE_WS, ping_interval=20) as ws:
                log("[gate] connected")
                for base in state.symbols:
                    sub = {"time": int(time.time()), "channel": "futures.book_ticker",
                           "event": "subscribe", "payload": [SYMBOLS_CFG[base][1]]}
                    await ws.send(json.dumps(sub))
                async for msg in ws:
                    try: d = json.loads(msg)
                    except: continue
                    if d.get("channel") != "futures.book_ticker" or d.get("event") != "update": continue
                    res = d.get("result", {})
                    sym = res.get("s", "")
                    base = next((b for b, c in SYMBOLS_CFG.items() if c[1] == sym), None)
                    if not base: continue
                    bid = float(res.get("b", 0)); ask = float(res.get("a", 0))
                    if bid > 0 and ask > 0:
                        state.sym[base].gate.val = (bid + ask) / 2
                        state.sym[base].gate.ts = time.time()
        except Exception as e:
            log(f"[gate] err {str(e)[:50]}, retry 3s")
            await asyncio.sleep(3)


async def bitget_feed(state: BotState, log):
    while True:
        try:
            async with websockets.connect(BITGET_WS, ping_interval=20) as ws:
                log("[bitget] connecting...")
                # Subscribe to ticker channel for each symbol
                args = [{"instType": "USDT-FUTURES", "channel": "ticker", "instId": SYMBOLS_CFG[b][2]}
                        for b in state.symbols]
                await ws.send(json.dumps({"op": "subscribe", "args": args}))
                log("[bitget] connected")
                async for msg in ws:
                    if msg == "pong": continue
                    try: d = json.loads(msg)
                    except: continue
                    if d.get("action") not in ("snapshot", "update"): continue
                    arg = d.get("arg", {})
                    if arg.get("channel") != "ticker": continue
                    inst = arg.get("instId", "")
                    base = next((b for b, c in SYMBOLS_CFG.items() if c[2] == inst), None)
                    if not base: continue
                    for tick in d.get("data", []):
                        bid = float(tick.get("bidPr", 0)); ask = float(tick.get("askPr", 0))
                        if bid > 0 and ask > 0:
                            state.sym[base].bitget.val = (bid + ask) / 2
                            state.sym[base].bitget.ts = time.time()
        except Exception as e:
            log(f"[bitget] err {str(e)[:50]}, retry 3s")
            await asyncio.sleep(3)


async def flipster_feed(state: BotState, log):
    while True:
        try:
            ver = json.loads(urllib.request.urlopen("http://localhost:9230/json/version", timeout=5).read())
            cookies = await _fetch_cookies(ver["webSocketDebuggerUrl"])
            cookie_str = "; ".join(f"{k}={v}" for k, v in cookies.items())
            headers = {"Cookie": cookie_str, "Origin": "https://flipster.io",
                       "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0"}
            async with websockets.connect(FLIP_V2_WS, additional_headers=headers, ping_interval=20) as ws:
                syms_flip = [SYMBOLS_CFG[b][3] for b in state.symbols]
                await ws.send(json.dumps({"s": {"market/orderbooks-v2": {"rows": syms_flip}}}))
                log(f"[flipster] connected ({len(syms_flip)} syms)")
                async for msg in ws:
                    try: d = json.loads(msg)
                    except: continue
                    s_data = d.get("t", {}).get("market/orderbooks-v2", {}).get("s", {})
                    for sym, book in s_data.items():
                        base = next((b for b, c in SYMBOLS_CFG.items() if c[3] == sym), None)
                        if not base: continue
                        bids = book.get("bids", [])
                        asks = book.get("asks", [])
                        if bids and asks:
                            bid = float(bids[0][0]); ask = float(asks[0][0])
                            state.sym[base].flip.val = (bid + ask) / 2
                            state.sym[base].flip.ts = time.time()
        except Exception as e:
            log(f"[flipster] err {str(e)[:50]}, retry 3s")
            await asyncio.sleep(3)


async def strategy_loop(state: BotState, log, duration_s):
    end = time.time() + duration_s
    last_status = 0
    while time.time() < end:
        try:
            for base in state.symbols:
                await maybe_enter(state, base, log)
                await maybe_exit(state, base, log)
            # periodic status
            if time.time() - last_status > 60:
                last_status = time.time()
                statuses = []
                for b in state.symbols:
                    d = state.sym[b].deviation_bp()
                    statuses.append(f"{b}={d:+.1f}" if d is not None else f"{b}=N/A")
                log(f"  [status] devs: {' | '.join(statuses)}  filled={state.n_filled} wins={state.n_wins} pnl=${state.realized_pnl:+.4f}")
        except SystemExit: raise
        except Exception as e: log(f"[strategy] err: {e}")
        await asyncio.sleep(0.5)


async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--symbols", default="BTC,ETH,SOL,XRP,DOGE")
    p.add_argument("--amount-usd", type=float, default=50)
    p.add_argument("--leverage", type=int, default=10)
    p.add_argument("--threshold", type=float, default=5.0, help="Entry threshold bp")
    p.add_argument("--exit-threshold", type=float, default=1.0, help="Exit when |dev| < this")
    p.add_argument("--sustain-s", type=float, default=2.0, help="Deviation must persist for this long")
    p.add_argument("--hold-max-s", type=float, default=60.0)
    p.add_argument("--kill-pnl", type=float, default=-5.0)
    p.add_argument("--duration-min", type=float, default=60)
    args = p.parse_args()

    proxies = (Path(__file__).parent / "proxies.txt").read_text().splitlines()
    proxies = [p for p in proxies if p.strip()]
    fc = FlipsterClient(FBM(), proxies=proxies)
    ver = json.loads(urllib.request.urlopen("http://localhost:9230/json/version", timeout=5).read())
    cookies = await _fetch_cookies(ver["webSocketDebuggerUrl"])
    import requests
    fc._cookies = cookies
    fc._session = requests.Session()
    fc._session.cookies.update(cookies)
    fc._session.headers.update({
        "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0",
        "Accept": "application/json, text/plain, */*",
        "Referer": "https://flipster.io/", "Content-Type": "application/json",
        "X-Prex-Client-Platform": "web", "X-Prex-Client-Version": "release-web-3.15.110",
        "Origin": "https://flipster.io",
    })

    symbols = [s.strip() for s in args.symbols.split(",") if s.strip() in SYMBOLS_CFG]
    state = BotState(
        symbols=symbols, amount_usd=args.amount_usd, leverage=args.leverage,
        threshold_bp=args.threshold, exit_threshold_bp=args.exit_threshold,
        sustain_secs=args.sustain_s, hold_max_s=args.hold_max_s,
        kill_pnl_usd=args.kill_pnl, fc=fc,
    )
    state.log_path.parent.mkdir(parents=True, exist_ok=True)
    for b in symbols:
        state.sym[b] = SymbolState(base=b)

    def log(msg):
        ts = datetime.now().strftime("%H:%M:%S")
        print(f"{ts} {msg}", flush=True)

    log(f"=== SLOW STAT-ARB START ===")
    log(f"  symbols: {symbols}")
    log(f"  amount=${args.amount_usd} lev={args.leverage}x  threshold={args.threshold}bp exit={args.exit_threshold}bp")
    log(f"  sustain={args.sustain_s}s hold_max={args.hold_max_s}s  kill_pnl=${args.kill_pnl}")
    log(f"  duration={args.duration_min}min")

    tasks = [
        asyncio.create_task(binance_feed(state, log)),
        asyncio.create_task(gate_feed(state, log)),
        asyncio.create_task(bitget_feed(state, log)),
        asyncio.create_task(flipster_feed(state, log)),
        asyncio.create_task(strategy_loop(state, log, args.duration_min * 60)),
    ]
    try:
        await tasks[4]
    finally:
        for t in tasks[:4]: t.cancel()
        log(f"\n=== SHUTDOWN ===")
        log(f"  signals={state.n_signals} filled={state.n_filled} wins={state.n_wins}")
        if state.n_filled:
            log(f"  win rate: {state.n_wins/state.n_filled*100:.1f}%")
        log(f"  realized PnL: ${state.realized_pnl:+.4f}")


if __name__ == "__main__":
    asyncio.run(main())

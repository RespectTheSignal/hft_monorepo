#!/usr/bin/env python3
"""Lead-Lag Bot v2 — multi-symbol, multi-leader, adverse-filter.

Improvements over v1:
  + Multi-symbol: BTC, ETH, SOL, XRP simultaneously
  + Multi-leader: Binance AND Gate must agree on direction
  + Adverse selection filter: skip if Flipster OBI opposes signal
  + Per-symbol position management (one open per symbol)
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
BINANCE_WS_BASE = "wss://fstream.binance.com/ws"
GATE_WS = "wss://fx-ws.gateio.ws/v4/ws/usdt"

# Symbol mapping
SYMBOLS_CFG = {
    # base: (binance_lower, gate, flipster)
    "BTC":  ("btcusdt",  "BTC_USDT",  "BTCUSDT.PERP"),
    "ETH":  ("ethusdt",  "ETH_USDT",  "ETHUSDT.PERP"),
    "SOL":  ("solusdt",  "SOL_USDT",  "SOLUSDT.PERP"),
    "XRP":  ("xrpusdt",  "XRP_USDT",  "XRPUSDT.PERP"),
    "BNB":  ("bnbusdt",  "BNB_USDT",  "BNBUSDT.PERP"),
    "DOGE": ("dogeusdt", "DOGE_USDT", "DOGEUSDT.PERP"),
    "ADA":  ("adausdt",  "ADA_USDT",  "ADAUSDT.PERP"),
    "AVAX": ("avaxusdt", "AVAX_USDT", "AVAXUSDT.PERP"),
    "LINK": ("linkusdt", "LINK_USDT", "LINKUSDT.PERP"),
    "LTC":  ("ltcusdt",  "LTC_USDT",  "LTCUSDT.PERP"),
}


@dataclass
class Feed:
    """Rolling mid window."""
    history: deque = field(default_factory=lambda: deque(maxlen=200))
    latest_mid: float = 0.0
    latest_ts: float = 0.0

    def update(self, ts_ms: int, bid: float, ask: float):
        mid = (bid + ask) / 2
        self.history.append((ts_ms, mid))
        self.latest_mid = mid
        self.latest_ts = ts_ms

    def cumulative_return_bp(self, window_ms: int = 500) -> float | None:
        if len(self.history) < 2:
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
class FlipsterDepth:
    bid: float = 0.0
    ask: float = 0.0
    ts: float = 0.0
    bs5: float = 0.0   # cumulative bid size top-5
    as5: float = 0.0

    @property
    def imbalance(self) -> float:
        if self.bs5 + self.as5 == 0:
            return 0
        return (self.bs5 - self.as5) / (self.bs5 + self.as5)


@dataclass
class SymbolState:
    base: str
    binance: Feed = field(default_factory=Feed)
    gate: Feed = field(default_factory=Feed)
    flip: FlipsterDepth = field(default_factory=FlipsterDepth)
    in_position: bool = False
    entry_slot: int | None = None
    entry_price: float = 0.0
    entry_time: float = 0.0
    entry_side: Side | None = None
    last_signal_time: float = 0.0


@dataclass
class BotState:
    symbols: list[str]
    amount_usd: float
    leverage: int
    threshold_bp: float
    hold_ms: int
    obi_adverse: float    # if obi against direction > this, skip
    kill_pnl_usd: float

    sym_state: dict = field(default_factory=dict)
    realized_pnl: float = 0.0
    n_attempts: int = 0
    n_filled: int = 0
    n_wins: int = 0
    n_skipped_adverse: int = 0
    n_skipped_no_gate_confirm: int = 0
    fc: FlipsterClient = None

    log_path: Path = field(default_factory=lambda: Path("logs/leadlag_v2.jsonl"))


# ---------------------------------------------------------------------------
# Strategy logic per symbol
# ---------------------------------------------------------------------------

async def maybe_enter(state: BotState, base: str, log):
    s = state.sym_state[base]
    if s.in_position: return
    if time.time() - s.last_signal_time < 0.3: return  # cooldown 300ms (per-symbol)
    if not s.flip.bid or not s.binance.latest_mid: return
    if time.time() * 1000 - s.flip.ts > 1000: return  # stale flipster

    # 1. Binance signal
    bin_bp = s.binance.cumulative_return_bp(500)
    if bin_bp is None or abs(bin_bp) < state.threshold_bp:
        return

    # 2. Multi-leader: Gate must NOT oppose direction (looser than confirm).
    # Only block if gate moved meaningfully OPPOSITE.
    gate_bp = s.gate.cumulative_return_bp(500) if s.gate.latest_mid else None
    if gate_bp is None:
        gate_confirmed = True  # no data → don't block
    elif (gate_bp > 0) == (bin_bp > 0):
        gate_confirmed = True  # same direction (any magnitude)
    elif abs(gate_bp) < 1.0:
        gate_confirmed = True  # gate moved opposite but tiny → ignore
    else:
        gate_confirmed = False  # gate moved meaningfully opposite → skip

    if not gate_confirmed:
        state.n_skipped_no_gate_confirm += 1
        gate_str = f"{gate_bp:+.2f}" if gate_bp is not None else "N/A"
        log(f"[{base}] SKIP: bin {bin_bp:+.2f} but gate {gate_str} not confirming")
        s.last_signal_time = time.time()
        return

    # 3. Adverse filter: Flipster OBI must NOT oppose direction
    obi = s.flip.imbalance
    direction = 1 if bin_bp > 0 else -1
    if direction * obi < -state.obi_adverse:  # obi against direction
        state.n_skipped_adverse += 1
        log(f"[{base}] SKIP adverse: bin {bin_bp:+.2f} but Flipster OBI {obi:+.2f}")
        s.last_signal_time = time.time()
        return

    # All filters passed → enter
    side = Side.LONG if bin_bp > 0 else Side.SHORT
    ref = s.flip.ask if side == Side.LONG else s.flip.bid
    s.last_signal_time = time.time()
    state.n_attempts += 1
    gate_str = f"{gate_bp:+.2f}" if gate_bp is not None else "N/A"
    log(f"[{base} ENTER #{state.n_attempts}] bin={bin_bp:+.2f}bp gate={gate_str} obi={obi:+.2f} side={side.value} ref=${ref}")

    try:
        params = OrderParams(side=side, amount_usd=state.amount_usd,
                             order_type=OrderType.MARKET, leverage=state.leverage)
        sym = SYMBOLS_CFG[base][2]
        r = await asyncio.to_thread(state.fc.place_order, sym, params, ref)
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
    state.n_filled += 1
    log(f"  ✓ filled slot={slot} @ ${avg}")


async def maybe_exit(state: BotState, base: str, log):
    s = state.sym_state[base]
    if not s.in_position: return
    elapsed = (time.time() - s.entry_time) * 1000
    if elapsed < state.hold_ms: return

    ref = s.flip.bid if s.entry_side == Side.LONG else s.flip.ask
    sym = SYMBOLS_CFG[base][2]
    close_ok = False
    avg_close = None
    offsets = [1.0, 0.998, 1.002, 0.99, 1.01] if s.entry_side == Side.LONG else [1.0, 1.002, 0.998, 1.01, 0.99]
    for off in offsets:
        try:
            try_px = ref * off
            r = await asyncio.to_thread(state.fc.close_position, sym, s.entry_slot, try_px)
            avg_close = float(r.get("order", {}).get("avgPrice", try_px))
            close_ok = True
            break
        except Exception as e:
            if "InsufficientLiquidity" in str(e): continue
            log(f"  ✗ close err: {str(e)[:100]}")
            break

    if not close_ok:
        log(f"  ⚠ {base} close all retries failed")
        return

    sign = 1 if s.entry_side == Side.LONG else -1
    pnl_bp = (avg_close - s.entry_price) / s.entry_price * 1e4 * sign
    pnl_usd = pnl_bp * state.amount_usd / 1e4
    state.realized_pnl += pnl_usd
    if pnl_bp > 0: state.n_wins += 1

    log(f"[{base} EXIT] hold={elapsed:.0f}ms entry=${s.entry_price} exit=${avg_close} pnl={pnl_bp:+.2f}bp (${pnl_usd:+.4f})")
    log(f"  TOTAL: filled={state.n_filled} wins={state.n_wins} ({state.n_wins/state.n_filled*100:.0f}%) pnl=${state.realized_pnl:+.4f}")
    log(f"  skipped: adverse={state.n_skipped_adverse} no-gate={state.n_skipped_no_gate_confirm}")

    with open(state.log_path, "a") as f:
        f.write(json.dumps({
            "ts": datetime.now(timezone.utc).isoformat(),
            "base": base, "side": s.entry_side.value,
            "entry": s.entry_price, "exit": avg_close,
            "amount_usd": state.amount_usd, "hold_ms": elapsed,
            "pnl_bp": pnl_bp, "pnl_usd": pnl_usd,
        }) + "\n")

    s.in_position = False
    s.entry_slot = None
    s.entry_side = None

    if state.realized_pnl < state.kill_pnl_usd:
        log(f"⛔ KILL: ${state.realized_pnl:.2f}")
        raise SystemExit(0)


# ---------------------------------------------------------------------------
# Feeds (binance, gate, flipster)
# ---------------------------------------------------------------------------

async def binance_feed(state: BotState, log):
    streams = "/".join(f"{SYMBOLS_CFG[b][0]}@bookTicker" for b in state.symbols)
    url = f"{BINANCE_WS_BASE}/{streams}"
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
                    ts = int(d.get("T", time.time() * 1000))
                    if bid > 0 and ask > 0:
                        state.sym_state[base].binance.update(ts, bid, ask)
        except Exception as e:
            log(f"[binance] err {e}, retry 3s")
            await asyncio.sleep(3)


async def gate_feed(state: BotState, log):
    while True:
        try:
            async with websockets.connect(GATE_WS, ping_interval=20) as ws:
                log("[gate] connected")
                # Subscribe to book_ticker for each symbol
                for base in state.symbols:
                    sub = {
                        "time": int(time.time()),
                        "channel": "futures.book_ticker",
                        "event": "subscribe",
                        "payload": [SYMBOLS_CFG[base][1]],
                    }
                    await ws.send(json.dumps(sub))
                async for msg in ws:
                    try: d = json.loads(msg)
                    except: continue
                    if d.get("channel") != "futures.book_ticker": continue
                    if d.get("event") != "update": continue
                    res = d.get("result", {})
                    sym = res.get("s", "")
                    base = next((b for b, c in SYMBOLS_CFG.items() if c[1] == sym), None)
                    if not base: continue
                    bid = float(res.get("b", 0)); ask = float(res.get("a", 0))
                    ts = int(res.get("t", time.time() * 1000))
                    if bid > 0 and ask > 0:
                        state.sym_state[base].gate.update(ts, bid, ask)
        except Exception as e:
            log(f"[gate] err {e}, retry 3s")
            await asyncio.sleep(3)


async def flipster_feed(state: BotState, log):
    while True:
        try:
            ver = json.loads(urllib.request.urlopen("http://localhost:9230/json/version", timeout=5).read())
            cookies = await _fetch_cookies(ver["webSocketDebuggerUrl"])
            cookie_str = "; ".join(f"{k}={v}" for k, v in cookies.items())
            headers = {
                "Cookie": cookie_str, "Origin": "https://flipster.io",
                "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0",
            }
            async with websockets.connect(FLIP_V2_WS, additional_headers=headers, ping_interval=20) as ws:
                syms_flip = [SYMBOLS_CFG[b][2] for b in state.symbols]
                await ws.send(json.dumps({"s": {"market/orderbooks-v2": {"rows": syms_flip}}}))
                log(f"[flipster] connected ({len(syms_flip)} symbols)")
                async for msg in ws:
                    try: d = json.loads(msg)
                    except: continue
                    s_data = d.get("t", {}).get("market/orderbooks-v2", {}).get("s", {})
                    for sym, book in s_data.items():
                        base = next((b for b, c in SYMBOLS_CFG.items() if c[2] == sym), None)
                        if not base: continue
                        bids = book.get("bids", [])[:5]
                        asks = book.get("asks", [])[:5]
                        if not bids or not asks: continue
                        fd = state.sym_state[base].flip
                        fd.bid = float(bids[0][0])
                        fd.ask = float(asks[0][0])
                        fd.bs5 = sum(float(b[1]) for b in bids)
                        fd.as5 = sum(float(a[1]) for a in asks)
                        fd.ts = time.time() * 1000
        except Exception as e:
            log(f"[flipster] err {e}, retry 3s")
            await asyncio.sleep(3)


async def strategy_loop(state: BotState, log, duration_s: float):
    end = time.time() + duration_s
    while time.time() < end:
        try:
            for base in state.symbols:
                await maybe_enter(state, base, log)
                await maybe_exit(state, base, log)
        except SystemExit:
            raise
        except Exception as e:
            log(f"[strategy] err: {e}")
        await asyncio.sleep(0.05)


async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--symbols", default="BTC,ETH,SOL,XRP")
    p.add_argument("--amount-usd", type=float, default=10.0)
    p.add_argument("--leverage", type=int, default=10)
    p.add_argument("--threshold", type=float, default=3.0)
    p.add_argument("--hold-ms", type=int, default=1000)
    p.add_argument("--obi-adverse", type=float, default=0.5,
                   help="If Flipster OBI opposes direction by more than this, skip")
    p.add_argument("--kill-pnl", type=float, default=-3.0)
    p.add_argument("--duration-min", type=float, default=60.0)
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
        threshold_bp=args.threshold, hold_ms=args.hold_ms,
        obi_adverse=args.obi_adverse, kill_pnl_usd=args.kill_pnl, fc=fc,
    )
    state.log_path.parent.mkdir(parents=True, exist_ok=True)
    for b in symbols:
        state.sym_state[b] = SymbolState(base=b)

    def log(msg):
        ts = datetime.now().strftime("%H:%M:%S.%f")[:-3]
        print(f"{ts} {msg}", flush=True)

    log(f"=== LEADLAG v2 START ===")
    log(f"  symbols: {symbols}  amount=${args.amount_usd}  lev={args.leverage}x  notional=${args.amount_usd*args.leverage}")
    log(f"  threshold={args.threshold}bp  hold={args.hold_ms}ms  obi_adverse={args.obi_adverse}")
    log(f"  kill_pnl=${args.kill_pnl}  duration={args.duration_min}min  proxies={len(proxies)}")

    tasks = [
        asyncio.create_task(binance_feed(state, log)),
        asyncio.create_task(gate_feed(state, log)),
        asyncio.create_task(flipster_feed(state, log)),
        asyncio.create_task(strategy_loop(state, log, args.duration_min * 60)),
    ]

    try:
        await tasks[3]
    finally:
        for t in tasks[:3]: t.cancel()
        log(f"\n=== SHUTDOWN ===")
        log(f"  attempts={state.n_attempts} filled={state.n_filled} wins={state.n_wins}")
        log(f"  skipped: adverse={state.n_skipped_adverse} no-gate={state.n_skipped_no_gate_confirm}")
        if state.n_filled:
            log(f"  win rate: {state.n_wins/state.n_filled*100:.1f}%")
        log(f"  realized PnL: ${state.realized_pnl:+.4f}")


if __name__ == "__main__":
    asyncio.run(main())

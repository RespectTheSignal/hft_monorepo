#!/usr/bin/env python3
"""Slow Stat-Arb v6 — BROWSER account (v2 cookies + proxies).

Uses flipster_web client with browser cookies for orders.
LIMIT orders via v2 API (orderType=ORDER_TYPE_LIMIT).
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import time
import urllib.error
import urllib.request
import uuid
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

# (binance_lower, gate, bitget, flipster, tick_size)
SYMBOLS_CFG = {
    "BTC":  ("btcusdt",  "BTC_USDT",  "BTCUSDT",  "BTCUSDT.PERP",  0.1),
    "ETH":  ("ethusdt",  "ETH_USDT",  "ETHUSDT",  "ETHUSDT.PERP",  0.01),
    "SOL":  ("solusdt",  "SOL_USDT",  "SOLUSDT",  "SOLUSDT.PERP",  0.001),
    "XRP":  ("xrpusdt",  "XRP_USDT",  "XRPUSDT",  "XRPUSDT.PERP",  0.0001),
    "DOGE": ("dogeusdt", "DOGE_USDT", "DOGEUSDT", "DOGEUSDT.PERP", 0.00001),
    "LINK": ("linkusdt", "LINK_USDT", "LINKUSDT", "LINKUSDT.PERP", 0.001),
    "AVAX": ("avaxusdt", "AVAX_USDT", "AVAXUSDT", "AVAXUSDT.PERP", 0.001),
    "LTC":  ("ltcusdt",  "LTC_USDT",  "LTCUSDT",  "LTCUSDT.PERP",  0.01),
    "INJ":  ("injusdt",  "INJ_USDT",  "INJUSDT",  "INJUSDT.PERP",  0.001),
    "SUI":  ("suiusdt",  "SUI_USDT",  "SUIUSDT",  "SUIUSDT.PERP",  0.0001),
    "APT":  ("aptusdt",  "APT_USDT",  "APTUSDT",  "APTUSDT.PERP",  0.0001),
}


@dataclass
class Mid:
    val: float = 0.0
    ts: float = 0.0


@dataclass
class FlipsterTOB:
    bid: float = 0.0
    ask: float = 0.0
    ts: float = 0.0


@dataclass
class SymbolState:
    base: str
    flip_tob: FlipsterTOB = field(default_factory=FlipsterTOB)
    binance: Mid = field(default_factory=Mid)
    gate: Mid = field(default_factory=Mid)
    bitget: Mid = field(default_factory=Mid)
    dev_history: deque = field(default_factory=lambda: deque(maxlen=10))

    state: str = "IDLE"  # IDLE | ENTRY_PENDING | IN_POSITION | EXIT_PENDING
    entry_slot: int | None = None
    entry_order_id: str | None = None
    entry_order_time: float = 0.0
    entry_price: float = 0.0
    entry_side: Side | None = None
    entry_amount: float = 0.0
    entry_dev: float = 0.0
    in_position_since: float = 0.0
    exit_order_id: str | None = None
    exit_order_time: float = 0.0
    cooldown_until: float = 0.0

    def consensus(self) -> float | None:
        now = time.time()
        for m in [self.binance, self.gate, self.bitget]:
            if m.val == 0 or now - m.ts > 5: return None
        return (self.binance.val + self.gate.val + self.bitget.val) / 3

    def deviation_bp(self) -> float | None:
        flip_mid = (self.flip_tob.bid + self.flip_tob.ask) / 2
        if flip_mid == 0 or time.time() - self.flip_tob.ts > 2: return None
        cons = self.consensus()
        if cons is None: return None
        return (flip_mid - cons) / cons * 1e4


@dataclass
class BotState:
    symbols: list[str]
    amount_usd: float
    leverage: int
    threshold_bp: float
    sustain_secs: float
    fill_wait_s: float
    hold_max_s: float
    exit_threshold_bp: float
    kill_pnl_usd: float

    sym: dict = field(default_factory=dict)
    realized_pnl: float = 0.0
    n_signals: int = 0
    n_filled: int = 0
    n_cancelled: int = 0
    n_wins: int = 0
    fc: FlipsterClient = None
    log_path: Path = field(default_factory=lambda: Path("logs/slow_statarb_browser.jsonl"))


# ---------------------------------------------------------------------------
# Flipster v2 helpers (using browser cookies)
# ---------------------------------------------------------------------------

async def has_position(state: BotState, base: str) -> bool:
    """Check if Flipster has open position. Uses raw API call."""
    flip_sym = SYMBOLS_CFG[base][3]
    def _call():
        try:
            r = state.fc._session.get(
                f"https://api.flipster.io/api/v2/trade/positions/{flip_sym}",
                proxies=state.fc._next_proxy(), timeout=8,
            )
            if r.status_code == 200:
                d = r.json()
                pos = d.get("position", d)
                qty = pos.get("size") or pos.get("positionQty")
                return qty and float(qty) != 0
        except Exception:
            return None
        return False
    return await asyncio.to_thread(_call)


async def step_symbol(state: BotState, base: str, log):
    s = state.sym[base]
    flip_sym = SYMBOLS_CFG[base][3]

    if s.state == "IDLE":
        if time.time() < s.cooldown_until: return
        dev = s.deviation_bp()
        if dev is None: return
        s.dev_history.append((time.time(), dev))
        if abs(dev) < state.threshold_bp: return

        cutoff = time.time() - state.sustain_secs
        recent = [d for t, d in s.dev_history if t >= cutoff]
        if len(recent) < 2: return
        same_sign = sum(1 for d in recent if (d > 0) == (dev > 0)) / len(recent)
        avg_dev = sum(recent) / len(recent)
        if same_sign < 0.7 or abs(avg_dev) < state.threshold_bp: return

        # Direction: dev > 0 → Flipster overpriced → SHORT
        side = Side.SHORT if dev > 0 else Side.LONG
        # Place AT TOB on opposite side (likely fills instantly as taker but at GOOD price)
        # This captures SPREAD improvement vs market order
        # SHORT: SELL at TOB ASK (we sell at the highest active offer)
        # LONG: BUY at TOB BID (we buy at the lowest active bid - actually impossible, this would sit)
        # Wait actually: for taker action via limit:
        #   SHORT (sell): hit BID = SELL at BID
        #   LONG (buy): hit ASK = BUY at ASK
        # For "instant fill" with spread improvement:
        #   SHORT: SELL at ASK (sits at TOB; cross only if ASK changes UP since cross BID would be loss)
        #   LONG: BUY at BID (sits at TOB)
        # Actually simplest: place at MID (between bid/ask, never crosses, may sit as inside-spread maker)
        mid = (s.flip_tob.bid + s.flip_tob.ask) / 2
        # Round to symbol's tick to avoid InvalidPrice
        tick = SYMBOLS_CFG[base][4]
        price = round(mid / tick) * tick
        # Ensure not crossing — adjust if needed
        if side == Side.SHORT and price < s.flip_tob.ask:
            price = s.flip_tob.ask  # don't sell below ask (would cross/take)
        if side == Side.LONG and price > s.flip_tob.bid:
            price = s.flip_tob.bid

        state.n_signals += 1
        log(f"[{base} ENTRY #{state.n_signals}] dev={dev:+.2f}bp side={side.value} px={price} (mid-pegged)")

        try:
            params = OrderParams(side=side, amount_usd=state.amount_usd,
                                 order_type=OrderType.LIMIT, price=price,
                                 leverage=state.leverage, post_only=True)
            r = await asyncio.to_thread(state.fc.place_order, flip_sym, params, price)
        except Exception as e:
            log(f"  ✗ entry: {str(e)[:120]}")
            s.cooldown_until = time.time() + 5
            return

        # Response shape varies. Get slot/orderId
        pos = r.get("position", {})
        order = r.get("order", {})
        slot = pos.get("slot")
        order_id = order.get("orderId")
        avg_price = pos.get("avgPrice") or order.get("avgPrice")
        # Check if instantly filled (status=Filled or position has size)
        status = order.get("action") or order.get("status", "")
        size = pos.get("size") or "0"

        if size and float(size) != 0:
            # Instantly filled (price was crossed)
            log(f"  ✓ instantly FILLED slot={slot} avg=${avg_price}")
            s.state = "IN_POSITION"
            s.entry_slot = slot
            s.entry_price = float(avg_price)
            s.entry_side = side
            s.entry_amount = state.amount_usd
            s.in_position_since = time.time()
            state.n_filled += 1
        else:
            # Sitting as limit
            log(f"  ⏳ limit placed slot={slot} order_id={order_id[:8] if order_id else '?'}")
            s.state = "ENTRY_PENDING"
            s.entry_slot = slot
            s.entry_order_id = order_id
            s.entry_order_time = time.time()
            s.entry_price = price
            s.entry_side = side
            s.entry_amount = state.amount_usd
            s.entry_dev = dev
        return

    if s.state == "ENTRY_PENDING":
        elapsed = time.time() - s.entry_order_time
        # Poll position
        has_pos = await has_position(state, base)
        if has_pos:
            log(f"[{base}] entry FILLED")
            s.state = "IN_POSITION"
            s.in_position_since = time.time()
            state.n_filled += 1
            return

        if elapsed > state.fill_wait_s:
            # Timeout — try to cancel via close (size=0 PUT works for limit too sometimes)
            log(f"[{base}] entry timeout {elapsed:.0f}s → trying cancel")
            # Best effort: try the close endpoint
            try:
                await asyncio.to_thread(state.fc.close_position, flip_sym, s.entry_slot, s.flip_tob.bid)
                log(f"  cancel attempt sent")
            except Exception as e:
                log(f"  cancel err: {str(e)[:80]}")
            state.n_cancelled += 1
            s.state = "IDLE"
            s.entry_slot = None
            s.entry_order_id = None
            s.cooldown_until = time.time() + 5
        return

    if s.state == "IN_POSITION":
        elapsed = time.time() - s.in_position_since
        dev_now = s.deviation_bp()

        # Compute current unrealized PnL bp (using flipster mid)
        flip_mid = (s.flip_tob.bid + s.flip_tob.ask) / 2
        sign = 1 if s.entry_side == Side.LONG else -1
        unrealized_bp = (flip_mid - s.entry_price) / s.entry_price * 1e4 * sign

        # Exit conditions:
        # 1. Unrealized > MIN_PROFIT_BP (real profit beats fee)
        # 2. Hold timeout
        # 3. Stop loss (large loss)
        should_exit = False
        reason = ""
        MIN_PROFIT_BP = 3.0   # need at least 3bp gross to beat ~0.85bp fee + spread cost
        STOP_LOSS_BP = -8.0   # cut losses if too negative
        if unrealized_bp > MIN_PROFIT_BP:
            should_exit = True; reason = f"profit_{unrealized_bp:.1f}bp"
        elif unrealized_bp < STOP_LOSS_BP:
            should_exit = True; reason = f"stop_{unrealized_bp:.1f}bp"
        elif elapsed > state.hold_max_s:
            should_exit = True; reason = f"timeout_{unrealized_bp:.1f}bp"

        if not should_exit: return

        # Place exit LIMIT (postOnly maker) on opposite side
        # SHORT exit = BUY at TOB BID (sit there, hope to be hit by sellers)
        # LONG exit = SELL at TOB ASK
        if s.entry_side == Side.SHORT:
            exit_side = Side.LONG
            price = s.flip_tob.bid
        else:
            exit_side = Side.SHORT
            price = s.flip_tob.ask

        dev_str = f"{dev_now:+.2f}" if dev_now is not None else "N/A"
        log(f"[{base} EXIT-LIMIT-{reason}] dev={dev_str}bp side={exit_side.value} px={price}")

        # Try maker close first via close endpoint
        try:
            r = await asyncio.to_thread(state.fc.close_position, flip_sym, s.entry_slot, price)
            order = r.get("order", {})
            avg_close = float(order.get("avgPrice", price))
            # Check if actually filled (avgPrice 0 means not filled)
            if avg_close > 0 and abs(avg_close - price) / price < 0.01:
                await record_exit(state, base, avg_close, reason, log)
                return
            else:
                log(f"  close didn't fill (avg={avg_close}), retry with market")
        except Exception as e:
            log(f"  ✗ exit close: {str(e)[:120]}")

        # Force close with market-like price (cross spread)
        try:
            if s.entry_side == Side.SHORT:
                extreme_px = s.flip_tob.ask * 1.005  # buy aggressive
            else:
                extreme_px = s.flip_tob.bid * 0.995  # sell aggressive
            r = await asyncio.to_thread(state.fc.close_position, flip_sym, s.entry_slot, extreme_px)
            avg_close = float(r.get("order", {}).get("avgPrice", extreme_px))
            log(f"  ✓ forced close @ {avg_close}")
            await record_exit(state, base, avg_close, f"{reason}-force", log)
        except Exception as e2:
            log(f"  ✗ force close also failed: {str(e2)[:80]}")


async def record_exit(state: BotState, base: str, avg_close: float, reason: str, log):
    s = state.sym[base]
    sign = 1 if s.entry_side == Side.LONG else -1
    pnl_bp = (avg_close - s.entry_price) / s.entry_price * 1e4 * sign
    pnl_usd = pnl_bp * s.entry_amount / 1e4
    state.realized_pnl += pnl_usd
    if pnl_bp > 0: state.n_wins += 1

    hold = time.time() - s.in_position_since
    log(f"  pnl={pnl_bp:+.2f}bp (${pnl_usd:+.4f}) hold={hold:.0f}s reason={reason}")
    log(f"  TOTAL: signals={state.n_signals} filled={state.n_filled} cancelled={state.n_cancelled} wins={state.n_wins} pnl=${state.realized_pnl:+.4f}")

    with open(state.log_path, "a") as f:
        f.write(json.dumps({
            "ts": datetime.now(timezone.utc).isoformat(),
            "base": base, "side": s.entry_side.value, "reason": reason,
            "entry": s.entry_price, "exit": avg_close,
            "amount_usd": s.entry_amount, "hold_s": hold,
            "pnl_bp": pnl_bp, "pnl_usd": pnl_usd,
        }) + "\n")

    s.state = "IDLE"
    s.entry_slot = None
    s.entry_order_id = None
    s.cooldown_until = time.time() + 3

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
            log(f"[binance] err: retry 3s")
            await asyncio.sleep(3)


async def gate_feed(state: BotState, log):
    while True:
        try:
            async with websockets.connect(GATE_WS, ping_interval=20) as ws:
                log("[gate] connected")
                for base in state.symbols:
                    await ws.send(json.dumps({"time": int(time.time()), "channel": "futures.book_ticker",
                                              "event": "subscribe", "payload": [SYMBOLS_CFG[base][1]]}))
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
            log(f"[gate] err: retry 3s")
            await asyncio.sleep(3)


async def bitget_feed(state: BotState, log):
    while True:
        try:
            async with websockets.connect(BITGET_WS, ping_interval=20) as ws:
                log("[bitget] connected")
                args = [{"instType": "USDT-FUTURES", "channel": "ticker", "instId": SYMBOLS_CFG[b][2]}
                        for b in state.symbols]
                await ws.send(json.dumps({"op": "subscribe", "args": args}))
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
            log(f"[bitget] err: retry 3s")
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
                syms = [SYMBOLS_CFG[b][3] for b in state.symbols]
                await ws.send(json.dumps({"s": {"market/orderbooks-v2": {"rows": syms}}}))
                log(f"[flipster] connected ({len(syms)} syms)")
                async for msg in ws:
                    try: d = json.loads(msg)
                    except: continue
                    sd = d.get("t", {}).get("market/orderbooks-v2", {}).get("s", {})
                    for sym, book in sd.items():
                        base = next((b for b, c in SYMBOLS_CFG.items() if c[3] == sym), None)
                        if not base: continue
                        bids = book.get("bids", []); asks = book.get("asks", [])
                        if bids and asks:
                            t = state.sym[base].flip_tob
                            t.bid = float(bids[0][0])
                            t.ask = float(asks[0][0])
                            t.ts = time.time()
        except Exception as e:
            log(f"[flipster] err: retry 3s")
            await asyncio.sleep(3)


async def strategy_loop(state: BotState, log, duration_s):
    end = time.time() + duration_s
    last_status = 0
    while time.time() < end:
        try:
            for base in state.symbols:
                await step_symbol(state, base, log)
            if time.time() - last_status > 60:
                last_status = time.time()
                stats = []
                for b in state.symbols:
                    d = state.sym[b].deviation_bp()
                    st = state.sym[b].state
                    stats.append(f"{b}={d:+.1f}/{st}" if d is not None else f"{b}=N/A/{st}")
                log(f"  [status] {' | '.join(stats)}  s/f/c={state.n_signals}/{state.n_filled}/{state.n_cancelled} pnl=${state.realized_pnl:+.4f}")
        except SystemExit: raise
        except Exception as e: log(f"[strategy] err: {e}")
        await asyncio.sleep(0.5)


async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--symbols", default="ETH,XRP,DOGE,SOL,BTC,LINK,AVAX,LTC,SUI,APT")
    p.add_argument("--amount-usd", type=float, default=50)
    p.add_argument("--leverage", type=int, default=10)
    p.add_argument("--threshold", type=float, default=4.0)
    p.add_argument("--exit-threshold", type=float, default=1.0)
    p.add_argument("--sustain-s", type=float, default=1.0)
    p.add_argument("--fill-wait-s", type=float, default=10.0)
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
        sustain_secs=args.sustain_s, fill_wait_s=args.fill_wait_s,
        hold_max_s=args.hold_max_s, kill_pnl_usd=args.kill_pnl, fc=fc,
    )
    state.log_path.parent.mkdir(parents=True, exist_ok=True)
    for b in symbols: state.sym[b] = SymbolState(base=b)

    def log(msg):
        ts = datetime.now().strftime("%H:%M:%S")
        print(f"{ts} {msg}", flush=True)

    log(f"=== SLOW STAT-ARB BROWSER (LIMIT/MAKER) START ===")
    log(f"  symbols: {symbols}")
    log(f"  amount=${args.amount_usd} lev={args.leverage}x threshold={args.threshold}bp exit={args.exit_threshold}bp")
    log(f"  sustain={args.sustain_s}s fill_wait={args.fill_wait_s}s hold_max={args.hold_max_s}s")
    log(f"  account: BROWSER (cookies + {len(proxies)} proxies)")

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
        log(f"  signals={state.n_signals} filled={state.n_filled} cancelled={state.n_cancelled} wins={state.n_wins}")
        if state.n_filled:
            log(f"  win rate: {state.n_wins/state.n_filled*100:.1f}%")
        log(f"  realized PnL: ${state.realized_pnl:+.4f}")


if __name__ == "__main__":
    asyncio.run(main())

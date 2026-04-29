#!/usr/bin/env python3
"""Slow Stat-Arb v6 — MAKER orders.

Same signal as slow_statarb.py but uses LIMIT GTC orders at TOB:
  - SHORT signal: SELL LIMIT at TOB ASK (maker — sits on book)
  - LONG signal:  BUY LIMIT at TOB BID  (maker)
  - Wait MAX_FILL_WAIT_S → if filled continue, else cancel
  - Exit similarly with maker, fall back to market on timeout

Uses Flipster v1 HMAC API ($13k account) — supports LIMIT + cancel cleanly.
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
BINANCE_WS = "wss://fstream.binance.com/ws"
GATE_WS = "wss://fx-ws.gateio.ws/v4/ws/usdt"
BITGET_WS = "wss://ws.bitget.com/v2/ws/public"

# (base, binance_lower, gate, bitget, flipster, base_qty_round_decimals)
SYMBOLS_CFG = {
    "BTC":  ("btcusdt",  "BTC_USDT",  "BTCUSDT",  "BTCUSDT.PERP",  5),
    "ETH":  ("ethusdt",  "ETH_USDT",  "ETHUSDT",  "ETHUSDT.PERP",  4),
    "SOL":  ("solusdt",  "SOL_USDT",  "SOLUSDT",  "SOLUSDT.PERP",  2),
    "XRP":  ("xrpusdt",  "XRP_USDT",  "XRPUSDT",  "XRPUSDT.PERP",  1),
    "DOGE": ("dogeusdt", "DOGE_USDT", "DOGEUSDT", "DOGEUSDT.PERP", 0),
    "LINK": ("linkusdt", "LINK_USDT", "LINKUSDT", "LINKUSDT.PERP", 2),
    "AVAX": ("avaxusdt", "AVAX_USDT", "AVAXUSDT", "AVAXUSDT.PERP", 2),
    "LTC":  ("ltcusdt",  "LTC_USDT",  "LTCUSDT",  "LTCUSDT.PERP",  3),
    "INJ":  ("injusdt",  "INJ_USDT",  "INJUSDT",  "INJUSDT.PERP",  2),
    "SUI":  ("suiusdt",  "SUI_USDT",  "SUIUSDT",  "SUIUSDT.PERP",  1),
    "APT":  ("aptusdt",  "APT_USDT",  "APTUSDT",  "APTUSDT.PERP",  2),
}


# ---------------------------------------------------------------------------
# HMAC v1 API
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
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, {"_error": e.read().decode()[:200]}
    except Exception as e:
        return 0, {"_error": str(e)[:200]}


async def http(method, path, body=None):
    return await asyncio.to_thread(_http_sync, method, path, body)


async def place_limit(symbol: str, side: str, qty: str, price: str) -> dict:
    """Place LIMIT GTC order — sits on book as maker until filled or cancelled."""
    body = {
        "symbol": symbol, "side": side, "type": "LIMIT",
        "quantity": qty, "price": price, "timeInForce": "GTC",
    }
    s, r = await http("POST", "/api/v1/trade/order", body)
    if s == 200:
        return r.get("order", r)
    return {"_error": r.get("_error", f"http {s}"), "_status": s}


async def place_market(symbol: str, side: str, qty: str) -> dict:
    body = {"symbol": symbol, "side": side, "type": "MARKET", "quantity": qty}
    s, r = await http("POST", "/api/v1/trade/order", body)
    if s == 200:
        return r.get("order", r)
    return {"_error": r.get("_error", f"http {s}"), "_status": s}


async def cancel_order(symbol: str, order_id: str) -> bool:
    s, _ = await http("DELETE", "/api/v1/trade/order",
                      {"symbol": symbol, "orderId": order_id})
    return s == 200


async def get_position_qty(symbol: str) -> float:
    s, r = await http("GET", "/api/v1/account/position")
    if s != 200 or not isinstance(r, list):
        return 0.0
    for p in r:
        if p.get("symbol") == symbol and p.get("positionQty") not in (None, "0"):
            qty_str = p.get("positionQty", "0")
            qty = float(qty_str) if qty_str else 0.0
            if p.get("positionSide") == "SHORT":
                qty = -abs(qty)
            else:
                qty = abs(qty)
            return qty
    return 0.0


# ---------------------------------------------------------------------------
# State
# ---------------------------------------------------------------------------

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

    # Order lifecycle
    state: str = "IDLE"  # IDLE | ENTRY_PENDING | IN_POSITION | EXIT_PENDING
    entry_order_id: str | None = None
    entry_order_time: float = 0.0
    entry_price: float = 0.0
    entry_side: str = ""  # BUY/SELL
    entry_qty: float = 0.0
    entry_dev: float = 0.0

    exit_order_id: str | None = None
    exit_order_time: float = 0.0
    in_position_since: float = 0.0

    last_signal_time: float = 0.0
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
    threshold_bp: float
    sustain_secs: float
    fill_wait_s: float       # max time to wait for entry fill
    hold_max_s: float        # total max hold time
    exit_threshold_bp: float
    kill_pnl_usd: float

    sym: dict = field(default_factory=dict)
    realized_pnl: float = 0.0
    n_signals: int = 0
    n_filled: int = 0
    n_cancelled: int = 0
    n_wins: int = 0
    log_path: Path = field(default_factory=lambda: Path("logs/slow_statarb_maker.jsonl"))


# ---------------------------------------------------------------------------
# Strategy state machine per symbol
# ---------------------------------------------------------------------------

async def step_symbol(state: BotState, base: str, log):
    s = state.sym[base]
    flip_sym = SYMBOLS_CFG[base][3]
    qty_decimals = SYMBOLS_CFG[base][4]

    # === IDLE: look for signal ===
    if s.state == "IDLE":
        if time.time() < s.cooldown_until: return
        dev = s.deviation_bp()
        if dev is None: return
        s.dev_history.append((time.time(), dev))
        if abs(dev) < state.threshold_bp: return

        # Sustain check
        cutoff = time.time() - state.sustain_secs
        recent = [d for t, d in s.dev_history if t >= cutoff]
        if len(recent) < 2: return
        same_sign = sum(1 for d in recent if (d > 0) == (dev > 0)) / len(recent)
        avg_dev = sum(recent) / len(recent)
        if same_sign < 0.7 or abs(avg_dev) < state.threshold_bp: return

        # Direction: dev > 0 means Flipster overpriced → SHORT
        side = "SELL" if dev > 0 else "BUY"
        # Maker placement: SELL at ASK (sits there), BUY at BID (sits there)
        price = s.flip_tob.ask if side == "SELL" else s.flip_tob.bid
        # Compute qty from amount_usd and price
        qty = round(state.amount_usd / price, qty_decimals)
        if qty <= 0: return
        price_str = f"{price:.{6 if price < 1 else 4 if price < 100 else 2}f}"
        qty_str = f"{qty:.{qty_decimals}f}"

        s.last_signal_time = time.time()
        state.n_signals += 1
        log(f"[{base} ENTRY-LIMIT #{state.n_signals}] dev={dev:+.2f}bp side={side} px={price_str} qty={qty_str}")

        r = await place_limit(flip_sym, side, qty_str, price_str)
        if r.get("_error"):
            log(f"  ✗ entry: {r['_error'][:120]}")
            s.cooldown_until = time.time() + 5
            return

        s.state = "ENTRY_PENDING"
        s.entry_order_id = r.get("orderId")
        s.entry_order_time = time.time()
        s.entry_side = side
        s.entry_qty = qty
        s.entry_dev = dev
        s.entry_price = price  # intended price
        log(f"  ✓ placed order_id={s.entry_order_id[:8]}")
        return

    # === ENTRY_PENDING: check fill ===
    if s.state == "ENTRY_PENDING":
        elapsed = time.time() - s.entry_order_time

        # Check actual position
        pos = await get_position_qty(flip_sym)
        sign = 1 if s.entry_side == "BUY" else -1
        target_pos = sign * s.entry_qty
        # If position close to target → filled
        if abs(pos - target_pos) < s.entry_qty * 0.1:
            log(f"[{base}] entry FILLED pos={pos}")
            s.state = "IN_POSITION"
            s.in_position_since = time.time()
            state.n_filled += 1
            return

        # Timeout → cancel
        if elapsed > state.fill_wait_s:
            ok = await cancel_order(flip_sym, s.entry_order_id)
            log(f"[{base}] entry timeout {elapsed:.0f}s → cancel ok={ok}")
            state.n_cancelled += 1
            s.state = "IDLE"
            s.entry_order_id = None
            s.cooldown_until = time.time() + 3
            return

        return

    # === IN_POSITION: place exit when dev reverts ===
    if s.state == "IN_POSITION":
        elapsed = time.time() - s.in_position_since
        dev_now = s.deviation_bp()

        # Conditions to exit:
        # 1. Dev reverted to small (converged)
        # 2. Hold max exceeded
        should_exit = False
        if dev_now is not None and abs(dev_now) < state.exit_threshold_bp:
            should_exit = True
            reason = "converged"
        elif elapsed > state.hold_max_s:
            should_exit = True
            reason = "timeout"

        if not should_exit:
            return

        # Place exit LIMIT (maker)
        exit_side = "BUY" if s.entry_side == "SELL" else "SELL"
        price = s.flip_tob.bid if exit_side == "SELL" else s.flip_tob.ask
        qty_str = f"{s.entry_qty:.{qty_decimals}f}"
        price_str = f"{price:.{6 if price < 1 else 4 if price < 100 else 2}f}"

        dev_str = f"{dev_now:+.2f}" if dev_now is not None else "N/A"
        log(f"[{base} EXIT-LIMIT-{reason}] dev={dev_str}bp side={exit_side} px={price_str}")
        r = await place_limit(flip_sym, exit_side, qty_str, price_str)
        if r.get("_error"):
            log(f"  ✗ exit limit: {r['_error'][:120]}")
            # Fall back to market
            log(f"  → trying MARKET exit")
            mr = await place_market(flip_sym, exit_side, qty_str)
            if mr.get("_error"):
                log(f"  ✗ exit market also failed: {mr['_error'][:80]}")
                return
            # Mark exit done
            await asyncio.sleep(0.5)
            await record_exit(state, base, mr, "market_force", log)
            return

        s.state = "EXIT_PENDING"
        s.exit_order_id = r.get("orderId")
        s.exit_order_time = time.time()
        log(f"  ✓ exit order placed {s.exit_order_id[:8]}")
        return

    # === EXIT_PENDING: check fill, fall back to market on timeout ===
    if s.state == "EXIT_PENDING":
        elapsed = time.time() - s.exit_order_time
        pos = await get_position_qty(flip_sym)
        if abs(pos) < s.entry_qty * 0.1:
            # Closed!
            log(f"[{base}] exit FILLED")
            await record_exit(state, base, None, "limit_filled", log)
            return

        if elapsed > state.fill_wait_s:
            # Cancel and market exit
            await cancel_order(flip_sym, s.exit_order_id)
            log(f"[{base}] exit limit timeout → MARKET fallback")
            exit_side = "BUY" if s.entry_side == "SELL" else "SELL"
            qty_str = f"{abs(pos):.{qty_decimals}f}"
            mr = await place_market(flip_sym, exit_side, qty_str)
            if mr.get("_error"):
                log(f"  ✗ market fallback: {mr['_error'][:80]}")
                return
            await asyncio.sleep(0.5)
            await record_exit(state, base, mr, "market_fallback", log)


async def record_exit(state: BotState, base: str, exit_order: dict | None, reason: str, log):
    s = state.sym[base]
    flip_sym = SYMBOLS_CFG[base][3]

    # Determine exit price from order or query position
    if exit_order:
        avg_close = float(exit_order.get("avgPrice", s.flip_tob.bid))
    else:
        # Limit filled — use the order price (we placed it)
        # We don't have the actual fill price easily; estimate from position queries
        avg_close = s.flip_tob.bid if s.entry_side == "SELL" else s.flip_tob.ask
        # If we had limit order at entry mid+, fill should be at that price approximately

    sign = 1 if s.entry_side == "BUY" else -1
    pnl_bp = (avg_close - s.entry_price) / s.entry_price * 1e4 * sign
    pnl_usd = pnl_bp * state.amount_usd / 1e4
    state.realized_pnl += pnl_usd
    if pnl_bp > 0: state.n_wins += 1

    hold = time.time() - s.in_position_since
    log(f"  pnl={pnl_bp:+.2f}bp (${pnl_usd:+.4f}) hold={hold:.0f}s reason={reason}")
    log(f"  TOTAL: signals={state.n_signals} filled={state.n_filled} cancelled={state.n_cancelled} wins={state.n_wins} pnl=${state.realized_pnl:+.4f}")

    with open(state.log_path, "a") as f:
        f.write(json.dumps({
            "ts": datetime.now(timezone.utc).isoformat(),
            "base": base, "side": s.entry_side, "reason": reason,
            "entry": s.entry_price, "exit": avg_close,
            "amount_usd": state.amount_usd, "hold_s": hold,
            "pnl_bp": pnl_bp, "pnl_usd": pnl_usd,
        }) + "\n")

    s.state = "IDLE"
    s.entry_order_id = None
    s.exit_order_id = None
    s.cooldown_until = time.time() + 3

    if state.realized_pnl < state.kill_pnl_usd:
        log(f"⛔ KILL: ${state.realized_pnl:.2f}")
        raise SystemExit(0)


# ---------------------------------------------------------------------------
# Feeds (same as v5)
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
    sys.path.insert(0, "/home/gate1/projects/quant/hft_monorepo/flipster_web")
    from python.cookies import _fetch_cookies
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
    p.add_argument("--symbols", default="ETH,XRP,DOGE,SOL,BTC,LINK,AVAX,LTC,INJ,SUI,APT")
    p.add_argument("--amount-usd", type=float, default=50)
    p.add_argument("--threshold", type=float, default=4.0)
    p.add_argument("--exit-threshold", type=float, default=1.0)
    p.add_argument("--sustain-s", type=float, default=1.0)
    p.add_argument("--fill-wait-s", type=float, default=10.0,
                   help="Max time to wait for limit fill before cancel/market")
    p.add_argument("--hold-max-s", type=float, default=60.0)
    p.add_argument("--kill-pnl", type=float, default=-5.0)
    p.add_argument("--duration-min", type=float, default=60)
    args = p.parse_args()

    symbols = [s.strip() for s in args.symbols.split(",") if s.strip() in SYMBOLS_CFG]
    state = BotState(
        symbols=symbols, amount_usd=args.amount_usd, threshold_bp=args.threshold,
        exit_threshold_bp=args.exit_threshold, sustain_secs=args.sustain_s,
        fill_wait_s=args.fill_wait_s, hold_max_s=args.hold_max_s,
        kill_pnl_usd=args.kill_pnl,
    )
    state.log_path.parent.mkdir(parents=True, exist_ok=True)
    for b in symbols: state.sym[b] = SymbolState(base=b)

    def log(msg):
        ts = datetime.now().strftime("%H:%M:%S")
        print(f"{ts} {msg}", flush=True)

    log(f"=== SLOW STAT-ARB MAKER START ===")
    log(f"  symbols: {symbols}")
    log(f"  amount=${args.amount_usd} threshold={args.threshold}bp exit={args.exit_threshold}bp")
    log(f"  sustain={args.sustain_s}s fill_wait={args.fill_wait_s}s hold_max={args.hold_max_s}s")
    log(f"  kill=${args.kill_pnl} duration={args.duration_min}min")
    log(f"  account: HMAC v1 ($13k account)")

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

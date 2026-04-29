#!/usr/bin/env python3
"""Slow Stat-Arb v9 — Multi-timeframe + Symbol baseline + Hysteresis exit.

Improvements over v8:
  1. PER-SYMBOL ROLLING BASELINE: instead of (Flipster - consensus), use
     (Flipster - consensus) - rolling_5min_baseline.
     Auto-handles symbols with persistent offset (ARB, TON-style).

  2. MULTI-TIMEFRAME SUSTAINED: signal must agree across NOW + 5s ago + 10s ago.
     Filters momentary spikes that are just noise.

  3. HYSTERESIS EXIT: don't exit on tiny profit. Wait for:
     - Overshoot to OPPOSITE side (entry +6bp → close at <= -2bp)
     - OR big profit (>+10bp unrealized)
     - OR stop loss (-10bp)
     - OR timeout (5 min)
     Avoids the "small wins eaten by fees" trap.

  4. LATENCY GUARD: reject any feed > 500ms stale.

  5. STARTUP WARMUP 60s: collect baseline data before any trades.
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

sys.path.insert(0, str(Path(__file__).resolve().parent))
from positions_ws import WSPositionTracker

FLIP_V2_WS = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"
BINANCE_WS = "wss://fstream.binance.com/ws"
GATE_WS = "wss://fx-ws.gateio.ws/v4/ws/usdt"
BITGET_WS = "wss://ws.bitget.com/v2/ws/public"

# Auto-loaded from symbols_v9.json (374 4-exchange overlap → filtered to 144 by tick precision)
def _load_symbols_cfg():
    cfg_path = Path(__file__).parent / "symbols_v9.json"
    if not cfg_path.exists():
        # Fallback to majors
        return {
            "BTC":  ("btcusdt",  "BTC_USDT",  "BTCUSDT",  "BTCUSDT.PERP",  0.1),
            "ETH":  ("ethusdt",  "ETH_USDT",  "ETHUSDT",  "ETHUSDT.PERP",  0.01),
        }
    data = json.loads(cfg_path.read_text())
    cfg = {}
    for d in data:
        base = d["base"]
        cfg[base] = (
            f"{base.lower()}usdt",
            f"{base}_USDT",
            f"{base}USDT",
            f"{base}USDT.PERP",
            d["tick"],
        )
    return cfg

SYMBOLS_CFG = _load_symbols_cfg()


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

    # Rolling history of (Flipster - consensus) raw values
    # Used for: (a) baseline calc (5min avg)  (b) multi-timeframe check
    raw_dev_history: deque = field(default_factory=lambda: deque(maxlen=600))  # 10min @ 1Hz
    # Rolling consensus price for momentum check
    consensus_history: deque = field(default_factory=lambda: deque(maxlen=120))  # 2min @ 1Hz

    # Position
    state: str = "IDLE"  # IDLE | ENTRY_PENDING | IN_POSITION | EXIT_PENDING
    entry_slot: int | None = None
    entry_order_id: str | None = None
    entry_order_time: float = 0.0
    entry_price: float = 0.0
    entry_side: Side | None = None
    entry_amount: float = 0.0
    entry_dev: float = 0.0  # in BASELINE-ADJUSTED terms
    in_position_since: float = 0.0
    cooldown_until: float = 0.0
    # Per-symbol session loss tracking
    consecutive_losses: int = 0
    total_pnl_bp: float = 0.0
    blacklist_until: float = 0.0

    def consensus(self) -> float | None:
        now = time.time()
        # LATENCY GUARD: reject if any feed > 500ms stale
        for m in [self.binance, self.gate, self.bitget]:
            if m.val == 0 or now - m.ts > 0.5:
                return None
        return (self.binance.val + self.gate.val + self.bitget.val) / 3

    def consensus_quality_bp(self) -> float | None:
        """Spread between max and min of the 3 reference exchanges in bp.
        Returns None if any feed missing/stale.
        Higher = exchanges disagree more = consensus less reliable.
        """
        now = time.time()
        prices = []
        for m in [self.binance, self.gate, self.bitget]:
            if m.val == 0 or now - m.ts > 0.5: return None
            prices.append(m.val)
        avg = sum(prices) / 3
        if avg == 0: return None
        return (max(prices) - min(prices)) / avg * 1e4

    def raw_dev_bp(self) -> float | None:
        """Raw deviation: (Flipster - consensus) / consensus in bp."""
        flip_mid = (self.flip_tob.bid + self.flip_tob.ask) / 2
        if flip_mid == 0 or time.time() - self.flip_tob.ts > 0.5: return None
        cons = self.consensus()
        if cons is None: return None
        return (flip_mid - cons) / cons * 1e4

    def baseline_bp(self) -> float:
        """Median of last 5 min of raw_dev. 0 if not enough data."""
        if len(self.raw_dev_history) < 60:  # need 1 min minimum
            return 0.0
        # Use median over last 5 min for robustness
        cutoff_ts = time.time() - 300
        recent = [v for t, v in self.raw_dev_history if t >= cutoff_ts]
        if len(recent) < 30: return 0.0
        recent_sorted = sorted(recent)
        return recent_sorted[len(recent_sorted) // 2]  # median

    def adjusted_dev_bp(self) -> float | None:
        """Baseline-adjusted deviation = raw - baseline.
        This is the TRUE deviation regardless of structural offset."""
        raw = self.raw_dev_bp()
        if raw is None: return None
        return raw - self.baseline_bp()

    def adjusted_dev_at(self, secs_ago: float) -> float | None:
        """Get baseline-adjusted dev from N seconds ago."""
        target_ts = time.time() - secs_ago
        # Find closest sample
        for t, v in reversed(self.raw_dev_history):
            if t <= target_ts:
                return v - self.baseline_bp()
        return None

    def consensus_change_bp(self, secs_ago: float) -> float | None:
        """% change in consensus over the last `secs_ago` seconds, in bp.
        Positive = consensus rose, Negative = consensus fell.
        """
        cur = self.consensus()
        if cur is None or len(self.consensus_history) == 0: return None
        target_ts = time.time() - secs_ago
        past = None
        for t, v in reversed(self.consensus_history):
            if t <= target_ts:
                past = v; break
        if past is None or past == 0: return None
        return (cur - past) / past * 1e4


@dataclass
class BotState:
    symbols: list[str]
    amount_usd: float
    leverage: int
    threshold_bp: float
    max_dev_bp: float
    max_spread_bp: float
    min_net_edge_bp: float
    momentum_block_bp: float
    max_consensus_disagree_bp: float
    profit_take_bp: float
    stop_loss_bp: float
    overshoot_bp: float
    hold_max_s: float
    fill_wait_s: float
    warmup_s: float
    kill_pnl_usd: float

    sym: dict = field(default_factory=dict)
    orphan_first_seen: dict = field(default_factory=dict)  # sym_slot -> first-seen ts
    realized_pnl: float = 0.0
    n_signals: int = 0
    n_filled: int = 0
    n_cancelled: int = 0
    n_wins: int = 0
    fc: FlipsterClient = None
    tracker: WSPositionTracker = None
    log_path: Path = field(default_factory=lambda: Path("logs/slow_statarb_v9.jsonl"))
    started_at: float = field(default_factory=time.time)


# ---------------------------------------------------------------------------
# Strategy
# ---------------------------------------------------------------------------

async def step_symbol(state: BotState, base: str, log):
    s = state.sym[base]
    flip_sym = SYMBOLS_CFG[base][3]

    # Always update raw dev history (for baseline)
    raw = s.raw_dev_bp()
    if raw is not None:
        s.raw_dev_history.append((time.time(), raw))
    # Update consensus history
    cons = s.consensus()
    if cons is not None:
        s.consensus_history.append((time.time(), cons))

    # Skip strategy until warmup complete
    if time.time() - state.started_at < state.warmup_s:
        return

    if s.state == "IDLE":
        if time.time() < s.cooldown_until: return
        if time.time() < s.blacklist_until: return
        adj = s.adjusted_dev_bp()
        if adj is None: return
        if abs(adj) < state.threshold_bp: return
        if abs(adj) > state.max_dev_bp: return  # bad data / structural offset, skip

        # Skip if we already have a position on this symbol (orphan or our own)
        if state.tracker and state.tracker.has_position(flip_sym):
            return
        # Skip if not enough margin (need ~ amount/leverage with safety buffer)
        need_margin = state.amount_usd / state.leverage * 1.15
        if state.tracker and state.tracker.connected and state.tracker.margin_avail < need_margin:
            return

        # CONSENSUS QUALITY: 3 reference exchanges must agree within X bp,
        # otherwise the "consensus" is unreliable and adj_dev is meaningless.
        cq = s.consensus_quality_bp()
        if cq is not None and cq > state.max_consensus_disagree_bp:
            return

        # SPREAD FILTER: skip if Flipster spread > X bp (crossing too costly)
        flip_mid = (s.flip_tob.bid + s.flip_tob.ask) / 2
        if flip_mid > 0:
            spread_bp = (s.flip_tob.ask - s.flip_tob.bid) / flip_mid * 1e4
            if spread_bp > state.max_spread_bp:
                return
        # EDGE-VS-COST CHECK: net edge after spread+fee must beat min_net_edge_bp
        # cost ≈ spread + 2 × taker_fee_bp (worst-case) = spread + 0.85
        est_cost_bp = spread_bp + 0.85 if flip_mid > 0 else 0
        net_edge_bp = abs(adj) - est_cost_bp
        if net_edge_bp < state.min_net_edge_bp:
            return

        # MULTI-TIMEFRAME SUSTAINED CHECK
        adj_5s = s.adjusted_dev_at(5)
        adj_10s = s.adjusted_dev_at(10)
        if adj_5s is None or adj_10s is None: return

        sign = 1 if adj > 0 else -1
        # Both 5s and 10s ago must be SAME SIGN (deviation persisting, not new spike)
        if (adj_5s > 0) != (adj > 0) or (adj_10s > 0) != (adj > 0):
            return
        # AND magnitude growing (current >= 5s ago)
        if abs(adj) < abs(adj_5s) * 0.8:
            return

        # MOMENTUM GATE: don't fight a strong directional move on consensus.
        # If we're betting Short (Flipster too high → expect down), consensus must NOT be rising fast.
        # If we're betting Long (Flipster too low → expect up), consensus must NOT be falling fast.
        cons_ch_5s = s.consensus_change_bp(5.0)
        if cons_ch_5s is not None:
            # adj > 0 → Short bet → reject if cons_ch > +threshold
            # adj < 0 → Long bet  → reject if cons_ch < -threshold
            mom_thresh = state.momentum_block_bp
            if adj > 0 and cons_ch_5s > mom_thresh: return
            if adj < 0 and cons_ch_5s < -mom_thresh: return

        # All checks passed
        side = Side.SHORT if adj > 0 else Side.LONG
        price = s.flip_tob.ask if side == Side.SHORT else s.flip_tob.bid

        state.n_signals += 1
        log(f"[{base} ENTRY #{state.n_signals}] adj_dev={adj:+.2f}bp (5s={adj_5s:+.2f}, 10s={adj_10s:+.2f}, baseline={s.baseline_bp():+.2f}) side={side.value} px={price}")

        try:
            params = OrderParams(side=side, amount_usd=state.amount_usd,
                                 order_type=OrderType.LIMIT, price=price,
                                 leverage=state.leverage)
            r = await asyncio.to_thread(state.fc.place_order, flip_sym, params, price)
        except Exception as e:
            log(f"  ✗ entry: {str(e)[:120]}")
            s.cooldown_until = time.time() + 5
            return

        pos = r.get("position", {})
        order = r.get("order", {})
        slot = pos.get("slot")
        order_id = order.get("orderId")
        avg_price = pos.get("avgPrice") or order.get("avgPrice") or 0
        size = pos.get("size") or "0"

        if size and float(size) != 0:
            # Filled (instantly or slow)
            state.n_filled += 1
            s.state = "IN_POSITION"
            s.entry_slot = slot
            s.entry_price = float(avg_price)
            s.entry_side = side
            s.entry_amount = state.amount_usd
            s.entry_dev = adj
            s.in_position_since = time.time()
            log(f"  ✓ FILLED slot={slot} avg=${avg_price}")
        else:
            s.state = "ENTRY_PENDING"
            s.entry_slot = slot
            s.entry_order_id = order_id
            s.entry_order_time = time.time()
            s.entry_price = price
            s.entry_side = side
            s.entry_amount = state.amount_usd
            s.entry_dev = adj
            log(f"  ⏳ pending slot={slot}")
        return

    if s.state == "ENTRY_PENDING":
        elapsed = time.time() - s.entry_order_time
        # Check WS tracker for fill (covers slow fills up to fill_wait_s)
        if state.tracker and state.tracker.has_position(flip_sym):
            log(f"[{base}] entry FILLED (ws-detected, {elapsed:.1f}s)")
            state.n_filled += 1
            s.state = "IN_POSITION"
            s.in_position_since = time.time()
            return

        if elapsed > state.fill_wait_s:
            # Real cancel via DELETE /api/v2/trade/orders/{symbol}/{orderId}
            cancelled_ok = False
            if s.entry_order_id:
                try:
                    await asyncio.to_thread(state.fc.cancel_order, flip_sym, s.entry_order_id)
                    cancelled_ok = True
                except Exception as e:
                    msg = str(e)[:120]
                    # If already filled or not found, WS will tell us
                    if "404" in msg or "NotFound" in msg:
                        cancelled_ok = True
                    else:
                        log(f"[{base}] cancel err: {msg}")
            log(f"[{base}] entry timeout {elapsed:.0f}s "
                f"({'cancelled' if cancelled_ok else 'cancel-failed'})")
            state.n_cancelled += 1
            s.state = "IDLE"
            s.entry_slot = None
            s.entry_order_id = None
            s.cooldown_until = time.time() + 5
        return

    if s.state == "IN_POSITION":
        elapsed = time.time() - s.in_position_since

        # Compute current adjusted dev
        adj_now = s.adjusted_dev_bp()

        # Compute unrealized PnL — use REALIZABLE close price, not mid.
        # For Long position we'd close by selling at bid; for Short by buying at ask.
        # This makes exit decisions reflect actual achievable PnL after spread crossing.
        sign_pos = 1 if s.entry_side == Side.LONG else -1
        if s.entry_side == Side.LONG:
            close_price = s.flip_tob.bid
        else:
            close_price = s.flip_tob.ask
        if close_price <= 0:
            return
        unrealized_bp = (close_price - s.entry_price) / s.entry_price * 1e4 * sign_pos

        # Mid-based for overshoot logic (sign reversal, not pnl)
        flip_mid = (s.flip_tob.bid + s.flip_tob.ask) / 2

        # HYSTERESIS EXIT — only exit on real conditions, not small profit
        should_exit = False
        reason = ""

        # 1. Profit take (large profit)
        if unrealized_bp > state.profit_take_bp:
            should_exit = True; reason = f"profit_{unrealized_bp:.1f}bp"
        # 2. Stop loss
        elif unrealized_bp < -state.stop_loss_bp:
            should_exit = True; reason = f"stop_{unrealized_bp:.1f}bp"
        # 3. Overshoot exit (entry direction reversed past 0)
        elif adj_now is not None:
            entry_sign = 1 if s.entry_dev > 0 else -1
            cur_sign = 1 if adj_now > 0 else -1
            # Entry sign opposite of current sign AND magnitude past overshoot threshold
            if entry_sign != cur_sign and abs(adj_now) > state.overshoot_bp:
                # Overshoot — but only exit if also profitable
                if unrealized_bp > 0:
                    should_exit = True; reason = f"overshoot_{adj_now:+.1f}bp_pnl_{unrealized_bp:+.1f}bp"
        # 4. Hard timeout
        if not should_exit and elapsed > state.hold_max_s:
            should_exit = True; reason = f"timeout_{unrealized_bp:+.1f}bp"

        if not should_exit: return

        # Exit
        exit_side = Side.LONG if s.entry_side == Side.SHORT else Side.SHORT
        price = s.flip_tob.bid if exit_side == Side.SHORT else s.flip_tob.ask

        log(f"[{base} EXIT-{reason}] adj_now={(adj_now if adj_now is not None else 0):+.2f}bp side={exit_side.value} px={price}")

        try:
            r = await asyncio.to_thread(state.fc.close_position, flip_sym, s.entry_slot, price)
            order = r.get("order", {})
            avg_close = float(order.get("avgPrice", price))
            await record_exit(state, base, avg_close, reason, log)
        except Exception as e:
            log(f"  ✗ exit limit: {str(e)[:120]}")
            # Force market
            try:
                if s.entry_side == Side.LONG:
                    extreme = s.flip_tob.bid * 0.995
                else:
                    extreme = s.flip_tob.ask * 1.005
                r = await asyncio.to_thread(state.fc.close_position, flip_sym, s.entry_slot, extreme)
                avg_close = float(r.get("order", {}).get("avgPrice", extreme))
                await record_exit(state, base, avg_close, f"{reason}-force", log)
            except Exception as e2:
                log(f"  ✗ force close also failed: {str(e2)[:80]}")


async def record_exit(state: BotState, base: str, avg_close: float, reason: str, log):
    s = state.sym[base]
    sign = 1 if s.entry_side == Side.LONG else -1
    pnl_bp = (avg_close - s.entry_price) / s.entry_price * 1e4 * sign
    pnl_usd = pnl_bp * s.entry_amount / 1e4
    state.realized_pnl += pnl_usd
    if pnl_bp > 0:
        state.n_wins += 1
        s.consecutive_losses = 0
    else:
        s.consecutive_losses += 1
    s.total_pnl_bp += pnl_bp
    # Tiered blacklist:
    #   single loss < -12bp   → 30 min ban (toxic symbol — slippage too large)
    #   2 consecutive losses  → 5 min ban (cool-off)
    #   3 consecutive losses  → 30 min ban
    #   sum < -20bp           → 60 min ban
    #   sum < -40bp           → permanent (rest of session)
    if s.total_pnl_bp < -40.0:
        s.blacklist_until = time.time() + 86400
        log(f"  ⛔ {base} BLACKLISTED PERMANENT (sum_bp={s.total_pnl_bp:+.1f})")
    elif s.total_pnl_bp < -20.0:
        s.blacklist_until = time.time() + 3600
        log(f"  ⛔ {base} BLACKLISTED 60min (sum_bp={s.total_pnl_bp:+.1f})")
    elif pnl_bp < -12.0:
        s.blacklist_until = time.time() + 1800
        log(f"  ⛔ {base} BLACKLISTED 30min (single loss {pnl_bp:.1f}bp — slippage)")
    elif s.consecutive_losses >= 3:
        s.blacklist_until = time.time() + 1800
        log(f"  ⛔ {base} BLACKLISTED 30min (cons_loss={s.consecutive_losses})")
    elif s.consecutive_losses >= 2:
        s.blacklist_until = time.time() + 300
        log(f"  ⏸ {base} cool-off 5min (cons_loss={s.consecutive_losses})")

    hold = time.time() - s.in_position_since
    log(f"  pnl={pnl_bp:+.2f}bp (${pnl_usd:+.4f}) hold={hold:.0f}s")
    log(f"  TOTAL: signals={state.n_signals} filled={state.n_filled} cancelled={state.n_cancelled} wins={state.n_wins} pnl=${state.realized_pnl:+.4f}")

    with open(state.log_path, "a") as f:
        f.write(json.dumps({
            "ts": datetime.now(timezone.utc).isoformat(),
            "base": base, "side": s.entry_side.value, "reason": reason,
            "entry": s.entry_price, "exit": avg_close, "entry_dev_bp": s.entry_dev,
            "amount_usd": s.entry_amount, "hold_s": hold,
            "pnl_bp": pnl_bp, "pnl_usd": pnl_usd,
        }) + "\n")

    s.state = "IDLE"
    s.entry_slot = None
    s.entry_dev = 0.0
    # Longer cooldown after a loss (avoid revenge re-entry)
    s.cooldown_until = time.time() + (60 if pnl_bp < 0 else 10)

    if state.realized_pnl < state.kill_pnl_usd:
        log(f"⛔ KILL: ${state.realized_pnl:.2f}")
        raise SystemExit(0)


# ---------------------------------------------------------------------------
# Feeds (same as v8)
# ---------------------------------------------------------------------------

async def binance_feed_chunk(state: BotState, log, chunk: list[str]):
    """One WS for a chunk of symbols (Binance has implicit limits per stream)."""
    streams = "/".join(f"{SYMBOLS_CFG[b][0]}@bookTicker" for b in chunk)
    url = f"{BINANCE_WS}/{streams}"
    sym_to_base = {SYMBOLS_CFG[b][0]: b for b in chunk}
    while True:
        try:
            async with websockets.connect(url, ping_interval=20) as ws:
                log(f"[binance] chunk connected ({len(chunk)} syms)")
                async for msg in ws:
                    try: d = json.loads(msg)
                    except: continue
                    sym = d.get("s", "").lower()
                    base = sym_to_base.get(sym)
                    if not base: continue
                    bid = float(d.get("b", 0)); ask = float(d.get("a", 0))
                    if bid > 0 and ask > 0:
                        state.sym[base].binance.val = (bid + ask) / 2
                        state.sym[base].binance.ts = time.time()
        except Exception:
            await asyncio.sleep(3)


async def binance_feed(state: BotState, log):
    """Split symbols into chunks of 50 for stability."""
    syms = state.symbols
    chunks = [syms[i:i+50] for i in range(0, len(syms), 50)]
    log(f"[binance] starting {len(chunks)} WS chunks for {len(syms)} symbols")
    await asyncio.gather(*[binance_feed_chunk(state, log, c) for c in chunks])


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
        except Exception:
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
        except Exception:
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
        except Exception:
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
                    adj = state.sym[b].adjusted_dev_bp()
                    base_bp = state.sym[b].baseline_bp()
                    st = state.sym[b].state
                    if adj is not None:
                        stats.append(f"{b}={adj:+.1f}/{st}")
                    else:
                        stats.append(f"{b}=N/A/{st}")
                warm_left = max(0, state.warmup_s - (time.time() - state.started_at))
                warm = f"warming({warm_left:.0f}s)" if warm_left > 0 else "ARMED"
                log(f"  [{warm}] {' | '.join(stats)}  s/f/c={state.n_signals}/{state.n_filled}/{state.n_cancelled} pnl=${state.realized_pnl:+.4f}")
        except SystemExit: raise
        except Exception as e: log(f"[strategy] err: {e}")
        await asyncio.sleep(0.5)


async def stale_order_cleanup_loop(state, log, max_age_s: float = 60.0):
    """Cancel any pending order older than max_age_s. Uses private/orders WS stream.

    Order fields seen in WS: orderId, symbol, side, price, size, remainingQty,
    triggerTimestamp (ns since epoch), status, etc.
    """
    await asyncio.sleep(20)
    while True:
        try:
            await asyncio.sleep(15)
            if not state.tracker or not state.tracker.connected:
                continue
            now_ns = time.time_ns()
            for oid, fields in list(state.tracker.orders.items()):
                # Skip already-filled or cancelled orders
                remaining = fields.get("remainingQty") or fields.get("size") or "0"
                try:
                    if float(remaining) == 0: continue
                except ValueError: continue
                # age via timestamp
                ts_ns = fields.get("triggerTimestamp") or fields.get("timestamp") or fields.get("createdAt")
                if not ts_ns: continue
                try:
                    age_s = (now_ns - int(ts_ns)) / 1e9
                except (ValueError, TypeError): continue
                if age_s < max_age_s: continue
                sym = fields.get("symbol")
                if not sym: continue
                try:
                    await asyncio.to_thread(state.fc.cancel_order, sym, oid)
                    log(f"[stale-cancel] {sym} oid={oid[:10]}.. age={age_s:.0f}s")
                except Exception as e:
                    msg = str(e)[:80]
                    if "404" not in msg and "NotFound" not in msg:
                        log(f"[stale-cancel] {sym} fail: {msg}")
                await asyncio.sleep(0.2)
        except Exception as e:
            log(f"[stale-cleanup] err: {e}")
            await asyncio.sleep(5)


async def orphan_adopt_loop(state, log):
    """Adopt untracked positions into bot's symbol state so EXIT logic manages them.

    For each WS position with no matching IN_POSITION/PENDING in our state:
      - Read side/qty/entry from WS snapshot (`position` field sign + `avgEntryPrice`)
      - Set s.state = IN_POSITION with the right entry_side / entry_price / amount
      - Bot's hysteresis exit then handles profit_take/stop_loss/overshoot/timeout
    For positions on symbols NOT in our universe, fall back to closing
    (we can't manage what we don't subscribe to feeds for).
    """
    await asyncio.sleep(10)
    while True:
        try:
            await asyncio.sleep(15)
            if not state.tracker or not state.tracker.connected:
                continue
            for sym_slot, fields in list(state.tracker.positions.items()):
                if "/" not in sym_slot: continue
                flip_sym, slot_s = sym_slot.split("/", 1)
                try: slot = int(slot_s)
                except ValueError: continue
                base = flip_sym.replace("USDT.PERP", "")

                # Already tracked?
                ss = state.sym.get(base)
                if ss and ss.state in ("IN_POSITION", "ENTRY_PENDING", "EXIT_PENDING"):
                    continue

                # Symbol not in our universe → can't manage, close it
                if base not in state.sym:
                    mid = fields.get("midPrice") or fields.get("bidPrice")
                    if not mid: continue
                    try:
                        await asyncio.to_thread(state.fc.close_position, flip_sym, slot, float(mid))
                        log(f"[orphan-close] {sym_slot} (not in universe) closed at {mid}")
                    except Exception as e:
                        log(f"[orphan-close] {sym_slot} FAIL: {str(e)[:120]}")
                    await asyncio.sleep(0.3)
                    continue

                # In universe but untracked — try to adopt
                ssr = state.tracker.position_side_qty_entry(flip_sym, slot)
                if not ssr:
                    # No snapshot from WS — track first-seen time. Close once we've
                    # waited long enough or PnL crosses easy thresholds.
                    if sym_slot not in state.orphan_first_seen:
                        state.orphan_first_seen[sym_slot] = time.time()
                    age_s = time.time() - state.orphan_first_seen[sym_slot]
                    try:
                        unr = float(fields.get("unrealizedPnl", "0"))
                    except (TypeError, ValueError):
                        unr = 0.0
                    # Close conditions (any one):
                    #  - decisively profitable (>$0.03 ≈ +12bp)
                    #  - bad loss (<-$0.05 ≈ -20bp)
                    #  - aged > 90s without snapshot (give up adoption)
                    should_close = (unr > 0.03) or (unr < -0.05) or (age_s > 90)
                    if should_close:
                        mid = fields.get("midPrice") or fields.get("bidPrice")
                        if not mid: continue
                        try:
                            await asyncio.to_thread(state.fc.close_position, flip_sym, slot, float(mid))
                            log(f"[orphan-pnl-close] {sym_slot} unr={unr:+.4f} age={age_s:.0f}s closed at {mid}")
                            state.orphan_first_seen.pop(sym_slot, None)
                        except Exception as e:
                            log(f"[orphan-pnl-close] {sym_slot} FAIL: {str(e)[:120]}")
                        await asyncio.sleep(0.3)
                    continue
                side_str, qty, entry = ssr
                side = Side.LONG if side_str == "Long" else Side.SHORT
                amount_usd = qty * entry
                ss.state = "IN_POSITION"
                ss.entry_slot = slot
                ss.entry_price = entry
                ss.entry_side = side
                ss.entry_amount = amount_usd
                ss.entry_dev = 0.0  # unknown for adopted positions
                ss.in_position_since = time.time()
                log(f"[adopt] {sym_slot} side={side_str} qty={qty} entry={entry} (~${amount_usd:.2f})")
        except Exception as e:
            log(f"[adopt-loop] err: {e}")
            await asyncio.sleep(5)


async def _log_orphans_after_warmup(state, log):
    await asyncio.sleep(8)
    if not state.tracker: return
    open_pos = state.tracker.positions
    if open_pos:
        log(f"[orphans] {len(open_pos)} pre-existing positions (margin_avail=${state.tracker.margin_avail:.2f}):")
        for k, v in open_pos.items():
            unr = v.get('unrealizedPnl', '?')
            marg = v.get('initMarginReserved', '?')
            log(f"  {k}: marg={marg} unrealized={unr}")
        log(f"[orphans] bot will SKIP these symbols for new entries until they close")


async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--symbols", default="ALL",
                   help="Comma-separated bases or 'ALL' for all from symbols_v9.json")
    p.add_argument("--amount-usd", type=float, default=50)
    p.add_argument("--leverage", type=int, default=10)
    p.add_argument("--threshold", type=float, default=6.0,
                   help="Entry: |adjusted_dev| > this AND sustained at 5s/10s")
    p.add_argument("--max-dev-bp", type=float, default=30.0,
                   help="Reject signals with |adj_dev| above this (data quality cap)")
    p.add_argument("--max-spread-bp", type=float, default=4.0,
                   help="Reject if Flipster ask-bid spread exceeds this (crossing too costly)")
    p.add_argument("--min-net-edge-bp", type=float, default=4.0,
                   help="Reject unless |adj_dev| - (spread + 0.85bp fee) >= this (net edge after costs)")
    p.add_argument("--momentum-block-bp", type=float, default=4.0,
                   help="Reject if 5s consensus moved >this bp against our bet direction")
    p.add_argument("--max-consensus-disagree-bp", type=float, default=8.0,
                   help="Reject if (max - min) of 3 reference exchanges > this bp (consensus unreliable)")
    p.add_argument("--min-vol-usd", type=float, default=50_000_000,
                   help="Skip symbols with Binance 24h vol below this")
    p.add_argument("--profit-take", type=float, default=8.0,
                   help="Exit if unrealized > this bp (positive)")
    p.add_argument("--stop-loss", type=float, default=10.0,
                   help="Exit if unrealized < -this bp")
    p.add_argument("--overshoot", type=float, default=2.0,
                   help="Exit if dev crossed to opposite side beyond this AND profitable")
    p.add_argument("--hold-max-s", type=float, default=300.0,
                   help="Hard timeout in seconds")
    p.add_argument("--fill-wait-s", type=float, default=8.0)
    p.add_argument("--warmup-s", type=float, default=60.0,
                   help="Skip strategy for N seconds at start (collect baseline data)")
    p.add_argument("--kill-pnl", type=float, default=-5.0)
    p.add_argument("--duration-min", type=float, default=120)
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

    # Load full v9 catalog with vol info for filter decision
    _v9_path = Path(__file__).parent / "symbols_v9.json"
    _vol_map = {}
    if _v9_path.exists():
        for d in json.loads(_v9_path.read_text()):
            _vol_map[d["base"]] = d.get("binance_vol_usd", 0)
    if args.symbols.strip().upper() == "ALL":
        symbols = [b for b in SYMBOLS_CFG if _vol_map.get(b, 0) >= args.min_vol_usd]
    else:
        symbols = [s.strip() for s in args.symbols.split(",") if s.strip() in SYMBOLS_CFG]
    tracker = WSPositionTracker(cookies=cookies)
    state = BotState(
        symbols=symbols, amount_usd=args.amount_usd, leverage=args.leverage,
        threshold_bp=args.threshold, max_dev_bp=args.max_dev_bp,
        max_spread_bp=args.max_spread_bp, min_net_edge_bp=args.min_net_edge_bp,
        momentum_block_bp=args.momentum_block_bp,
        max_consensus_disagree_bp=args.max_consensus_disagree_bp,
        profit_take_bp=args.profit_take,
        stop_loss_bp=args.stop_loss, overshoot_bp=args.overshoot,
        hold_max_s=args.hold_max_s, fill_wait_s=args.fill_wait_s,
        warmup_s=args.warmup_s, kill_pnl_usd=args.kill_pnl, fc=fc, tracker=tracker,
    )
    state.log_path.parent.mkdir(parents=True, exist_ok=True)
    for b in symbols: state.sym[b] = SymbolState(base=b)

    def log(msg):
        ts = datetime.now().strftime("%H:%M:%S")
        print(f"{ts} {msg}", flush=True)

    log(f"=== SLOW STAT-ARB v9 (multi-tf + baseline + hysteresis) ===")
    log(f"  symbols ({len(symbols)}, min_vol=${args.min_vol_usd:,.0f}): {symbols}")
    log(f"  amount=${args.amount_usd} lev={args.leverage}x")
    log(f"  ENTRY: {args.threshold} < |adj_dev| < {args.max_dev_bp}bp + sustained at 5s/10s + growing")
    log(f"         spread <= {args.max_spread_bp}bp + net_edge >= {args.min_net_edge_bp}bp")
    log(f"  EXIT:  profit > {args.profit_take}bp OR stop < -{args.stop_loss}bp OR overshoot < -{args.overshoot}bp (+profitable) OR timeout {args.hold_max_s}s")
    log(f"  warmup={args.warmup_s}s")
    log(f"  account: BROWSER (cookies + {len(proxies)} proxies)")

    # Wait briefly for tracker to capture orphan positions then log them
    asyncio.create_task(_log_orphans_after_warmup(state, log))

    tasks = [
        asyncio.create_task(binance_feed(state, log)),
        asyncio.create_task(gate_feed(state, log)),
        asyncio.create_task(bitget_feed(state, log)),
        asyncio.create_task(flipster_feed(state, log)),
        asyncio.create_task(tracker.run(log)),
        asyncio.create_task(orphan_adopt_loop(state, log)),
        asyncio.create_task(stale_order_cleanup_loop(state, log)),
        asyncio.create_task(strategy_loop(state, log, args.duration_min * 60)),
    ]
    try:
        await tasks[7]
    finally:
        # On shutdown: cancel all bot's pending orders + close all positions
        log("\n=== shutdown cleanup ===")
        if tracker.orders:
            for oid, f in list(tracker.orders.items()):
                sym = f.get("symbol")
                rem = f.get("remainingQty") or f.get("size") or "0"
                try:
                    if float(rem) > 0 and sym:
                        try:
                            fc.cancel_order(sym, oid)
                            log(f"  cancelled order {sym}/{oid[:10]}..")
                        except Exception as e: log(f"  cancel fail {sym}: {str(e)[:80]}")
                except ValueError: pass
        for t in tasks[:7]: t.cancel()
        log(f"\n=== SHUTDOWN ===")
        log(f"  signals={state.n_signals} filled={state.n_filled} cancelled={state.n_cancelled} wins={state.n_wins}")
        if state.n_filled:
            log(f"  win rate: {state.n_wins/state.n_filled*100:.1f}%")
        log(f"  realized PnL: ${state.realized_pnl:+.4f}")


if __name__ == "__main__":
    asyncio.run(main())

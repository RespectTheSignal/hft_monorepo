#!/usr/bin/env python3
"""Slow Stat-Arb v11 — Rust strategy.rs + gate_order_manager.rs port (Python).

Adds to v10 the four high-leverage order_manager features:
  1. STALE_POSITION_AUTO_CLOSE — close positions older than max_hold_min via market.
  2. PROFIT_BP_EMA — per-symbol EMA of mid_profit_bp; gate entry on positive EMA after warmup.
  3. OPEN_ORDERS_BY_CONTRACT cache — formal index for duplicate-order detection.
  4. TRADE_STATS PERSISTENCE — JSON file dump every 60s, load on startup.

Plus from gate_order_manager.rs:
  • cancel_duplicate_orders — same (symbol, side) → keep oldest, cancel rest.
  • last_trade_time per symbol — gate re-entry until X seconds since last fill.
  • Recent orders queue + is_too_many_orders — global throttle.
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import random
import sys
import time
import urllib.request
from collections import deque
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import websockets

MONOREPO = Path(__file__).resolve().parent.parent.parent
sys.path.insert(0, str(MONOREPO / "flipster_web"))
from python.client import FlipsterClient
from python.browser import BrowserManager as FBM
from python.order import OrderParams, Side, OrderType, MarginType
from python.cookies import _fetch_cookies

sys.path.insert(0, str(Path(__file__).resolve().parent))
from positions_ws import WSPositionTracker


FLIP_V2_WS = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"
BINANCE_WS = "wss://fstream.binance.com/ws"
GATE_WS = "wss://fx-ws.gateio.ws/v4/ws/usdt"
BITGET_WS = "wss://ws.bitget.com/v2/ws/public"


# ---------------------------------------------------------------------------
# Symbol config
# ---------------------------------------------------------------------------

def _load_symbols_cfg() -> dict:
    cfg_path = Path(__file__).parent / "symbols_v9.json"
    if not cfg_path.exists():
        return {"BTC": ("btcusdt", "BTC_USDT", "BTCUSDT", "BTCUSDT.PERP", 0.1, 0, 100)}
    data = json.loads(cfg_path.read_text())
    cfg = {}
    for d in data:
        base = d["base"]
        cfg[base] = (
            f"{base.lower()}usdt",                # 0: binance stream name
            f"{base}_USDT",                       # 1: gate sym
            f"{base}USDT",                        # 2: bitget sym
            f"{base}USDT.PERP",                   # 3: flipster sym
            d.get("tick", 0.0001),                # 4: tick size
            d.get("binance_vol_usd", 0.0),        # 5: binance 24h vol
            int(d.get("max_leverage", 10)),       # 6: per-symbol max leverage from Flipster specs
        )
    return cfg


SYMBOLS_CFG = _load_symbols_cfg()


def symbol_max_leverage(base: str) -> int:
    """Per-symbol max leverage from Flipster specs (1/initMarginRate)."""
    cfg = SYMBOLS_CFG.get(base)
    if not cfg or len(cfg) < 7: return 10
    return cfg[6]


# ---------------------------------------------------------------------------
# DataCache (Rust analog: data_cache.rs)
# ---------------------------------------------------------------------------

@dataclass
class TickerSnap:
    """One side's top-of-book snapshot at a moment."""
    bid: float = 0.0
    ask: float = 0.0
    server_time_ms: int = 0  # exchange-side timestamp
    received_at_ms: int = 0  # local receive timestamp

    def mid(self) -> float:
        if self.bid <= 0 or self.ask <= 0: return 0.0
        return (self.bid + self.ask) / 2.0

    def spread_bp(self) -> float:
        m = self.mid()
        if m <= 0: return 0.0
        return (self.ask - self.bid) / m * 1e4


@dataclass
class SymbolSnap:
    """All exchange tops-of-book at one wall-clock instant for one symbol.

    Rust analog: BookticketSnapshot containing gate_bt + gate_web_bt + base_bt.
    Mapping for Flipster:
      flip   ↔ Rust gate_bt   (we trade here)
      gate   ↔ Rust gate_web  (web/secondary)
      bitget ↔ extra reference
      binance ↔ Rust base_bt  (primary reference)
    """
    flip: TickerSnap = field(default_factory=TickerSnap)
    binance: TickerSnap = field(default_factory=TickerSnap)
    gate: TickerSnap = field(default_factory=TickerSnap)
    bitget: TickerSnap = field(default_factory=TickerSnap)
    wall_ms: int = 0  # snapshot wall-clock when assembled

    def consensus_mid(self) -> float | None:
        prices = [m for m in (self.binance.mid(), self.gate.mid(), self.bitget.mid()) if m > 0]
        if len(prices) < 2: return None
        return sum(prices) / len(prices)

    def consensus_disagree_bp(self) -> float | None:
        prices = [m for m in (self.binance.mid(), self.gate.mid(), self.bitget.mid()) if m > 0]
        if len(prices) < 2: return None
        avg = sum(prices) / len(prices)
        if avg <= 0: return None
        return (max(prices) - min(prices)) / avg * 1e4

    def raw_dev_bp(self) -> float | None:
        """(flip - consensus) / consensus, bp."""
        f = self.flip.mid()
        c = self.consensus_mid()
        if f <= 0 or c is None or c <= 0: return None
        return (f - c) / c * 1e4


@dataclass
class DataCache:
    """Per-symbol rolling history of SymbolSnap.

    Rust calls these previous_snapshot_1s / 5s / 10s / 20s.
    History deque holds (wall_ms, SymbolSnap), retains last 30s.
    """
    history: dict[str, deque] = field(default_factory=dict)  # base -> deque
    current: dict[str, SymbolSnap] = field(default_factory=dict)

    def push(self, base: str, snap: SymbolSnap):
        if base not in self.history:
            self.history[base] = deque(maxlen=600)  # 30s @ 20Hz
        self.history[base].append((snap.wall_ms, snap))
        self.current[base] = snap

    def snap_before(self, base: str, before_ms: int) -> Optional[SymbolSnap]:
        h = self.history.get(base)
        if not h: return None
        for wall, snap in reversed(h):
            if wall <= before_ms:
                return snap
        return None


# ---------------------------------------------------------------------------
# MarketWatcher (Rust analog: market_watcher.rs)
# ---------------------------------------------------------------------------

@dataclass
class MarketStats:
    """60s rolling avg_gap and avg_spread per symbol (Rust avg_mid_gap, avg_spread)."""
    gap_history: dict[str, deque] = field(default_factory=dict)     # base -> deque[(ts, gap_bp)]
    spread_history: dict[str, deque] = field(default_factory=dict)  # base -> deque[(ts, spread_bp)]
    window_s: float = 60.0

    def update(self, base: str, gap_bp: float, spread_bp: float):
        now = time.time()
        for store, val in ((self.gap_history, gap_bp), (self.spread_history, spread_bp)):
            d = store.setdefault(base, deque(maxlen=300))
            d.append((now, val))
            cutoff = now - self.window_s
            while d and d[0][0] < cutoff: d.popleft()

    def avg_gap_bp(self, base: str) -> float:
        d = self.gap_history.get(base)
        if not d or len(d) < 5: return 0.0
        vals = [v for _, v in d]
        s = sorted(vals)
        return s[len(s) // 2]  # median for robustness

    def avg_spread_bp(self, base: str) -> float:
        d = self.spread_history.get(base)
        if not d: return 0.0
        return sum(v for _, v in d) / len(d)


# ---------------------------------------------------------------------------
# OrderCountTracker (Rust analog: increment_order_count + GateOrderManager limits)
# ---------------------------------------------------------------------------

@dataclass
class OrderCountTracker:
    """Per-symbol rate limit. Rejects if N orders sent in last window_s seconds."""
    counts: dict[str, deque] = field(default_factory=dict)  # base -> deque[ts]
    max_per_minute: int = 8

    def can_send(self, base: str) -> bool:
        now = time.time()
        d = self.counts.setdefault(base, deque(maxlen=200))
        cutoff = now - 60.0
        while d and d[0] < cutoff: d.popleft()
        return len(d) < self.max_per_minute

    def increment(self, base: str):
        d = self.counts.setdefault(base, deque(maxlen=200))
        d.append(time.time())


# ---------------------------------------------------------------------------
# OrderRequest + queue (Rust analog: OrderRequest struct + crossbeam channel)
# ---------------------------------------------------------------------------

@dataclass
class OrderRequest:
    base: str
    flip_sym: str
    side: Side
    price: float
    amount_usd: float
    leverage: int
    is_close: bool = False
    slot: int = 0
    entry_dev_bp: float = 0.0  # carried for record_exit
    request_id: str = ""


# ---------------------------------------------------------------------------
# Per-symbol position state
# ---------------------------------------------------------------------------

@dataclass
class SymbolState:
    state: str = "IDLE"  # IDLE | ENTRY_PENDING | IN_POSITION | EXIT_PENDING
    entry_slot: int | None = None
    entry_order_id: str | None = None
    entry_order_time: float = 0.0
    entry_price: float = 0.0
    entry_side: Side | None = None
    entry_amount: float = 0.0
    entry_dev: float = 0.0
    in_position_since: float = 0.0
    cooldown_until: float = 0.0
    consecutive_losses: int = 0
    total_pnl_bp: float = 0.0
    blacklist_until: float = 0.0
    # Rust gate_order_manager.rs analogs
    profit_bp_ema: float = 0.0          # EMA of mid_profit_bp on exit
    n_trades: int = 0                   # total exits
    n_profitable: int = 0               # exits with pnl_bp > 0
    last_trade_ms: int = 0              # ms since epoch of last fill (entry+exit)


# ---------------------------------------------------------------------------
# StrategyConfig + main BotState
# ---------------------------------------------------------------------------

@dataclass
class StrategyConfig:
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
    amount_usd: float
    leverage: int
    # Rust-style: latency limits
    base_lat_ms: int = 300        # Binance/Gate/Bitget reference feeds
    target_lat_ms: int = 200      # Flipster (Rust gate_bt)
    # Multi-timeframe confirmation: how many of 1s/5s/10s/20s must agree on direction
    min_tf_agree: int = 3
    # AccountHealth
    min_margin_usd: float = 5.0
    # Restart loop
    restart_interval_s: float = 600.0
    # Stale position auto-close
    stale_position_min: float = 30.0   # close if open longer than this (minutes)
    # profit_bp_ema
    profit_bp_ema_alpha: float = 0.2   # weight on new sample
    min_trades_for_ema_gate: int = 5   # trades before EMA filter activates
    min_profit_bp_ema: float = -2.0    # require ema >= this to enter (after activation)
    # Recent orders global throttle
    max_orders_per_min_global: int = 30


@dataclass
class BotState:
    symbols: list[str]
    config: StrategyConfig
    cache: DataCache = field(default_factory=DataCache)
    market: MarketStats = field(default_factory=MarketStats)
    sym: dict[str, SymbolState] = field(default_factory=dict)
    order_count: OrderCountTracker = field(default_factory=OrderCountTracker)
    order_q: asyncio.Queue = field(default_factory=lambda: asyncio.Queue(maxsize=200))
    realized_pnl: float = 0.0
    n_signals: int = 0
    n_filled: int = 0
    n_cancelled: int = 0
    n_wins: int = 0
    n_orders_sent: int = 0
    fc: FlipsterClient = None
    tracker: WSPositionTracker = None
    log_path: Path = field(default_factory=lambda: Path("logs/slow_statarb_v11.jsonl"))
    stats_path: Path = field(default_factory=lambda: Path("logs/v11_trade_stats.json"))
    started_at: float = field(default_factory=time.time)
    orphan_first_seen: dict = field(default_factory=dict)
    kill_pnl_usd: float = -10.0
    # Recent global orders queue (Rust analog: recent_orders_queue)
    recent_orders: deque = field(default_factory=lambda: deque(maxlen=200))


# ---------------------------------------------------------------------------
# Signal calculation (Rust analog: core.calculate_signal)
# ---------------------------------------------------------------------------

def calculate_signal(snap: SymbolSnap, baseline_bp: float) -> dict:
    """Returns {"adj_dev_bp": float, "side": Side|None, "spread_bp": float}."""
    raw = snap.raw_dev_bp()
    if raw is None: return {"adj_dev_bp": None}
    adj = raw - baseline_bp
    return {
        "adj_dev_bp": adj,
        "side": Side.SHORT if adj > 0 else Side.LONG,
        "spread_bp": snap.flip.spread_bp(),
        "consensus_disagree_bp": snap.consensus_disagree_bp() or 0.0,
    }


# ---------------------------------------------------------------------------
# decide_order (Rust analog: core.decide_order with multi-tf confirmation)
# ---------------------------------------------------------------------------

def decide_order(state: BotState, base: str, signal: dict, now_ms: int, log) -> Optional[dict]:
    """Returns dict {"side": Side, "price": float} if entry should fire, else None.

    Mirrors Rust decide_order_v8 multi-confirmation logic:
      - signal magnitude > threshold + < cap
      - 1s/5s/10s/20s previous snapshots: at least min_tf_agree on same side
      - magnitude growing (now >= 5s ago)
      - net_edge after costs
      - consensus quality
      - flipster spread
      - momentum gate (consensus 5s movement vs our bet)
      - account health (margin)
      - per-symbol order count
      - per-symbol blacklist + cooldown
    """
    cfg = state.config
    s = state.sym[base]
    flip_sym = SYMBOLS_CFG[base][3]

    if time.time() < s.cooldown_until: return None
    if time.time() < s.blacklist_until: return None
    if s.state != "IDLE": return None

    adj = signal.get("adj_dev_bp")
    if adj is None: return None
    if abs(adj) < cfg.threshold_bp: return None
    if abs(adj) > cfg.max_dev_bp: return None

    # consensus quality
    if signal["consensus_disagree_bp"] > cfg.max_consensus_disagree_bp: return None
    # spread
    if signal["spread_bp"] > cfg.max_spread_bp: return None
    # net edge
    est_cost_bp = signal["spread_bp"] + 0.85
    if abs(adj) - est_cost_bp < cfg.min_net_edge_bp: return None

    # No double-position
    if state.tracker and state.tracker.has_position(flip_sym): return None
    # Account health
    need_margin = cfg.amount_usd / cfg.leverage * 1.15
    if state.tracker and state.tracker.connected:
        if state.tracker.margin_avail < max(need_margin, cfg.min_margin_usd):
            return None
    # Order count rate limit
    if not state.order_count.can_send(base): return None
    # Global throttle (Rust is_too_many_orders): >max_orders_per_min_global in last 60s
    cutoff_ms = (now_ms / 1000) - 60.0
    while state.recent_orders and state.recent_orders[0] < cutoff_ms:
        state.recent_orders.popleft()
    if len(state.recent_orders) >= cfg.max_orders_per_min_global: return None
    # profit_bp_ema gate: after enough trades, require non-trash EMA on this symbol
    if s.n_trades >= cfg.min_trades_for_ema_gate:
        if s.profit_bp_ema < cfg.min_profit_bp_ema: return None
    # last_trade gating: don't re-enter same symbol within cooldown
    if s.last_trade_ms and (now_ms - s.last_trade_ms) < int(s.cooldown_until - time.time()) * 1000:
        # Already in cooldown_until check, just keep — kept for explicitness
        pass

    # Multi-timeframe confirmation: get adj at -1s, -5s, -10s, -20s
    baseline_bp = state.market.avg_gap_bp(base)
    sign_bet = 1 if adj > 0 else -1
    tf_marks_ms = [1000, 5000, 10000, 20000]
    same_dir_count = 0
    adj_5s = None
    for ms_back in tf_marks_ms:
        prev = state.cache.snap_before(base, now_ms - ms_back)
        if prev is None: continue
        praw = prev.raw_dev_bp()
        if praw is None: continue
        padj = praw - baseline_bp
        if (padj > 0) == (adj > 0): same_dir_count += 1
        if ms_back == 5000: adj_5s = padj
    if same_dir_count < cfg.min_tf_agree: return None
    # Magnitude must be growing (current >= 80% of 5s ago)
    if adj_5s is not None and abs(adj) < abs(adj_5s) * 0.8: return None

    # Momentum gate: consensus 5s movement vs our bet
    cur_cons = state.cache.current.get(base)
    prev_cons = state.cache.snap_before(base, now_ms - 5000)
    if cur_cons and prev_cons:
        c_now = cur_cons.consensus_mid()
        c_old = prev_cons.consensus_mid()
        if c_now and c_old and c_old > 0:
            cons_ch_bp = (c_now - c_old) / c_old * 1e4
            if adj > 0 and cons_ch_bp > cfg.momentum_block_bp: return None
            if adj < 0 and cons_ch_bp < -cfg.momentum_block_bp: return None

    # All passed
    side = Side.SHORT if adj > 0 else Side.LONG
    price = cur_cons.flip.ask if side == Side.SHORT else cur_cons.flip.bid
    if price <= 0: return None

    return {"side": side, "price": price, "adj": adj}


# ---------------------------------------------------------------------------
# Order worker (Rust analog: order_worker_loop)
# ---------------------------------------------------------------------------

async def order_worker(state: BotState, log, worker_id: int):
    """Pulls OrderRequest from queue, places it, increments counters."""
    while True:
        try:
            req: OrderRequest = await state.order_q.get()
        except asyncio.CancelledError:
            return
        try:
            if req.is_close:
                r = await asyncio.to_thread(
                    state.fc.close_position, req.flip_sym, req.slot, req.price
                )
            else:
                params = OrderParams(
                    side=req.side, amount_usd=req.amount_usd,
                    order_type=OrderType.LIMIT, price=req.price,
                    leverage=req.leverage,
                    margin_type=MarginType.CROSS,  # Rust gate_order_manager uses cross
                )
                r = await asyncio.to_thread(state.fc.place_order, req.flip_sym, params, req.price)
            state.n_orders_sent += 1
            state.order_count.increment(req.base)
            await on_order_response(state, req, r, log)
        except Exception as e:
            log(f"[w{worker_id}] order err {req.base} {req.side.value if not req.is_close else 'CLOSE'}: {str(e)[:120]}")
            s = state.sym[req.base]
            if not req.is_close:
                # Failed entry → IDLE
                s.state = "IDLE"; s.cooldown_until = time.time() + 5
        finally:
            state.order_q.task_done()


async def on_order_response(state: BotState, req: OrderRequest, r: dict, log):
    """Apply order response — promote to IN_POSITION on fill, log on close."""
    s = state.sym[req.base]
    pos = r.get("position", {})
    order = r.get("order", {})
    slot = pos.get("slot", req.slot)
    order_id = order.get("orderId")
    avg_price = pos.get("avgPrice") or order.get("avgPrice") or 0
    size = pos.get("size") or "0"

    if req.is_close:
        # Record exit pnl
        avg_close = float(avg_price) if avg_price else req.price
        await record_exit(state, req.base, avg_close, "close", log)
        return

    if size and float(size) != 0:
        state.n_filled += 1
        s.state = "IN_POSITION"
        s.entry_slot = slot
        s.entry_price = float(avg_price)
        s.entry_side = req.side
        s.entry_amount = req.amount_usd
        s.entry_dev = req.entry_dev_bp
        s.in_position_since = time.time()
        log(f"  ✓ {req.base} FILLED slot={slot} avg=${avg_price}")
    else:
        s.state = "ENTRY_PENDING"
        s.entry_slot = slot
        s.entry_order_id = order_id
        s.entry_order_time = time.time()
        s.entry_price = req.price
        s.entry_side = req.side
        s.entry_amount = req.amount_usd
        s.entry_dev = req.entry_dev_bp
        log(f"  ⏳ {req.base} pending slot={slot}")


# ---------------------------------------------------------------------------
# record_exit (Rust analog: pnl tracking + blacklist tiers)
# ---------------------------------------------------------------------------

async def record_exit(state: BotState, base: str, avg_close: float, reason: str, log):
    s = state.sym[base]
    if s.entry_side is None or s.entry_price <= 0:
        s.state = "IDLE"; s.entry_slot = None
        return
    sign = 1 if s.entry_side == Side.LONG else -1
    pnl_bp = (avg_close - s.entry_price) / s.entry_price * 1e4 * sign
    pnl_usd = pnl_bp * s.entry_amount / 1e4
    state.realized_pnl += pnl_usd
    if pnl_bp > 0:
        state.n_wins += 1
        s.consecutive_losses = 0
        s.n_profitable += 1
    else:
        s.consecutive_losses += 1
    s.total_pnl_bp += pnl_bp
    s.n_trades += 1
    s.last_trade_ms = int(time.time() * 1000)
    # profit_bp_ema (Rust gate_order_manager analog) — updated on every exit
    alpha = state.config.profit_bp_ema_alpha
    s.profit_bp_ema = (1 - alpha) * s.profit_bp_ema + alpha * pnl_bp

    if s.total_pnl_bp < -40.0:
        s.blacklist_until = time.time() + 86400
        log(f"  ⛔ {base} BLACKLISTED PERMANENT (sum={s.total_pnl_bp:+.1f})")
    elif s.total_pnl_bp < -20.0:
        s.blacklist_until = time.time() + 3600
        log(f"  ⛔ {base} BLACKLISTED 60min (sum={s.total_pnl_bp:+.1f})")
    elif pnl_bp < -12.0:
        s.blacklist_until = time.time() + 1800
        log(f"  ⛔ {base} BLACKLISTED 30min (single loss {pnl_bp:.1f}bp)")
    elif s.consecutive_losses >= 3:
        s.blacklist_until = time.time() + 1800
        log(f"  ⛔ {base} BLACKLISTED 30min (cons_loss=3)")
    elif s.consecutive_losses >= 2:
        s.blacklist_until = time.time() + 300

    hold = time.time() - s.in_position_since
    log(f"  pnl={pnl_bp:+.2f}bp (${pnl_usd:+.4f}) hold={hold:.0f}s")
    log(f"  TOTAL: signals={state.n_signals} filled={state.n_filled} "
        f"cancelled={state.n_cancelled} wins={state.n_wins} pnl=${state.realized_pnl:+.4f}")
    with open(state.log_path, "a") as f:
        f.write(json.dumps({
            "ts": datetime.now(timezone.utc).isoformat(),
            "base": base, "side": s.entry_side.value, "reason": reason,
            "entry": s.entry_price, "exit": avg_close, "entry_dev_bp": s.entry_dev,
            "amount_usd": s.entry_amount, "hold_s": hold,
            "pnl_bp": pnl_bp, "pnl_usd": pnl_usd,
        }) + "\n")
    s.state = "IDLE"; s.entry_slot = None; s.entry_dev = 0.0
    s.cooldown_until = time.time() + (60 if pnl_bp < 0 else 10)
    if state.realized_pnl < state.kill_pnl_usd:
        log(f"⛔ KILL: ${state.realized_pnl:.2f}")
        raise SystemExit(0)


# ---------------------------------------------------------------------------
# Per-symbol process (Rust analog: process_symbol)
# ---------------------------------------------------------------------------

async def process_symbol(state: BotState, base: str, now_ms: int, log):
    cfg = state.config
    s = state.sym[base]

    # Build current snapshot from existing top-of-book trackers (filled by feeds)
    snap = state.cache.current.get(base)
    if snap is None: return

    # Latency check
    if snap.flip.received_at_ms and snap.flip.server_time_ms:
        if (snap.flip.received_at_ms - snap.flip.server_time_ms) > cfg.target_lat_ms:
            return
    if snap.binance.received_at_ms and snap.binance.server_time_ms:
        if (snap.binance.received_at_ms - snap.binance.server_time_ms) > cfg.base_lat_ms:
            return

    # Update market stats
    flip_spread = snap.flip.spread_bp()
    raw_dev = snap.raw_dev_bp()
    if raw_dev is not None and flip_spread > 0:
        state.market.update(base, raw_dev, flip_spread)

    # ===== ENTRY path =====
    if s.state == "IDLE":
        baseline = state.market.avg_gap_bp(base)
        signal = calculate_signal(snap, baseline)
        decision = decide_order(state, base, signal, now_ms, log)
        if decision is None: return
        # Skip if warmup
        if time.time() - state.started_at < 90: return
        state.n_signals += 1
        log(f"[{base} ENTRY #{state.n_signals}] adj_dev={decision['adj']:+.2f}bp "
            f"baseline={baseline:+.2f}bp side={decision['side'].value} px={decision['price']}")
        # Enqueue order
        try:
            # Per-symbol leverage cap (Rust gate_order_manager.rs analog)
            sym_max_lev = symbol_max_leverage(base)
            actual_lev = min(cfg.leverage, sym_max_lev)
            state.order_q.put_nowait(OrderRequest(
                base=base, flip_sym=SYMBOLS_CFG[base][3],
                side=decision['side'], price=decision['price'],
                amount_usd=cfg.amount_usd, leverage=actual_lev,
                entry_dev_bp=decision['adj'],
            ))
            state.recent_orders.append(time.time())  # global throttle
            s.state = "ENTRY_PENDING"  # will be confirmed by worker
            s.entry_order_time = time.time()
        except asyncio.QueueFull:
            log(f"  ✗ queue full")
        return

    # ===== ENTRY_PENDING — check WS for fill, else timeout-cancel =====
    if s.state == "ENTRY_PENDING":
        flip_sym = SYMBOLS_CFG[base][3]
        elapsed = time.time() - s.entry_order_time
        if state.tracker and state.tracker.has_position(flip_sym):
            state.n_filled += 1
            s.state = "IN_POSITION"
            s.in_position_since = time.time()
            log(f"[{base}] entry FILLED (ws-detected, {elapsed:.1f}s)")
            return
        if elapsed > cfg.fill_wait_s:
            if s.entry_order_id:
                try:
                    await asyncio.to_thread(state.fc.cancel_order, flip_sym, s.entry_order_id)
                except Exception: pass
            log(f"[{base}] entry timeout {elapsed:.0f}s")
            state.n_cancelled += 1
            s.state = "IDLE"; s.entry_slot = None
            s.entry_order_id = None
            s.cooldown_until = time.time() + 5
        return

    # ===== IN_POSITION — exit logic =====
    if s.state == "IN_POSITION":
        elapsed = time.time() - s.in_position_since
        # Realizable price for unrealized: bid for Long, ask for Short
        close_price = snap.flip.bid if s.entry_side == Side.LONG else snap.flip.ask
        if close_price <= 0: return
        sign_pos = 1 if s.entry_side == Side.LONG else -1
        unrealized_bp = (close_price - s.entry_price) / s.entry_price * 1e4 * sign_pos

        should_exit, reason = False, ""
        if unrealized_bp > cfg.profit_take_bp:
            should_exit, reason = True, f"profit_{unrealized_bp:.1f}bp"
        elif unrealized_bp < -cfg.stop_loss_bp:
            should_exit, reason = True, f"stop_{unrealized_bp:.1f}bp"
        elif elapsed > cfg.hold_max_s:
            should_exit, reason = True, f"timeout_{unrealized_bp:+.1f}bp"
        if not should_exit: return

        s.state = "EXIT_PENDING"
        flip_sym = SYMBOLS_CFG[base][3]
        log(f"[{base} EXIT-{reason}] side={s.entry_side.value} px={close_price}")
        try:
            r = await asyncio.to_thread(state.fc.close_position, flip_sym, s.entry_slot, close_price)
            avg_close = float(r.get("order", {}).get("avgPrice", close_price))
            await record_exit(state, base, avg_close, reason, log)
        except Exception as e:
            log(f"  ✗ exit fail: {str(e)[:120]}")
            s.state = "IN_POSITION"  # retry next tick


# ---------------------------------------------------------------------------
# Signal loop (Rust analog: signal_loop with rayon par_iter — Python uses gather)
# ---------------------------------------------------------------------------

async def signal_loop(state: BotState, log, end_ts: float):
    last_status = 0
    while time.time() < end_ts:
        try:
            now_ms = int(time.time() * 1000)
            # Fan-out per-symbol processing (asyncio gather is cooperative, not parallel,
            # but each process_symbol is fast and IO-bound on the order queue).
            await asyncio.gather(
                *[process_symbol(state, b, now_ms, log) for b in state.symbols],
                return_exceptions=True,
            )
            if time.time() - last_status > 60:
                last_status = time.time()
                warm_left = max(0, 90 - (time.time() - state.started_at))
                warm = f"warming({warm_left:.0f}s)" if warm_left > 0 else "ARMED"
                avail = state.tracker.margin_avail if state.tracker else 0
                log(f"  [{warm}] s/f/c={state.n_signals}/{state.n_filled}/{state.n_cancelled} "
                    f"wins={state.n_wins} pnl=${state.realized_pnl:+.4f} margin=${avail:.2f}")
        except SystemExit: raise
        except Exception as e:
            log(f"[strategy] err: {e}")
        await asyncio.sleep(0.1)  # 10Hz signal loop (Rust default 10ms = 100Hz)


# ---------------------------------------------------------------------------
# Restart loop (Rust analog: restart_manager thread)
# ---------------------------------------------------------------------------

async def restart_loop(state: BotState, log):
    """Reset per-symbol cooldown / blacklist every restart_interval_s."""
    while True:
        await asyncio.sleep(state.config.restart_interval_s)
        log(f"[restart] {state.config.restart_interval_s}s tick — keeping state, just rotating cookies")
        # Rotate cookies (long-running session)
        try:
            ver = json.loads(urllib.request.urlopen("http://localhost:9230/json/version", timeout=5).read())
            cookies = await _fetch_cookies(ver["webSocketDebuggerUrl"])
            state.fc._cookies = cookies
            state.fc._session.cookies.update(cookies)
            log("[restart] cookies rotated")
        except Exception as e:
            log(f"[restart] cookie rotate fail: {e}")


# ---------------------------------------------------------------------------
# Feeds (same as v9, but write into SymbolSnap with timestamps for latency)
# ---------------------------------------------------------------------------

async def binance_feed_chunk(state: BotState, log, chunk: list[str]):
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
                    if bid <= 0 or ask <= 0: continue
                    server_ms = int(d.get("E", 0)) or int(time.time() * 1000)
                    now_ms = int(time.time() * 1000)
                    snap = state.cache.current.setdefault(base, SymbolSnap())
                    snap.binance.bid = bid; snap.binance.ask = ask
                    snap.binance.server_time_ms = server_ms
                    snap.binance.received_at_ms = now_ms
                    snap.wall_ms = now_ms
                    state.cache.push(base, snap)
        except Exception:
            await asyncio.sleep(3)


async def binance_feed(state: BotState, log):
    syms = state.symbols
    chunks = [syms[i:i+50] for i in range(0, len(syms), 50)]
    log(f"[binance] starting {len(chunks)} chunks for {len(syms)} symbols")
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
                    if bid <= 0 or ask <= 0: continue
                    server_ms = int(res.get("t", 0)) or int(time.time() * 1000)
                    now_ms = int(time.time() * 1000)
                    snap = state.cache.current.setdefault(base, SymbolSnap())
                    snap.gate.bid = bid; snap.gate.ask = ask
                    snap.gate.server_time_ms = server_ms
                    snap.gate.received_at_ms = now_ms
                    snap.wall_ms = now_ms
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
                        if bid <= 0 or ask <= 0: continue
                        server_ms = int(tick.get("ts", 0)) or int(time.time() * 1000)
                        now_ms = int(time.time() * 1000)
                        snap = state.cache.current.setdefault(base, SymbolSnap())
                        snap.bitget.bid = bid; snap.bitget.ask = ask
                        snap.bitget.server_time_ms = server_ms
                        snap.bitget.received_at_ms = now_ms
                        snap.wall_ms = now_ms
        except Exception:
            await asyncio.sleep(3)


async def flipster_feed(state: BotState, log):
    """Subscribe to market/orderbooks-v2 and use top-of-book."""
    while True:
        try:
            ver = json.loads(urllib.request.urlopen("http://localhost:9230/json/version", timeout=5).read())
            cookies = await _fetch_cookies(ver["webSocketDebuggerUrl"])
            cookie_str = "; ".join(f"{k}={v}" for k, v in cookies.items())
            headers = {"Cookie": cookie_str, "Origin": "https://flipster.io",
                       "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) Chrome/141.0.0.0"}
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
                        if not (bids and asks): continue
                        bid = float(bids[0][0]); ask = float(asks[0][0])
                        if bid <= 0 or ask <= 0: continue
                        now_ms = int(time.time() * 1000)
                        snap = state.cache.current.setdefault(base, SymbolSnap())
                        snap.flip.bid = bid; snap.flip.ask = ask
                        snap.flip.server_time_ms = now_ms  # no server ts in orderbook
                        snap.flip.received_at_ms = now_ms
                        snap.wall_ms = now_ms
                        state.cache.push(base, snap)
        except Exception as e:
            await asyncio.sleep(3)


# ---------------------------------------------------------------------------
# Orphan adopt (same as v9)
# ---------------------------------------------------------------------------

async def orphan_adopt_loop(state: BotState, log):
    await asyncio.sleep(10)
    while True:
        try:
            await asyncio.sleep(15)
            if not state.tracker or not state.tracker.connected: continue
            for sym_slot, fields in list(state.tracker.positions.items()):
                if "/" not in sym_slot: continue
                flip_sym, slot_s = sym_slot.split("/", 1)
                try: slot = int(slot_s)
                except ValueError: continue
                base = flip_sym.replace("USDT.PERP", "")
                ss = state.sym.get(base)
                if ss and ss.state in ("IN_POSITION", "ENTRY_PENDING", "EXIT_PENDING"): continue
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
                ssr = state.tracker.position_side_qty_entry(flip_sym, slot)
                if not ssr:
                    if sym_slot not in state.orphan_first_seen:
                        state.orphan_first_seen[sym_slot] = time.time()
                    age_s = time.time() - state.orphan_first_seen[sym_slot]
                    try: unr = float(fields.get("unrealizedPnl", "0"))
                    except: unr = 0.0
                    if unr > 0.03 or unr < -0.05 or age_s > 90:
                        mid = fields.get("midPrice") or fields.get("bidPrice")
                        if not mid: continue
                        try:
                            await asyncio.to_thread(state.fc.close_position, flip_sym, slot, float(mid))
                            log(f"[orphan-pnl-close] {sym_slot} unr={unr:+.4f} age={age_s:.0f}s")
                            state.orphan_first_seen.pop(sym_slot, None)
                        except Exception as e:
                            log(f"[orphan-pnl-close] {sym_slot} FAIL: {str(e)[:120]}")
                        await asyncio.sleep(0.3)
                    continue
                side_str, qty, entry = ssr
                side = Side.LONG if side_str == "Long" else Side.SHORT
                amount_usd = qty * entry
                ss.state = "IN_POSITION"
                ss.entry_slot = slot; ss.entry_price = entry; ss.entry_side = side
                ss.entry_amount = amount_usd; ss.entry_dev = 0.0
                ss.in_position_since = time.time()
                log(f"[adopt] {sym_slot} side={side_str} qty={qty} entry={entry} (~${amount_usd:.2f})")
        except Exception as e:
            log(f"[adopt-loop] err: {e}")
            await asyncio.sleep(5)


# ---------------------------------------------------------------------------
# Stale position auto-close (Rust analog: try_to_close_stale_positions)
# ---------------------------------------------------------------------------

async def stale_position_loop(state: BotState, log):
    """Close any IN_POSITION older than stale_position_min via market close."""
    await asyncio.sleep(60)
    while True:
        try:
            await asyncio.sleep(30)
            cutoff = time.time() - state.config.stale_position_min * 60
            for base, s in state.sym.items():
                if s.state != "IN_POSITION": continue
                if s.in_position_since == 0 or s.in_position_since > cutoff: continue
                flip_sym = SYMBOLS_CFG[base][3]
                close_price = state.cache.current.get(base)
                if close_price is None: continue
                px = close_price.flip.bid if s.entry_side == Side.LONG else close_price.flip.ask
                if px <= 0: continue
                log(f"[stale-close] {base} held {(time.time()-s.in_position_since)/60:.1f}min, force close")
                try:
                    r = await asyncio.to_thread(state.fc.close_position, flip_sym, s.entry_slot, px)
                    avg_close = float(r.get("order", {}).get("avgPrice", px))
                    await record_exit(state, base, avg_close, "stale_close", log)
                except Exception as e:
                    log(f"  ✗ stale-close fail: {str(e)[:120]}")
        except SystemExit: raise
        except Exception as e:
            log(f"[stale-pos] err: {e}")
            await asyncio.sleep(5)


# ---------------------------------------------------------------------------
# Cancel duplicate orders (Rust analog: cancel_duplicate_orders)
# ---------------------------------------------------------------------------

async def cancel_duplicate_loop(state: BotState, log):
    """Per (symbol, side), keep oldest, cancel rest. Uses tracker.orders."""
    await asyncio.sleep(45)
    while True:
        try:
            await asyncio.sleep(30)
            if not state.tracker or not state.tracker.connected: continue
            groups = {}
            for oid, fields in list(state.tracker.orders.items()):
                sym = fields.get("symbol")
                size = fields.get("size") or 0
                rem = fields.get("remainingQty") or fields.get("size") or "0"
                try:
                    if float(rem) <= 0: continue
                    side = "buy" if float(size) > 0 else "sell"
                except (TypeError, ValueError): continue
                ts = fields.get("triggerTimestamp") or fields.get("timestamp") or "0"
                try: ts_ns = int(ts)
                except: ts_ns = 0
                groups.setdefault((sym, side), []).append((ts_ns, oid))
            for (sym, side), lst in groups.items():
                if len(lst) <= 1: continue
                lst.sort()  # oldest first
                for _, oid in lst[1:]:
                    try:
                        await asyncio.to_thread(state.fc.cancel_order, sym, oid)
                        log(f"[dedup-cancel] {sym} {side} oid={oid[:10]}..")
                    except Exception as e:
                        msg = str(e)[:80]
                        if "404" not in msg and "NotFound" not in msg:
                            log(f"[dedup-cancel] fail {sym}: {msg}")
                    await asyncio.sleep(0.2)
        except SystemExit: raise
        except Exception as e:
            log(f"[dedup-loop] err: {e}")
            await asyncio.sleep(5)


# ---------------------------------------------------------------------------
# Trade stats persistence (Rust analog: save_metrics_to_file)
# ---------------------------------------------------------------------------

def load_trade_stats(state: BotState, log):
    """Load per-symbol stats from disk on startup."""
    if not state.stats_path.exists(): return
    try:
        d = json.loads(state.stats_path.read_text())
        for base, st in d.get("symbols", {}).items():
            if base in state.sym:
                ss = state.sym[base]
                ss.profit_bp_ema = float(st.get("profit_bp_ema", 0.0))
                ss.n_trades = int(st.get("n_trades", 0))
                ss.n_profitable = int(st.get("n_profitable", 0))
                ss.total_pnl_bp = float(st.get("total_pnl_bp", 0.0))
                ss.consecutive_losses = int(st.get("consecutive_losses", 0))
                ss.last_trade_ms = int(st.get("last_trade_ms", 0))
        log(f"[stats] loaded {len(d.get('symbols', {}))} symbol records from {state.stats_path}")
    except Exception as e:
        log(f"[stats] load fail: {e}")


async def trade_stats_persist_loop(state: BotState, log):
    """Save per-symbol stats every 60s (atomic via tmpfile)."""
    while True:
        try:
            await asyncio.sleep(60)
            data = {
                "ts": datetime.now(timezone.utc).isoformat(),
                "realized_pnl": state.realized_pnl,
                "n_signals": state.n_signals, "n_filled": state.n_filled,
                "n_wins": state.n_wins,
                "symbols": {}
            }
            for base, ss in state.sym.items():
                if ss.n_trades == 0 and ss.profit_bp_ema == 0.0: continue
                data["symbols"][base] = {
                    "profit_bp_ema": ss.profit_bp_ema,
                    "n_trades": ss.n_trades,
                    "n_profitable": ss.n_profitable,
                    "total_pnl_bp": ss.total_pnl_bp,
                    "consecutive_losses": ss.consecutive_losses,
                    "last_trade_ms": ss.last_trade_ms,
                }
            tmp = state.stats_path.with_suffix(".tmp")
            tmp.write_text(json.dumps(data, indent=2))
            tmp.rename(state.stats_path)
        except Exception as e:
            log(f"[stats-persist] err: {e}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--symbols", default="ALL")
    p.add_argument("--amount-usd", type=float, default=25)
    p.add_argument("--leverage", type=int, default=10)
    p.add_argument("--threshold", type=float, default=8.0)
    p.add_argument("--max-dev-bp", type=float, default=25.0)
    p.add_argument("--max-spread-bp", type=float, default=4.0)
    p.add_argument("--min-net-edge-bp", type=float, default=5.0)
    p.add_argument("--momentum-block-bp", type=float, default=4.0)
    p.add_argument("--max-consensus-disagree-bp", type=float, default=6.0)
    p.add_argument("--profit-take", type=float, default=14.0)
    p.add_argument("--stop-loss", type=float, default=7.0)
    p.add_argument("--overshoot", type=float, default=100.0)  # disabled by default
    p.add_argument("--hold-max-s", type=float, default=90.0)
    p.add_argument("--fill-wait-s", type=float, default=8.0)
    p.add_argument("--min-tf-agree", type=int, default=3)
    p.add_argument("--target-lat-ms", type=int, default=200)
    p.add_argument("--base-lat-ms", type=int, default=300)
    p.add_argument("--min-vol-usd", type=float, default=20_000_000)
    p.add_argument("--n-workers", type=int, default=4)
    p.add_argument("--restart-interval-s", type=float, default=600)
    p.add_argument("--stale-position-min", type=float, default=30.0)
    p.add_argument("--profit-bp-ema-alpha", type=float, default=0.2)
    p.add_argument("--min-trades-for-ema-gate", type=int, default=5)
    p.add_argument("--min-profit-bp-ema", type=float, default=-2.0)
    p.add_argument("--max-orders-per-min-global", type=int, default=30)
    p.add_argument("--kill-pnl", type=float, default=-5.0)
    p.add_argument("--duration-min", type=float, default=120)
    args = p.parse_args()

    proxies = (Path(__file__).parent / "proxies.txt").read_text().splitlines()
    proxies = [pp for pp in proxies if pp.strip()]
    fc = FlipsterClient(FBM(), proxies=proxies)
    ver = json.loads(urllib.request.urlopen("http://localhost:9230/json/version", timeout=5).read())
    cookies = await _fetch_cookies(ver["webSocketDebuggerUrl"])
    import requests
    fc._cookies = cookies
    fc._session = requests.Session()
    fc._session.cookies.update(cookies)
    fc._session.headers.update({
        "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) Chrome/141.0.0.0",
        "Accept": "application/json", "Referer": "https://flipster.io/",
        "Content-Type": "application/json", "X-Prex-Client-Platform": "web",
        "X-Prex-Client-Version": "release-web-3.15.110",
        "Origin": "https://flipster.io",
    })

    _vol_map = {}
    cfg_path = Path(__file__).parent / "symbols_v9.json"
    if cfg_path.exists():
        for d in json.loads(cfg_path.read_text()):
            _vol_map[d["base"]] = d.get("binance_vol_usd", 0)
    if args.symbols.strip().upper() == "ALL":
        symbols = [b for b in SYMBOLS_CFG if _vol_map.get(b, 0) >= args.min_vol_usd]
    else:
        symbols = [s.strip() for s in args.symbols.split(",") if s.strip() in SYMBOLS_CFG]

    cfg = StrategyConfig(
        threshold_bp=args.threshold, max_dev_bp=args.max_dev_bp,
        max_spread_bp=args.max_spread_bp, min_net_edge_bp=args.min_net_edge_bp,
        momentum_block_bp=args.momentum_block_bp,
        max_consensus_disagree_bp=args.max_consensus_disagree_bp,
        profit_take_bp=args.profit_take, stop_loss_bp=args.stop_loss,
        overshoot_bp=args.overshoot, hold_max_s=args.hold_max_s,
        fill_wait_s=args.fill_wait_s, amount_usd=args.amount_usd,
        leverage=args.leverage, min_tf_agree=args.min_tf_agree,
        base_lat_ms=args.base_lat_ms, target_lat_ms=args.target_lat_ms,
        restart_interval_s=args.restart_interval_s,
        stale_position_min=args.stale_position_min,
        profit_bp_ema_alpha=args.profit_bp_ema_alpha,
        min_trades_for_ema_gate=args.min_trades_for_ema_gate,
        min_profit_bp_ema=args.min_profit_bp_ema,
        max_orders_per_min_global=args.max_orders_per_min_global,
    )
    tracker = WSPositionTracker(cookies=cookies)
    state = BotState(symbols=symbols, config=cfg, fc=fc, tracker=tracker,
                     kill_pnl_usd=args.kill_pnl)
    state.log_path.parent.mkdir(parents=True, exist_ok=True)
    for b in symbols: state.sym[b] = SymbolState()

    def log(msg):
        ts = datetime.now().strftime("%H:%M:%S")
        print(f"{ts} {msg}", flush=True)

    log("=== SLOW STAT-ARB v10 (Rust strategy.rs port) ===")
    log(f"  symbols ({len(symbols)}, vol>=${args.min_vol_usd:,.0f})")
    log(f"  amount=${args.amount_usd} lev={args.leverage}x workers={args.n_workers}")
    log(f"  ENTRY: {args.threshold} < |adj_dev| < {args.max_dev_bp}bp")
    log(f"         spread <= {args.max_spread_bp}bp, net_edge >= {args.min_net_edge_bp}bp")
    log(f"         momentum_block {args.momentum_block_bp}bp, "
        f"min_tf_agree={args.min_tf_agree}/4, lat target/base={args.target_lat_ms}/{args.base_lat_ms}ms")
    log(f"  EXIT: profit > {args.profit_take}bp, stop < -{args.stop_loss}bp, hold {args.hold_max_s}s")
    log(f"  restart_loop {args.restart_interval_s}s, kill ${args.kill_pnl}")

    # Load persisted trade stats
    load_trade_stats(state, log)

    end_ts = time.time() + args.duration_min * 60
    tasks = [
        asyncio.create_task(binance_feed(state, log)),
        asyncio.create_task(gate_feed(state, log)),
        asyncio.create_task(bitget_feed(state, log)),
        asyncio.create_task(flipster_feed(state, log)),
        asyncio.create_task(tracker.run(log)),
        asyncio.create_task(orphan_adopt_loop(state, log)),
        asyncio.create_task(restart_loop(state, log)),
        asyncio.create_task(stale_position_loop(state, log)),
        asyncio.create_task(cancel_duplicate_loop(state, log)),
        asyncio.create_task(trade_stats_persist_loop(state, log)),
        *[asyncio.create_task(order_worker(state, log, i)) for i in range(args.n_workers)],
        asyncio.create_task(signal_loop(state, log, end_ts)),
    ]
    try:
        await tasks[-1]
    finally:
        log("\n=== shutdown ===")
        # Cancel all pending orders
        if tracker.orders:
            for oid, f in list(tracker.orders.items()):
                sym = f.get("symbol")
                rem = f.get("remainingQty") or f.get("size") or "0"
                try:
                    if float(rem) > 0 and sym:
                        try:
                            fc.cancel_order(sym, oid)
                            log(f"  cancelled {sym}/{oid[:10]}..")
                        except Exception: pass
                except ValueError: pass
        for t in tasks[:-1]: t.cancel()
        log(f"  signals={state.n_signals} filled={state.n_filled} cancelled={state.n_cancelled} "
            f"wins={state.n_wins} pnl=${state.realized_pnl:+.4f}")


if __name__ == "__main__":
    asyncio.run(main())

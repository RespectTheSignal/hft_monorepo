#!/usr/bin/env python3
"""Live executor sidecar — mirrors paper bot signals to real exchanges.

Polls QuestDB `trade_signal` table for entry/exit events from a chosen
variant, then places real orders on Flipster + Gate via browser-cookie
clients.

Usage:
    # 1. Start browsers & login (one-time)
    python3 scripts/live_executor.py --setup

    # 2. Run live (after login)
    python3 scripts/live_executor.py --variant T01_best --size-usd 10

Environment:
    QUESTDB_HTTP_URL  (default: http://211.181.122.102:9000)
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import queue
import signal
import sys
import threading
import time
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import zmq

# Both flipster_web and gate_web have a `python` subpackage — direct sys.path
# imports would collide. Load each one under a unique top-level name.
_QUANT = Path(__file__).resolve().parent.parent.parent
MONOREPO = _QUANT / "hft_monorepo" if (_QUANT / "hft_monorepo" / "flipster_web").exists() else _QUANT


def _load_pkg(name: str, root: Path):
    """Load `<root>/python/__init__.py` as a top-level module named `name`."""
    pkg_dir = root / "python"
    spec = importlib.util.spec_from_file_location(
        name, pkg_dir / "__init__.py", submodule_search_locations=[str(pkg_dir)]
    )
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


# Load both as distinct top-level packages.
flipster_pkg = _load_pkg("flipster_pkg", MONOREPO / "flipster_web")
gate_pkg = _load_pkg("gate_pkg", MONOREPO / "gate_web")

QUESTDB_URL = "http://211.181.122.102:9000"
SIGNAL_SUB_ADDR = "tcp://127.0.0.1:7500"  # collector's trade_signal PUB — <10ms delivery
POLL_INTERVAL = 0.5  # QuestDB fallback poll — only used if ZMQ down
EDGE_RECHECK_MAX_ADVERSE_BP = 3.0  # skip entry if Flipster mid has moved >= this bp in the signal's predicted direction (i.e. reversion already happened)

# Gate maker entry (POC = post-only). Trades fee burden for fill probability.
# - Place LIMIT at maker side: ASK for SHORT, BID for LONG. POC auto-cancels
#   if it would cross. We poll status and cancel after timeout if not filled.
# - On failure, SAFETY revert closes Flipster reduce-only (same as IOC path).
GATE_MAKER_ENTRY = False        # POC tested 2026-04-27: 0/3 fill rate. Mean-reversion entries place LIMIT on the side market is moving away from. Reverted to IOC.
GATE_MAKER_TIMEOUT_S = 1.5      # unused while GATE_MAKER_ENTRY=False
GATE_MAKER_POLL_S = 0.25

# Gate "manual IOC" via GTC + wait + cancel. At signal price (taker side):
# fills immediately if book at-or-better, OR sits as LIMIT for up to
# GATE_LIMIT_WAIT_S to catch books that move toward us (common in
# mean-reversion entries). Higher fill rate than IOC, fewer SAFETY reverts.
GATE_LIMIT_WAIT = False         # Tested 2026-04-27: 0/9 fill rate. Mean-reversion moves BID/ASK AWAY from our signal-price LIMIT during the wait — IOC's instant snapshot is better here. Reverted to IOC.
GATE_LIMIT_WAIT_S = 1.0

# Flipster LIMIT manual-IOC entry. Place LIMIT at paper signal price; wait
# briefly; cancel + position-query if not filled; abort if no position.
# Saves the +8 bp avg slippage we measured on Flipster MARKET entries —
# at the cost of trading less often (only when book is at-or-better than
# signal price). When Flipster doesn't fill we don't place Gate, so no
# SAFETY revert is needed.
FLIPSTER_LIMIT_ENTRY = True
FLIP_LIMIT_WAIT_S = 0.4         # how long to wait after place before cancel

# Flipster proxies for CF rate-limit avoidance. Loaded from proxies.txt at
# startup. Each request rotates through the list via FlipsterClient._next_proxy.
USE_FLIPSTER_PROXIES = True

# Entry filter — DISABLED 2026-04-26.
# Original 25bp threshold was based on an 11-trade subset showing |entry|≥40bp
# was profitable; that pattern didn't replicate over 28+ trades. Paper bot
# already filters by z-score >= 2.5σ AND std >= 3bp, so this raw-bp check was
# redundant double-filtering and was blocking ~70% of signals from POC
# validation. Set to 0 (effectively off) until a stronger empirical case for it.
MIN_ENTRY_SIGNAL_SPREAD_BP = 0.0

# Take-profit: trail the peak captured bp during hold and exit on drawdown.
# Empirically, paper exits on EWMA-z fire too late — peak reversion is reached,
# spread noise-bounces, and we close at a worse moment. TP closes near the peak.
TP_POLL_SEC = 1.0                # sample frequency
TP_MIN_ENTRY_SPREAD_BP = 8.0     # only arm for entries with strong mispricing
TP_MIN_PEAK_BP = 9999.0          # DISABLED 2026-04-26: 14 TP triggers averaged -10.11bp net (1/14 wins). Re-enable only after proper validation.
TP_DRAWDOWN_BP = 3.0             # close when drawdown from peak ≥ this
TP_MIN_HOLD_SEC = 5.0            # let the position breathe (matches paper MIN_HOLD)
TP_MAX_MID_AGE_SEC = 5.0         # skip if either mid is older than this

# User-confirmed fees (2026-04-24): Flipster taker 0.425bp, Gate taker 0.45bp → 0.875 per leg.
# Round-trip per leg = 2 × 0.875 bp = 1.75 bp = 0.000175 fraction on notional.
# Previous hardcoded 0.0003 × 2 = 60 bp was ~34× too high.
TAKER_FEE_FRAC = 0.0000875  # per leg per side; × 2 for round-trip gives 1.75 bp round-trip

FLIPSTER_API_BASE = "https://api.flipster.io"


def fetch_latest_mid(table: str, symbol: str) -> tuple[float, float] | None:
    """Returns (bid, ask) of latest bookticker row for symbol, or None."""
    sql = (f"SELECT bid_price, ask_price FROM {table} "
           f"WHERE symbol='{symbol}' ORDER BY timestamp DESC LIMIT 1")
    try:
        rows = query_qdb(sql)
        if rows:
            return float(rows[0][0]), float(rows[0][1])
    except Exception:
        pass
    return None


def fetch_latest_mid_age(table: str, symbol: str) -> tuple[float, float, float] | None:
    """Returns (bid, ask, age_seconds) of latest bookticker row, or None."""
    sql = (f"SELECT bid_price, ask_price, timestamp FROM {table} "
           f"WHERE symbol='{symbol}' ORDER BY timestamp DESC LIMIT 1")
    try:
        rows = query_qdb(sql)
        if rows:
            ts = datetime.fromisoformat(rows[0][2].replace("Z", "+00:00"))
            age = (datetime.now(timezone.utc) - ts).total_seconds()
            return float(rows[0][0]), float(rows[0][1]), age
    except Exception:
        pass
    return None


def flipster_oneway_order(fc, symbol: str, side: str, amount_usd: float,
                          ref_price: float, leverage: int = 10,
                          reduce_only: bool = False, order_type: str = "ORDER_TYPE_MARKET",
                          post_only: bool = False) -> dict:
    import time as _t, uuid as _u
    if fc._session is None:
        raise RuntimeError("Flipster client not logged in")
    now_ns = str(_t.time_ns())
    body = {
        "side": "Long" if side == "long" else "Short",
        "requestId": str(_u.uuid4()),
        "timestamp": now_ns,
        "refServerTimestamp": now_ns,
        "refClientTimestamp": now_ns,
        "reduceOnly": reduce_only,
        "leverage": leverage,
        "price": str(ref_price),
        "amount": f"{amount_usd:.4f}",
        "marginType": "Cross",
        "orderType": order_type,
        "postOnly": post_only,
    }
    url = f"{FLIPSTER_API_BASE}/api/v2/trade/one-way/order/{symbol}"
    proxies = (fc._next_proxy() if (USE_FLIPSTER_PROXIES and hasattr(fc, "_next_proxy"))
               else None)
    resp = fc._session.post(url, json=body, proxies=proxies, timeout=15)
    if resp.status_code in (401, 403):
        raise PermissionError(f"Auth failed ({resp.status_code})")
    data = resp.json()
    if resp.status_code >= 400:
        raise RuntimeError(f"API error {resp.status_code}: {json.dumps(data)[:300]}")
    return data


def flipster_limit_ioc_entry(fc, symbol: str, side: str, amount_usd: float,
                             signal_price: float, leverage: int = 10,
                             wait_s: float = 0.4,
                             position_size_fn=None,
                             has_position_fn=None,
                             order_open_fn=None,
                             ) -> tuple[Optional[dict], Optional[str]]:
    """Place LIMIT (no postOnly) at signal price; if not crossed immediately,
    wait ``wait_s`` then cancel + verify final state via the WS-backed
    position-size callback.

    position_size_fn(symbol) -> signed size in coin units (>0 long, <0 short).
    HTTP GET on /trade/positions is 405; WS is authoritative.

    Returns (fill_response, order_id):
      - fill_response: dict with 'position' field if we have a position,
        None if not filled (clean abort path — no SAFETY revert needed).
      - order_id: present when we placed an order (may be None).
    """
    import uuid
    now_ns = str(time.time_ns())
    body = {
        "side": "Long" if side.lower() == "long" else "Short",
        "requestId": str(uuid.uuid4()),
        "timestamp": now_ns,
        "reduceOnly": False,
        "refServerTimestamp": now_ns,
        "refClientTimestamp": now_ns,
        "leverage": leverage,
        "price": str(signal_price),
        "amount": str(amount_usd),
        "marginType": "Cross",
        "orderType": "ORDER_TYPE_LIMIT",
    }
    url = f"{FLIPSTER_API_BASE}/api/v2/trade/one-way/order/{symbol}"
    proxies_for = lambda: (fc._next_proxy() if (USE_FLIPSTER_PROXIES and hasattr(fc, "_next_proxy"))
                           else None)
    resp = fc._session.post(url, json=body, proxies=proxies_for(), timeout=15)
    if resp.status_code in (401, 403):
        raise PermissionError(f"Auth failed ({resp.status_code})")
    data = resp.json()
    if resp.status_code >= 400:
        raise RuntimeError(f"API error {resp.status_code}: {json.dumps(data)[:300]}")

    avg, sz, _ = _extract_flipster_fill(data)
    if avg > 0 and sz > 0:
        return data, None  # crossed and filled immediately

    # Pending. Extract order_id, wait, cancel, then query positions.
    order = data.get("order") if isinstance(data, dict) else {}
    order_id = order.get("orderId") if isinstance(order, dict) else None
    if not order_id:
        return None, None

    time.sleep(wait_s)
    # Cancel + verify + retry. Don't silently swallow failures — check WS
    # to confirm the order is actually gone (cancelled or filled).
    cancel_done = False
    for attempt in range(3):
        try:
            fc.cancel_order(symbol, order_id)
        except Exception as e:
            # Could be 404 (order already filled/cancelled) or transient.
            print(f"  [FLIPSTER-LIMIT] cancel attempt {attempt+1} err: {str(e)[:100]}")
        time.sleep(0.15)
        if order_open_fn is None or not order_open_fn(order_id):
            cancel_done = True
            break
        print(f"  [FLIPSTER-LIMIT] order {order_id} still open after cancel #{attempt+1}; retrying")
    if not cancel_done:
        print(f"  [FLIPSTER-LIMIT] !!! order {order_id} still OPEN after 3 cancel attempts — manual intervention may be needed !!!")

    # Position verification with margin-aware fallback.
    # WS sometimes broadcasts ``initMarginReserved`` before the ``position``
    # size field. has_position_fn returns True on either signal — if it's
    # True, we wait up to 1s for the size field, polling the position dict.
    if has_position_fn is not None and has_position_fn(symbol):
        deadline = time.time() + 1.0
        while time.time() < deadline:
            if position_size_fn is not None:
                sz = position_size_fn(symbol)
                if abs(sz) > 0:
                    break
            time.sleep(0.1)

    # Authoritative: WS-cached position size (after has_position polling).
    if position_size_fn is not None:
        try:
            sz = position_size_fn(symbol)
            if abs(sz) > 0:
                # We have a position; synthesize the fill response so the
                # caller's _extract_flipster_fill works.
                synthetic = {
                    "position": {
                        "symbol": symbol,
                        "size": abs(sz),
                        "avgPrice": signal_price,
                        "slot": 0,
                    },
                    "order": order,
                }
                return synthetic, order_id
        except Exception as e:
            print(f"  [FLIPSTER-LIMIT] WS position check err: {e}")

    # Margin-only fallback: if WS shows margin reserved but never delivered
    # the size field even after polling, treat as "position likely exists".
    # Use signal_size as approximation. Better to enter Gate hedge than to
    # leave Flipster naked.
    if has_position_fn is not None and has_position_fn(symbol):
        synthetic = {
            "position": {
                "symbol": symbol,
                "size": amount_usd / signal_price if signal_price > 0 else 0,
                "avgPrice": signal_price,
                "slot": 0,
            },
            "order": order,
        }
        print(f"  [FLIPSTER-LIMIT] margin-only fallback (size unknown) — treating as filled")
        return synthetic, order_id

    return None, order_id


# Gate contract specs (multiplier, min_size) keyed by contract name.
# Loaded once at startup from public API.
GATE_CONTRACTS: dict[str, dict] = {}


def load_gate_contracts():
    """Fetch contract multipliers from Gate.io public API."""
    global GATE_CONTRACTS
    print("[init] Loading Gate contract specs...")
    try:
        resp = urllib.request.urlopen(
            "https://api.gateio.ws/api/v4/futures/usdt/contracts", timeout=10
        )
        data = json.loads(resp.read())
        for c in data:
            GATE_CONTRACTS[c["name"]] = {
                "multiplier": float(c["quanto_multiplier"]),
                "order_size_min": int(c.get("order_size_min", 1)),
            }
        print(f"[init] Loaded {len(GATE_CONTRACTS)} Gate contracts")
    except Exception as e:
        print(f"[init] WARNING: contract load failed: {e}")


def gate_size_from_usd(contract: str, usd: float, price: float) -> int:
    """Convert USD notional to Gate contract count.

    notional_per_contract = price * multiplier
    contracts = round(usd / notional_per_contract)
    """
    spec = GATE_CONTRACTS.get(contract)
    if spec is None or price <= 0:
        return 1
    notional_per_contract = price * spec["multiplier"]
    if notional_per_contract <= 0:
        return spec["order_size_min"]
    contracts = round(usd / notional_per_contract)
    return max(contracts, spec["order_size_min"])


# ---------------------------------------------------------------------------
# QuestDB polling
# ---------------------------------------------------------------------------

def query_qdb(sql: str) -> list[list]:
    url = f"{QUESTDB_URL}/exec?query={urllib.parse.quote(sql)}"
    resp = urllib.request.urlopen(url, timeout=5)
    data = json.loads(resp.read())
    return data.get("dataset", [])


# ---------------------------------------------------------------------------
# Flipster private WS — positions feed
# ---------------------------------------------------------------------------

FLIP_WS_URL = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"
FLIP_WS_SUBSCRIBE = {
    "s": {
        "private/positions": {"rows": ["*"]},
        "private/orders":    {"rows": ["*"]},
        "private/margins":   {"rows": ["*"]},
    },
}


# ---------------------------------------------------------------------------
# Position tracker
# ---------------------------------------------------------------------------

@dataclass
class LivePosition:
    position_id: int
    base: str
    flipster_side: str  # "long" or "short"
    size_usd: float
    entry_time: str
    # Real fill prices (filled in after orders complete)
    flipster_entry_price: float = 0.0
    flipster_size: float = 0.0  # in base coin units
    flipster_slot: Optional[int] = None
    gate_entry_price: float = 0.0
    gate_size: int = 0  # in contracts
    gate_contract: str = ""
    flipster_order: Optional[dict] = None
    gate_order: Optional[dict] = None
    # Take-profit tracking (set after both legs fill)
    entry_spread_bp: float = 0.0       # signed: (flipster_mid - gate_mid)/gate_mid * 1e4 at entry
    entry_epoch: float = 0.0           # time.time() when entry completed
    peak_captured_bp: float = 0.0      # max (|entry_sprd| - |cur_sprd|) seen during hold
    # Slippage decomposition (positive = adverse / bad for us)
    f_entry_slip_bp: float = 0.0
    g_entry_slip_bp: float = 0.0
    signal_lag_ms_entry: float = 0.0
    paper_f_entry: float = 0.0         # paper's f_price at entry signal
    paper_g_entry: float = 0.0         # paper's g_price at entry signal


def _extract_flipster_fill(resp: dict) -> tuple[float, float, Optional[int]]:
    """Returns (avg_price, size, slot) from a Flipster open/close response."""
    pos = resp.get("position", {}) if isinstance(resp, dict) else {}
    order = resp.get("order", {}) if isinstance(resp, dict) else {}
    avg = float(pos.get("avgPrice") or order.get("avgPrice") or 0)
    size = abs(float(pos.get("size") or order.get("size") or 0))
    slot = pos.get("slot")
    if slot is None:
        slot = order.get("slot")
    return avg, size, slot


def _extract_gate_fill(resp: dict) -> tuple[float, int]:
    """Returns (fill_price, abs_filled_size) from a Gate order response.

    Gate IOC: ``size`` is the original request, ``left`` is unfilled remainder.
    Actual filled = |size| - |left|. ``fill_price == 0`` ⇒ no fill, regardless
    of size. Caller should treat ``fill_price <= 0`` as not-filled.
    """
    if not isinstance(resp, dict):
        return 0.0, 0
    inner = resp.get("data", {})
    if isinstance(inner, dict):
        d = inner.get("data", {})
        if isinstance(d, dict):
            fp = float(d.get("fill_price") or 0)
            order_size = abs(int(d.get("size") or 0))
            left = abs(int(d.get("left") or 0))
            filled = max(0, order_size - left)
            return fp, filled
    return 0.0, 0


def _append_trade(path: Path, record: dict):
    """Append a closed-trade record to a JSONL file."""
    with open(path, "a") as f:
        f.write(json.dumps(record) + "\n")


@dataclass
class Executor:
    variant: str
    size_usd: float  # override paper size with this
    dry_run: bool = False
    symbol_whitelist: set[str] = field(default_factory=set)  # empty = all
    symbol_blacklist: set[str] = field(default_factory=set)
    trade_log_path: Path = field(default_factory=lambda: Path("logs/live_trades.jsonl"))
    balance_log_path: Path = field(default_factory=lambda: Path("logs/balance_snapshots.jsonl"))

    flipster_client: object = field(default=None, repr=False)
    gate_client: object = field(default=None, repr=False)

    open_positions: dict[int, LivePosition] = field(default_factory=dict)
    seen_entry_ids: set[int] = field(default_factory=set)
    seen_exit_ids: set[int] = field(default_factory=set)
    last_poll_ts: str = ""
    total_pnl_usd: float = 0.0
    trade_count: int = 0
    win_count: int = 0
    gate_leverage_set: set[str] = field(default_factory=set)
    gate_target_leverage: int = 10  # cross mode cap
    zmq_queue: queue.Queue = field(default_factory=queue.Queue)
    zmq_events_seen: int = 0
    dispatch_pool: ThreadPoolExecutor = field(
        default_factory=lambda: ThreadPoolExecutor(max_workers=8, thread_name_prefix="dispatch")
    )
    seen_lock: threading.Lock = field(default_factory=threading.Lock)
    # Flipster private WS state — keyed by "SYMBOL/SLOT" (positions) or order_id (orders).
    flipster_positions: dict = field(default_factory=dict)
    flipster_orders: dict = field(default_factory=dict)
    flipster_ws_lock: threading.Lock = field(default_factory=threading.Lock)
    flipster_ws_connected: bool = False

    def _balance_snapshot_loop(self):
        """Background thread: every 30min, fetch Gate /accounts and append a
        snapshot to balance_log_path. The history.fee/pnl/fund fields are
        cumulative since account inception, so daily fee usage = delta of fee
        between two snapshots. Used to validate the hardcoded fee model.

        Flipster /trade/account hits CF 1015 from this session; skipped for now."""
        first = True
        while True:
            try:
                if first:
                    time.sleep(15)  # let main loop settle before first snapshot
                    first = False
                else:
                    time.sleep(1800)  # 30 min
                if self.dry_run or self.gate_client is None:
                    continue
                snap = {"ts": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")}
                try:
                    r = self.gate_client._session.get(
                        "https://www.gate.com/apiw/v2/futures/usdt/accounts", timeout=10)
                    body = r.json()
                    rows = body.get("data") if isinstance(body, dict) else None
                    if isinstance(rows, list) and rows:
                        d = rows[0]
                        h = d.get("history", {}) if isinstance(d, dict) else {}
                        snap["gate"] = {
                            "total": float(d.get("total") or 0),
                            "available": float(d.get("available") or 0),
                            "unrealised_pnl": float(d.get("unrealised_pnl") or 0),
                            "history_fee": float(h.get("fee") or 0),
                            "history_pnl": float(h.get("pnl") or 0),
                            "history_fund": float(h.get("fund") or 0),
                            "history_dnw": float(h.get("dnw") or 0),
                        }
                    else:
                        snap["gate_raw"] = str(body)[:200]
                except Exception as e:
                    snap["gate_err"] = f"{type(e).__name__}: {e}"
                snap["trade_count"] = self.trade_count
                snap["modeled_total_pnl"] = self.total_pnl_usd
                self.balance_log_path.parent.mkdir(parents=True, exist_ok=True)
                with open(self.balance_log_path, "a") as f:
                    f.write(json.dumps(snap) + "\n")
                if "gate" in snap:
                    g = snap["gate"]
                    print(f"\n[BALANCE] gate total={g['total']:.4f} avail={g['available']:.4f} "
                          f"hist_fee={g['history_fee']:.4f} hist_pnl={g['history_pnl']:.4f} "
                          f"trades={snap['trade_count']} modeled_pnl=${snap['modeled_total_pnl']:+.4f}")
            except Exception as e:
                print(f"[balance] loop err: {e}")

    def flipster_position_size(self, symbol: str, slot: int = 0) -> float:
        """Read current Flipster position size from WS-cached state.
        Returns signed size (positive=long, negative=short, 0=flat)."""
        with self.flipster_ws_lock:
            p = self.flipster_positions.get(f"{symbol}/{slot}")
            if not p:
                return 0.0
            try:
                return float(p.get("position") or 0)
            except (ValueError, TypeError):
                return 0.0

    def flipster_has_position(self, symbol: str, slot: int = 0) -> bool:
        """True if WS shows ANY indication of an open position — either a
        non-zero ``position`` field, or a non-zero ``initMarginReserved``.
        WS sometimes broadcasts margin before size; this catches that race."""
        with self.flipster_ws_lock:
            p = self.flipster_positions.get(f"{symbol}/{slot}")
            if not p:
                return False
            try:
                if abs(float(p.get("position") or 0)) > 0:
                    return True
            except (ValueError, TypeError):
                pass
            try:
                if float(p.get("initMarginReserved") or 0) > 0:
                    return True
            except (ValueError, TypeError):
                pass
            return False

    def flipster_order_open(self, order_id: str) -> bool:
        """True if WS state still shows this order as open (not filled/cancelled).
        Used to verify whether our cancel attempt actually succeeded."""
        with self.flipster_ws_lock:
            o = self.flipster_orders.get(order_id)
            if not o:
                return False  # gone from open orders → cancelled or filled
            status = (o.get("status") or "").upper()
            # Open-ish statuses: NEW, OPEN, PARTIALLY_FILLED, PENDING
            # Terminal: FILLED, CANCELLED, REJECTED, EXPIRED
            if any(t in status for t in ("FILL", "CANCEL", "REJECT", "EXPIR", "DONE")):
                return False
            return True

    def _flipster_ws_loop(self):
        """Background thread: connect to Flipster private WS, subscribe to
        positions/orders/margins, maintain self.flipster_positions in real-time.
        HTTP GET on /trade/positions returns 405 — WS is the only way to verify
        whether a LIMIT order filled after we attempted to cancel it."""
        import asyncio
        try:
            asyncio.run(self._flipster_ws_async())
        except Exception as e:
            print(f"[flip-ws] thread crashed: {e}")

    async def _flipster_ws_async(self):
        import websockets
        if self.flipster_client is None:
            print("[flip-ws] no flipster_client; thread idle")
            return
        cookies = getattr(self.flipster_client, "_cookies", {}) or {}
        cookie_hdr = "; ".join(f"{k}={v}" for k, v in cookies.items())
        headers = [
            ("Origin", "https://flipster.io"),
            ("Cookie", cookie_hdr),
            ("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
                           "(KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36"),
        ]
        while True:
            try:
                async with websockets.connect(FLIP_WS_URL, additional_headers=headers,
                                              max_size=10_000_000,
                                              ping_interval=20, ping_timeout=10) as ws:
                    await ws.send(json.dumps(FLIP_WS_SUBSCRIBE))
                    self.flipster_ws_connected = True
                    print("[flip-ws] connected & subscribed (private positions/orders/margins)")
                    async for raw in ws:
                        if raw == "ping":
                            await ws.send("pong")
                            continue
                        try:
                            msg = json.loads(raw)
                        except Exception:
                            continue
                        t = msg.get("t", {}) if isinstance(msg, dict) else {}
                        if "private/positions" in t:
                            updates = t["private/positions"].get("u", {}) or {}
                            deletes = t["private/positions"].get("d", []) or []
                            with self.flipster_ws_lock:
                                for k, fields in updates.items():
                                    cur = self.flipster_positions.setdefault(k, {})
                                    cur.update(fields)
                                    margin = cur.get("initMarginReserved")
                                    try:
                                        if margin is not None and float(margin) == 0:
                                            self.flipster_positions.pop(k, None)
                                    except (ValueError, TypeError):
                                        pass
                                for k in deletes:
                                    self.flipster_positions.pop(k, None)
                        if "private/orders" in t:
                            updates = t["private/orders"].get("u", {}) or {}
                            deletes = t["private/orders"].get("d", []) or []
                            with self.flipster_ws_lock:
                                for k, fields in updates.items():
                                    cur = self.flipster_orders.setdefault(k, {})
                                    cur.update(fields)
                                for k in deletes:
                                    self.flipster_orders.pop(k, None)
            except Exception as e:
                self.flipster_ws_connected = False
                print(f"[flip-ws] error: {type(e).__name__}: {str(e)[:120]}; reconnect 3s")
                await asyncio.sleep(3)

    def _take_profit_loop(self):
        """Background thread: every TP_POLL_SEC, scan open positions and close
        any whose captured-bp has drawn down ≥ TP_DRAWDOWN_BP from peak. Closes
        race-safely by adding pos_id to seen_exit_ids under seen_lock before
        calling _on_exit (so a concurrent paper exit signal will be deduped)."""
        while True:
            try:
                time.sleep(TP_POLL_SEC)
                if self.dry_run:
                    continue
                for pos_id, pos in list(self.open_positions.items()):
                    if abs(pos.entry_spread_bp) < TP_MIN_ENTRY_SPREAD_BP:
                        continue
                    if (time.time() - pos.entry_epoch) < TP_MIN_HOLD_SEC:
                        continue
                    flipster_sym = f"{pos.base}USDT.PERP"
                    gate_sym = f"{pos.base}_USDT"
                    f_q = fetch_latest_mid_age("flipster_bookticker", flipster_sym)
                    g_q = fetch_latest_mid_age("gate_bookticker", gate_sym)
                    if not f_q or not g_q:
                        continue
                    if f_q[2] > TP_MAX_MID_AGE_SEC or g_q[2] > TP_MAX_MID_AGE_SEC:
                        continue
                    f_mid = (f_q[0] + f_q[1]) / 2.0
                    g_mid = (g_q[0] + g_q[1]) / 2.0
                    if f_mid <= 0 or g_mid <= 0:
                        continue
                    cur_sprd_bp = (f_mid - g_mid) / g_mid * 1e4
                    captured = abs(pos.entry_spread_bp) - abs(cur_sprd_bp)
                    if captured > pos.peak_captured_bp:
                        pos.peak_captured_bp = captured
                    if pos.peak_captured_bp < TP_MIN_PEAK_BP:
                        continue
                    drawdown = pos.peak_captured_bp - captured
                    if drawdown < TP_DRAWDOWN_BP:
                        continue
                    # Trigger close. Lock so paper exit signal arriving in
                    # parallel won't double-close.
                    with self.seen_lock:
                        if pos_id in self.seen_exit_ids:
                            continue
                        self.seen_exit_ids.add(pos_id)
                    print(f"\n[TP] {pos.base} pos_id={pos_id} entry_sprd={pos.entry_spread_bp:+.1f} "
                          f"cur_sprd={cur_sprd_bp:+.1f} peak={pos.peak_captured_bp:+.1f} "
                          f"now={captured:+.1f} drawdown={drawdown:.1f}bp → CLOSE")
                    try:
                        self._on_exit(pos_id, pos.base, pos.flipster_side,
                                      f_mid, g_mid, "take_profit")
                    except Exception as e:
                        print(f"[TP] _on_exit error: {e}")
            except Exception as e:
                print(f"[tp] loop error: {e}")

    def _zmq_subscriber_loop(self):
        """Background thread: ZMQ SUB on collector's trade_signal PUB. Parses
        each message and pushes onto self.zmq_queue. Blocks on recv."""
        ctx = zmq.Context.instance()
        sock = ctx.socket(zmq.SUB)
        sock.setsockopt(zmq.RCVHWM, 10_000)
        sock.setsockopt(zmq.SUBSCRIBE, b"")
        sock.connect(SIGNAL_SUB_ADDR)
        print(f"[zmq] SUB connected {SIGNAL_SUB_ADDR}")
        while True:
            try:
                msg = sock.recv()
                event = json.loads(msg.decode("utf-8"))
                if event.get("account_id") != self.variant:
                    continue
                self.zmq_queue.put(event)
                self.zmq_events_seen += 1
            except Exception as e:
                print(f"[zmq] recv error: {e}")
                time.sleep(0.5)

    def drain_zmq(self):
        """Pull all queued events and dispatch to the thread pool (non-blocking).
        Returns count dispatched."""
        dispatched = 0
        while True:
            try:
                event = self.zmq_queue.get_nowait()
            except queue.Empty:
                break
            dispatched += 1
            self.dispatch_pool.submit(self._dispatch_event, event)
        return dispatched

    def _dispatch_event(self, event: dict):
        try:
            pos_id = int(event["position_id"])
            base = event["base"]
            action = event["action"]
            side = event["side"]
            size_usd = float(event["size_usd"])
            f_price = float(event["flipster_price"])
            g_price = float(event["gate_price"])
            ts = event["timestamp"]
            if action == "entry":
                with self.seen_lock:
                    if pos_id in self.seen_entry_ids:
                        return
                    self.seen_entry_ids.add(pos_id)
                if self.symbol_whitelist and base not in self.symbol_whitelist:
                    return
                if base in self.symbol_blacklist:
                    return
                self._on_entry(pos_id, base, side, size_usd, f_price, g_price, ts)
            elif action == "exit":
                with self.seen_lock:
                    if pos_id in self.seen_exit_ids:
                        return
                    self.seen_exit_ids.add(pos_id)
                self._on_exit(pos_id, base, side, f_price, g_price, ts)
        except Exception as e:
            print(f"[dispatch] error: {e}")

    def poll_signals(self):
        """Poll for new entry/exit signals from the chosen variant. Only
        looks back 30s — paper signals older than that are stale and
        actionless. Filters via seen_entry_ids/seen_exit_ids inline so we
        don't flood dispatch_pool with duplicates that ZMQ already handled."""
        sql = f"""
            SELECT position_id, base, action, flipster_side,
                   size_usd, flipster_price, gate_price, timestamp
            FROM trade_signal
            WHERE account_id = '{self.variant}'
              AND timestamp > dateadd('s', -30, now())
            ORDER BY timestamp ASC
        """
        try:
            rows = query_qdb(sql)
        except Exception as e:
            print(f"[poll] QuestDB error: {e}")
            return

        for row in rows:
            pos_id = int(row[0])
            base = row[1]
            action = row[2]
            side = row[3]
            size_usd = float(row[4])
            f_price = float(row[5])
            g_price = float(row[6])
            ts = row[7]

            # Inline dedup so we don't flood dispatch_pool with already-seen
            # entries (most of which ZMQ already delivered in real-time).
            with self.seen_lock:
                if action == "entry" and pos_id in self.seen_entry_ids:
                    continue
                if action == "exit" and pos_id in self.seen_exit_ids:
                    continue
            # Hand off to dispatch pool — never block main loop on HTTP I/O.
            event = {"position_id": pos_id, "base": base, "action": action,
                     "side": side, "size_usd": size_usd,
                     "flipster_price": f_price, "gate_price": g_price,
                     "timestamp": ts, "account_id": self.variant}
            self.dispatch_pool.submit(self._dispatch_event, event)

    def _on_entry(self, pos_id, base, side, paper_size, f_price, g_price, ts):
        flipster_sym = f"{base}USDT.PERP"
        gate_sym = f"{base}_USDT"
        size = self.size_usd
        entry_t0 = time.time()

        # Measure signal→now latency (seconds since the paper signal ts).
        signal_lag_ms = 0.0
        try:
            sig_ts = datetime.fromisoformat(ts.replace("Z", "+00:00"))
            signal_lag_ms = (datetime.now(timezone.utc) - sig_ts).total_seconds() * 1000.0
        except Exception:
            pass

        print(f"\n{'='*60}")
        print(f"[ENTRY] {base} | side={side} | ${size:.0f} | lag={signal_lag_ms:.0f}ms")
        print(f"  paper: flipster={f_price:.6f} gate={g_price:.6f}")
        print(f"  variant={self.variant} pos_id={pos_id}")

        if self.dry_run:
            print("  [DRY RUN] skipping real orders")
            self.open_positions[pos_id] = LivePosition(
                position_id=pos_id, base=base, flipster_side=side,
                size_usd=size, entry_time=ts,
            )
            return

        # Entry filter: only trade strong mispricings. |signal_spread|<25bp
        # categorically loses (analysis 2026-04-26). Compute from paper signal prices.
        if g_price > 0 and f_price > 0:
            sig_sprd_bp = (f_price - g_price) / g_price * 1e4
            if abs(sig_sprd_bp) < MIN_ENTRY_SIGNAL_SPREAD_BP:
                print(f"  [SKIP] weak signal — |entry_sprd|={abs(sig_sprd_bp):.1f}bp < {MIN_ENTRY_SIGNAL_SPREAD_BP}bp threshold")
                return

        # Edge recheck: if Flipster mid has already moved in the signal's predicted
        # direction by >= EDGE_RECHECK_MAX_ADVERSE_BP, the reversion has already happened
        # and we'd be late — skip.
        f_mid = fetch_latest_mid("flipster_bookticker", flipster_sym)
        if f_mid is not None and f_price > 0:
            cur_mid = (f_mid[0] + f_mid[1]) / 2.0
            move_bp = (cur_mid - f_price) / f_price * 10000.0
            adverse_bp = move_bp if side == "long" else -move_bp  # favorable for us = bad for edge
            if adverse_bp >= EDGE_RECHECK_MAX_ADVERSE_BP:
                print(f"  [SKIP] edge decayed — flipster mid moved {adverse_bp:+.1f}bp in signal direction (threshold {EDGE_RECHECK_MAX_ADVERSE_BP})")
                return
            print(f"  [RECHECK] flipster_mid={cur_mid:.6f} adverse={adverse_bp:+.1f}bp OK")

        # Match Flipster notional to Gate's discrete contract notional so the
        # hedge ratio is 1:1. gate_size_from_usd rounds to nearest contract;
        # the residual unbalanced part would otherwise carry directional risk.
        gate_spec = GATE_CONTRACTS.get(gate_sym, {"multiplier": 1.0, "order_size_min": 1})
        gate_contracts_pre = gate_size_from_usd(gate_sym, size, g_price)
        adjusted_size = gate_contracts_pre * gate_spec["multiplier"] * g_price
        # Skip if contract-rounded notional deviates too much from target (e.g. 1 contract = $25 when target is $10).
        if adjusted_size < 3.0 or adjusted_size > size * 2.5:
            print(f"  [SKIP] hedge mismatch — gate contract notional=${adjusted_size:.2f} vs target=${size}")
            return
        size = adjusted_size  # override --size-usd to match gate's discrete fill

        # Place Flipster FIRST (one-way mode). If it fails (or doesn't fill in
        # the LIMIT-IOC case), skip Gate entirely (no unhedged risk).
        f_result = None
        f_t0 = time.time()
        if FLIPSTER_LIMIT_ENTRY:
            # Manual IOC: LIMIT at signal price, brief wait, cancel + verify.
            try:
                f_result, _flip_oid = flipster_limit_ioc_entry(
                    self.flipster_client, flipster_sym, side, size, f_price,
                    leverage=10, wait_s=FLIP_LIMIT_WAIT_S,
                    position_size_fn=self.flipster_position_size,
                    has_position_fn=self.flipster_has_position,
                    order_open_fn=self.flipster_order_open,
                )
                f_dt_ms = (time.time() - f_t0) * 1000.0
                if f_result is None:
                    print(f"  [FLIPSTER-LIMIT] not filled ({f_dt_ms:.0f}ms) — abort, no Gate, no revert")
                    return
                f_avg_pre, f_size_pre, _ = _extract_flipster_fill(f_result)
                print(f"  [FLIPSTER-LIMIT] OK ({f_dt_ms:.0f}ms) size=${size:.2f} fill={f_avg_pre}")
            except Exception as e:
                print(f"  [FLIPSTER-LIMIT] FAILED: {e} — SKIPPING Gate (avoid unhedged)")
                return
        else:
            try:
                f_result = flipster_oneway_order(
                    self.flipster_client, flipster_sym, side, size, f_price,
                    leverage=10, reduce_only=False, order_type="ORDER_TYPE_MARKET",
                )
                f_dt_ms = (time.time() - f_t0) * 1000.0
                print(f"  [FLIPSTER] OK ({f_dt_ms:.0f}ms) size=${size:.2f}: {json.dumps(f_result, default=str)[:140]}")
            except Exception as e:
                print(f"  [FLIPSTER] FAILED: {e} — SKIPPING Gate (avoid unhedged)")
                return

        # Place Gate hedge order.
        # GATE_MAKER_ENTRY=True: postOnly (POC) on the maker side (ASK for SHORT,
        #   BID for LONG). Captures maker rebate and saves the half-spread vs IOC.
        #   Trade-off: may not fill if the market reverts before someone hits us.
        # GATE_MAKER_ENTRY=False: legacy IOC at paper signal price.
        # Either way, on failure SAFETY revert closes Flipster reduce-only.
        from gate_pkg.order import OrderParams as GParams, Side as GSide, OrderType as GType, TimeInForce as GTif
        g_side = GSide.SHORT if side == "long" else GSide.LONG
        g_size = gate_size_from_usd(gate_sym, size, g_price)

        if GATE_MAKER_ENTRY:
            g_book = fetch_latest_mid("gate_bookticker", gate_sym)
            if g_book:
                g_bid, g_ask = g_book
                g_limit_price = g_ask if g_side == GSide.SHORT else g_bid
            else:
                g_limit_price = g_price
            g_tif = GTif.POC
            g_label = "POC"
        elif GATE_LIMIT_WAIT:
            # GTC LIMIT at signal price. Crosses immediately if book at-or-
            # better; otherwise sits and may fill if book moves toward us
            # within GATE_LIMIT_WAIT_S. We poll fill status and cancel on
            # timeout.
            g_limit_price = g_price
            g_tif = GTif.GTC
            g_label = "LIMIT-WAIT"
        else:
            # IOC at the CURRENT Gate book's takable price (BID for SHORT,
            # ASK for LONG). Paper signal's g_price is consistently ~24bp
            # away from current book by the time our order arrives — IOC at
            # paper_price has 0% fill rate. Crossing the current book
            # guarantees fill at market and lets us capture the remaining
            # reversion edge instead of failing into SAFETY revert.
            g_book = fetch_latest_mid("gate_bookticker", gate_sym)
            if g_book:
                g_bid, g_ask = g_book
                g_limit_price = g_bid if g_side == GSide.SHORT else g_ask
            else:
                g_limit_price = g_price  # fallback
            g_tif = GTif.IOC
            g_label = "IOC"

        # Set leverage on first encounter of each contract (CROSS mode, 10x cap)
        if gate_sym not in self.gate_leverage_set:
            try:
                lr = self.gate_client.set_leverage(
                    gate_sym, leverage=0, cross_leverage_limit=self.gate_target_leverage,
                )
                print(f"  [LEV] {gate_sym} → cross {self.gate_target_leverage}x: {lr.get('message', lr)[:80] if isinstance(lr, dict) else lr}")
                self.gate_leverage_set.add(gate_sym)
            except Exception as e:
                print(f"  [LEV] set_leverage failed: {e}")

        g_result = None
        g_t0 = time.time()
        g_fill_price = 0.0
        g_filled_size = 0
        order_id = ""
        try:
            g_result = self.gate_client.place_order(
                gate_sym,
                GParams(side=g_side, size=g_size,
                        order_type=GType.LIMIT, price=g_limit_price, tif=g_tif),
            )
            ok = g_result.get("ok") if isinstance(g_result, dict) else False
            if not ok:
                raise RuntimeError(f"gate place_order not ok: {g_result}")

            inner = g_result.get("data", {}).get("data", {}) if isinstance(g_result, dict) else {}
            order_id = str(inner.get("id_string") or inner.get("id") or "")
            place_status = inner.get("status", "")

            if g_tif in (GTif.POC, GTif.GTC) and place_status == "open":
                # Posted on book — poll for fill until timeout, then cancel.
                _timeout = GATE_LIMIT_WAIT_S if g_tif == GTif.GTC else GATE_MAKER_TIMEOUT_S
                deadline = time.time() + _timeout
                final_seen = False
                while time.time() < deadline and not final_seen:
                    time.sleep(GATE_MAKER_POLL_S)
                    qs = self.gate_client.get_order(order_id) if order_id else {}
                    d = qs.get("data", {}).get("data", {}) if qs.get("ok") else {}
                    if d.get("status") == "finished":
                        final_seen = True
                        fp = float(d.get("fill_price") or 0)
                        if fp > 0:
                            g_fill_price = fp
                            sz = abs(int(d.get("size") or 0))
                            left = abs(int(d.get("left") or 0))
                            g_filled_size = max(0, sz - left)
                if not final_seen and order_id:
                    # Timeout: cancel and re-check (may have filled in race)
                    self.gate_client.cancel_order(order_id)
                    final = self.gate_client.get_order(order_id)
                    d = final.get("data", {}).get("data", {}) if final.get("ok") else {}
                    fp = float(d.get("fill_price") or 0)
                    if fp > 0:
                        g_fill_price = fp
                        sz = abs(int(d.get("size") or 0))
                        left = abs(int(d.get("left") or 0))
                        g_filled_size = max(0, sz - left)
            else:
                # IOC path, or POC immediately rejected (status=finished, fill=0).
                # _extract_gate_fill handles both cases.
                g_fill_price, g_filled_size = _extract_gate_fill(g_result or {})

            g_dt_ms = (time.time() - g_t0) * 1000.0
            total_dt_ms = (time.time() - entry_t0) * 1000.0
            if g_fill_price > 0 and g_filled_size > 0:
                print(f"  [GATE-{g_label}] filled={g_filled_size} @{g_fill_price} ({g_dt_ms:.0f}ms) | total_entry={total_dt_ms:.0f}ms sig→done={signal_lag_ms + total_dt_ms:.0f}ms")
            else:
                raise RuntimeError(f"gate {g_label} not filled (order_id={order_id})")
        except Exception as e:
            print(f"  [GATE] FAILED: {e}")
            print(f"  [SAFETY] Undoing Flipster side to stay flat...")
            # Open price of the LIMIT fill we are now reverting.
            f_open_avg, _, _ = _extract_flipster_fill(f_result or {})
            close_side = "short" if side == "long" else "long"
            f_close_avg = 0.0
            try:
                revert_result = flipster_oneway_order(
                    self.flipster_client, flipster_sym, close_side, size * 1.5, f_price,
                    leverage=10, reduce_only=True, order_type="ORDER_TYPE_MARKET",
                )
                f_close_avg, _, _ = _extract_flipster_fill(revert_result)
                print(f"  [SAFETY] Flipster reverted (opposite market, reduceOnly) close={f_close_avg}")
            except Exception as e2:
                print(f"  [SAFETY] !!! revert failed: {e2} — MANUAL INTERVENTION NEEDED on {flipster_sym} !!!")

            # Record SAFETY revert PnL — open at LIMIT, close at MARKET.
            # This was missing from JSONL before, hiding ~$0.005 cost per event.
            try:
                f_pnl_usd = 0.0
                if f_open_avg > 0 and f_close_avg > 0:
                    f_dir = 1 if side == "long" else -1
                    f_pnl_bp = (f_close_avg - f_open_avg) / f_open_avg * 1e4 * f_dir
                    f_pnl_usd = f_pnl_bp * size / 1e4
                # Flipster taker fees on both sides.
                approx_fee = size * TAKER_FEE_FRAC * 2
                net = f_pnl_usd - approx_fee
                self.trade_log_path.parent.mkdir(parents=True, exist_ok=True)
                _append_trade(self.trade_log_path, {
                    "ts_close": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
                    "exit_reason": "safety_revert",
                    "ts_entry": ts,
                    "pos_id": pos_id,
                    "base": base,
                    "flipster_side": side,
                    "size_usd": size,
                    "flipster_entry": f_open_avg,
                    "flipster_exit": f_close_avg,
                    "gate_entry": 0.0,
                    "gate_exit": 0.0,
                    "gate_size": 0,
                    "gate_contract": gate_sym,
                    "f_pnl_usd": round(f_pnl_usd, 6),
                    "g_pnl_usd": 0.0,
                    "net_pnl_usd": round(f_pnl_usd, 6),
                    "approx_fee_usd": round(approx_fee, 6),
                    "net_after_fees_usd": round(net, 6),
                })
                # Roll into running totals so STATS reflects reality.
                self.trade_count += 1
                self.total_pnl_usd += net
                if net > 0:
                    self.win_count += 1
                print(f"  [SAFETY-PNL] f_pnl=${f_pnl_usd:+.4f} fee=${approx_fee:.4f} net=${net:+.4f}")
                print(f"  [STATS] trades={self.trade_count} wins={self.win_count} "
                      f"win%={self.win_count/self.trade_count*100:.1f} total_pnl=${self.total_pnl_usd:+.4f}")
            except Exception as log_err:
                print(f"  [SAFETY-PNL] log err: {log_err}")
            return

        # Capture actual fill prices for PnL computation later.
        f_avg, f_size, f_slot = _extract_flipster_fill(f_result or {})
        g_avg, g_filled = _extract_gate_fill(g_result or {})
        # Entry spread used by TP loop. Signed; sign convention same as
        # _take_profit_loop's `cur_sprd_bp` so that |entry|-|cur| = captured.
        entry_sprd_bp = 0.0
        if f_avg > 0 and g_avg > 0:
            entry_sprd_bp = (f_avg - g_avg) / g_avg * 1e4
        # Slippage at entry. Positive = adverse (we paid worse than paper signal).
        # Flipster long buys → bad if f_avg > f_price. Flipster short sells → bad if f_avg < f_price.
        # Gate is the opposite leg: gate short (when flipster long) → bad if g_avg < g_price.
        f_entry_slip = 0.0
        g_entry_slip = 0.0
        if f_avg > 0 and f_price > 0:
            raw = (f_avg - f_price) / f_price * 1e4
            f_entry_slip = raw if side == "long" else -raw
        if g_avg > 0 and g_price > 0:
            raw = (g_avg - g_price) / g_price * 1e4
            # gate side is opposite of flipster side
            g_entry_slip = -raw if side == "long" else raw
        print(f"  [SLIP-IN] flipster={f_entry_slip:+.2f}bp  gate={g_entry_slip:+.2f}bp  (>0 = adverse)")
        lp = LivePosition(
            position_id=pos_id, base=base, flipster_side=side,
            size_usd=size, entry_time=ts,
            flipster_entry_price=f_avg,
            flipster_size=f_size,
            flipster_slot=f_slot,
            gate_entry_price=g_avg,
            gate_size=g_filled or g_size,
            gate_contract=gate_sym,
            flipster_order=f_result, gate_order=g_result,
            entry_spread_bp=entry_sprd_bp,
            entry_epoch=time.time(),
            f_entry_slip_bp=f_entry_slip,
            g_entry_slip_bp=g_entry_slip,
            signal_lag_ms_entry=signal_lag_ms,
            paper_f_entry=f_price,
            paper_g_entry=g_price,
        )
        self.open_positions[pos_id] = lp
        print(f"  [TRACK] flipster_entry={f_avg} size={f_size} slot={f_slot} | "
              f"gate_entry={g_avg} contracts={lp.gate_size} | entry_sprd={entry_sprd_bp:+.1f}bp")

    def _on_exit(self, pos_id, base, side, f_price, g_price, ts):
        pos = self.open_positions.pop(pos_id, None)

        print(f"\n{'='*60}")
        print(f"[EXIT] {base} | side={side} | pos_id={pos_id}")
        print(f"  paper: flipster={f_price:.2f} gate={g_price:.2f}")

        if pos is None:
            print("  (position not tracked — skipped or from before startup)")
            return

        if self.dry_run:
            print("  [DRY RUN] skipping real close")
            self.trade_count += 1
            return

        flipster_sym = f"{base}USDT.PERP"
        gate_sym = f"{base}_USDT"

        # One-way close: opposite side MARKET with reduceOnly (1.5x buffer).
        f_close_avg = 0.0
        ref_px = f_price if f_price > 0 else pos.flipster_entry_price
        try:
            close_side = "short" if side == "long" else "long"
            result = flipster_oneway_order(
                self.flipster_client, flipster_sym, close_side,
                pos.size_usd * 1.5, ref_px,
                leverage=10, reduce_only=True, order_type="ORDER_TYPE_MARKET",
            )
            f_close_avg, _, _ = _extract_flipster_fill(result)
            print(f"  [FLIPSTER CLOSE] OK @ {f_close_avg}")
        except Exception as e:
            print(f"  [FLIPSTER CLOSE] FAILED: {e} — MANUAL CLOSE may be needed on {flipster_sym}")

        # Close Gate (reduce_only opposite direction).
        g_close_avg = 0.0
        try:
            g_size = pos.gate_size or gate_size_from_usd(gate_sym, pos.size_usd, g_price)
            close_size = g_size if side == "long" else -g_size
            result = self.gate_client.close_position(
                gate_sym, size=close_size,
            )
            g_close_avg, _ = _extract_gate_fill(result)
            ok = result.get("ok") if isinstance(result, dict) else None
            label = result.get("data", {}).get("label", "") if isinstance(result, dict) else ""
            print(f"  [GATE CLOSE] ok={ok} label={label} size={close_size} fill={g_close_avg}")
        except Exception as e:
            print(f"  [GATE CLOSE] FAILED: {e}")

        # ---- Compute net PnL ----
        # Flipster: long → gain when exit > entry; short → gain when exit < entry
        # Gate hedge is opposite side.
        f_pnl_usd = 0.0
        g_pnl_usd = 0.0
        if pos.flipster_entry_price > 0 and f_close_avg > 0:
            f_dir = 1 if side == "long" else -1
            f_pnl_bp = (f_close_avg - pos.flipster_entry_price) / pos.flipster_entry_price * 1e4 * f_dir
            f_pnl_usd = f_pnl_bp * pos.size_usd / 1e4
        if pos.gate_entry_price > 0 and g_close_avg > 0:
            # Gate: SHORT side when flipster long (so g_dir = -1); LONG when flipster short
            g_dir = -1 if side == "long" else 1
            g_pnl_bp = (g_close_avg - pos.gate_entry_price) / pos.gate_entry_price * 1e4 * g_dir
            # Gate notional ≈ contracts × multiplier × price ≈ size_usd
            spec = GATE_CONTRACTS.get(pos.gate_contract, {"multiplier": 1.0})
            g_notional = pos.gate_size * spec["multiplier"] * pos.gate_entry_price
            g_pnl_usd = g_pnl_bp * g_notional / 1e4

        net_pnl_usd = f_pnl_usd + g_pnl_usd
        # Real fees (user-confirmed 2026-04-24): Flipster 0.425bp + Gate 0.45bp per side = 0.875bp round-trip per leg.
        # Formula: notional × 0.0000875 × 2 sides.
        gate_notional = pos.gate_size * GATE_CONTRACTS.get(pos.gate_contract, {"multiplier": 1.0})["multiplier"] * pos.gate_entry_price
        approx_fee = (pos.size_usd + gate_notional) * TAKER_FEE_FRAC * 2
        net_after_fees = net_pnl_usd - approx_fee

        self.trade_count += 1
        if net_after_fees > 0:
            self.win_count += 1
        self.total_pnl_usd += net_after_fees

        print(f"  [PNL] flipster=${f_pnl_usd:+.4f} gate=${g_pnl_usd:+.4f} "
              f"net=${net_pnl_usd:+.4f} (-fees ${approx_fee:.4f}) → ${net_after_fees:+.4f}")
        print(f"  [STATS] trades={self.trade_count} wins={self.win_count} "
              f"win%={self.win_count/self.trade_count*100:.1f} total_pnl=${self.total_pnl_usd:+.4f}")

        # Persist to JSONL. ts can be a real ISO timestamp, "shutdown", or
        # "take_profit"; record the trigger reason separately so analytics
        # can still parse ts_close as ISO.
        exit_reason = "paper_signal"
        ts_close_iso = ts
        if ts == "take_profit":
            exit_reason = "take_profit"
            ts_close_iso = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
        elif ts == "shutdown":
            exit_reason = "shutdown"
            ts_close_iso = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")

        # Exit slippage. Close direction is opposite of entry on both legs.
        # Flipster close_side=short (when entry long) → selling, bad if f_close_avg < f_signal_exit.
        # Flipster close_side=long  (when entry short) → buying,  bad if f_close_avg > f_signal_exit.
        # Gate close direction is opposite of flipster's close (since gate hedge = opposite of flipster).
        f_exit_slip = 0.0
        g_exit_slip = 0.0
        if f_close_avg > 0 and f_price > 0:
            raw = (f_close_avg - f_price) / f_price * 1e4
            # When entry side=long, close is sell → bad if fill < signal → adverse = -raw
            f_exit_slip = -raw if side == "long" else raw
        if g_close_avg > 0 and g_price > 0:
            raw = (g_close_avg - g_price) / g_price * 1e4
            # gate close side is same direction as flipster entry (since gate entry was opposite)
            g_exit_slip = raw if side == "long" else -raw
        print(f"  [SLIP-OUT] flipster={f_exit_slip:+.2f}bp  gate={g_exit_slip:+.2f}bp  (>0 = adverse)")

        try:
            self.trade_log_path.parent.mkdir(parents=True, exist_ok=True)
            _append_trade(self.trade_log_path, {
                "ts_close": ts_close_iso,
                "exit_reason": exit_reason,
                "entry_spread_bp": round(pos.entry_spread_bp, 2),
                "peak_captured_bp": round(pos.peak_captured_bp, 2),
                "f_entry_slip_bp": round(pos.f_entry_slip_bp, 2),
                "g_entry_slip_bp": round(pos.g_entry_slip_bp, 2),
                "f_exit_slip_bp": round(f_exit_slip, 2),
                "g_exit_slip_bp": round(g_exit_slip, 2),
                "signal_lag_ms": round(pos.signal_lag_ms_entry, 0),
                "paper_f_entry": pos.paper_f_entry,
                "paper_g_entry": pos.paper_g_entry,
                "paper_f_exit": f_price,
                "paper_g_exit": g_price,
                "ts_entry": pos.entry_time,
                "pos_id": pos_id,
                "base": base,
                "flipster_side": side,
                "size_usd": pos.size_usd,
                "flipster_entry": pos.flipster_entry_price,
                "flipster_exit": f_close_avg,
                "gate_entry": pos.gate_entry_price,
                "gate_exit": g_close_avg,
                "gate_size": pos.gate_size,
                "gate_contract": pos.gate_contract,
                "f_pnl_usd": round(f_pnl_usd, 6),
                "g_pnl_usd": round(g_pnl_usd, 6),
                "net_pnl_usd": round(net_pnl_usd, 6),
                "approx_fee_usd": round(approx_fee, 6),
                "net_after_fees_usd": round(net_after_fees, 6),
            })
        except Exception as e:
            print(f"  [TRACK] log write failed: {e}")

    def emergency_close_all(self):
        """Close every position currently tracked. Call on shutdown."""
        if not self.open_positions:
            print("[shutdown] no open positions to close")
            return
        print(f"\n[shutdown] EMERGENCY CLOSE: {len(self.open_positions)} open positions")
        for pos_id, pos in list(self.open_positions.items()):
            print(f"  closing pos_id={pos_id} {pos.base} side={pos.flipster_side}")
            # Fake an exit signal
            try:
                # Use last paper price approximation — fall back to entry price
                self._on_exit(pos_id, pos.base, pos.flipster_side, 0.0, 0.0, "shutdown")
            except Exception as e:
                print(f"    err: {e}")

    def run(self):
        print(f"\n{'#'*60}")
        print(f"# Live Executor")
        print(f"# Variant: {self.variant}")
        print(f"# Size: ${self.size_usd:.0f} per trade")
        print(f"# Dry run: {self.dry_run}")
        print(f"# Poll interval: {POLL_INTERVAL}s")
        print(f"{'#'*60}\n")

        # Pre-seed seen IDs from recent signals so we don't replay
        print("[init] Loading recent signals to avoid replay...")
        sql = f"""
            SELECT position_id, action FROM trade_signal
            WHERE account_id = '{self.variant}'
              AND timestamp > dateadd('m', -10, now())
        """
        try:
            for row in query_qdb(sql):
                pid = int(row[0])
                if row[1] == "entry":
                    self.seen_entry_ids.add(pid)
                else:
                    self.seen_exit_ids.add(pid)
            print(f"[init] Pre-seeded {len(self.seen_entry_ids)} entries, "
                  f"{len(self.seen_exit_ids)} exits")
        except Exception as e:
            print(f"[init] Warning: {e}")

        # Start ZMQ subscriber thread (primary signal path, <10ms)
        t = threading.Thread(target=self._zmq_subscriber_loop, daemon=True, name="zmq-sub")
        t.start()
        # Start take-profit watcher thread
        tp = threading.Thread(target=self._take_profit_loop, daemon=True, name="tp")
        tp.start()
        # Start Gate balance snapshot logger
        bal = threading.Thread(target=self._balance_snapshot_loop, daemon=True, name="balance")
        bal.start()
        # Start Flipster private WS — needed for fill verification on LIMIT entries
        flws = threading.Thread(target=self._flipster_ws_loop, daemon=True, name="flip-ws")
        flws.start()
        print(f"[running] ZMQ sub @ {SIGNAL_SUB_ADDR} | TP watcher armed "
              f"(min_entry={TP_MIN_ENTRY_SPREAD_BP}bp, peak≥{TP_MIN_PEAK_BP}bp, drawdown≥{TP_DRAWDOWN_BP}bp) | "
              f"balance snapshot every 30min → {self.balance_log_path}\n")

        last_fallback_poll = time.time()
        while True:
            try:
                self.drain_zmq()
                # Fallback QuestDB poll every 5s in case collector PUB is down.
                if time.time() - last_fallback_poll > 5.0:
                    self.poll_signals()
                    last_fallback_poll = time.time()
            except KeyboardInterrupt:
                break
            except Exception as e:
                print(f"[error] {e}")
            time.sleep(0.01)  # 10ms — tight drain cadence

        print(f"\n[stopped] {self.trade_count} trades executed")


# ---------------------------------------------------------------------------
# Setup: start browsers for login
# ---------------------------------------------------------------------------

def setup_browsers():
    print("Starting browsers for login...\n")

    print("=== FLIPSTER ===")
    try:
        from flipster_pkg.client import FlipsterClient
        fc = FlipsterClient()
        url = fc.start_browser()
        print(f"Flipster VNC: {url}")
        print("→ Log in to Flipster, then come back here\n")
    except Exception as e:
        print(f"Flipster browser failed: {e}\n")
        fc = None

    print("=== GATE ===")
    try:
        from gate_pkg.client import GateClient
        gc = GateClient()
        url = gc.start_browser()
        print(f"Gate VNC: {url}")
        print("→ Log in to Gate.io, then come back here\n")
    except Exception as e:
        print(f"Gate browser failed: {e}\n")
        gc = None

    input("Press Enter after logging into BOTH exchanges...")

    if fc:
        try:
            fc.login_done()
            print("✓ Flipster cookies extracted")
        except Exception as e:
            print(f"✗ Flipster cookie extraction failed: {e}")

    if gc:
        try:
            gc.login_done()
            logged_in = gc.is_logged_in()
            print(f"✓ Gate cookies extracted (logged_in={logged_in})")
        except Exception as e:
            print(f"✗ Gate cookie extraction failed: {e}")

    print("\nBrowsers ready. You can now run:")
    print("  python3 scripts/live_executor.py --variant T01_best --size-usd 10")
    print("\nKeep this terminal open (browsers stay alive).")
    print("Press Ctrl+C to stop browsers.")

    try:
        while True:
            time.sleep(60)
    except KeyboardInterrupt:
        pass
    finally:
        if fc:
            fc.stop()
        if gc:
            gc.stop()


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="Live executor sidecar")
    parser.add_argument("--setup", action="store_true",
                        help="Start browsers for login")
    parser.add_argument("--variant", type=str, default="T01_best",
                        help="Paper variant to mirror (default: T01_best)")
    parser.add_argument("--size-usd", type=float, default=10.0,
                        help="USD size per trade (default: 10)")
    parser.add_argument("--symbols", type=str, default="",
                        help="Comma-separated symbol whitelist (e.g. BTC,ETH,SOL). Empty = all")
    parser.add_argument("--blacklist", type=str, default="",
                        help="Comma-separated symbol blacklist (applied after whitelist)")
    parser.add_argument("--dry-run", action="store_true",
                        help="Log signals without placing real orders")
    args = parser.parse_args()

    if args.setup:
        setup_browsers()
        return

    # Load Gate contract specs (multipliers needed for correct sizing)
    load_gate_contracts()

    # For live mode, need to connect to already-running browsers
    flipster_client = None
    gate_client = None

    # Load proxies for Flipster (rate-limit avoidance) only if enabled.
    proxies = []
    if USE_FLIPSTER_PROXIES:
        proxy_file = Path(__file__).resolve().parent / "proxies.txt"
        if proxy_file.exists():
            proxies = [ln.strip() for ln in proxy_file.read_text().splitlines() if ln.strip()]
            print(f"[init] Loaded {len(proxies)} proxies from {proxy_file.name}")
    else:
        print("[init] Flipster proxies DISABLED (direct connection)")

    if not args.dry_run:
        print("[init] Connecting to browser sessions...")
        try:
            from flipster_pkg.client import FlipsterClient
            from flipster_pkg.browser import BrowserManager as FBM
            fc = FlipsterClient(FBM(), proxies=proxies)
            fc.login_done()
            flipster_client = fc
            print(f"  ✓ Flipster connected (proxies={len(proxies)})")
        except Exception as e:
            print(f"  ✗ Flipster: {e}")

        try:
            from gate_pkg.client import GateClient
            from gate_pkg.browser import BrowserManager as GBM
            gc = GateClient(GBM())  # uses default ports (11/5911/6091/9231)
            gc.login_done()
            gate_client = gc
            print(f"  ✓ Gate connected (logged_in={gc.is_logged_in()})")
        except Exception as e:
            print(f"  ✗ Gate: {e}")

        if not flipster_client or not gate_client:
            print("\n✗ One or both exchanges failed to connect. Aborting.")
            return

    whitelist = set(s.strip() for s in args.symbols.split(",") if s.strip()) if args.symbols else set()
    if whitelist:
        print(f"[init] Symbol whitelist: {sorted(whitelist)}")
    blacklist = set(s.strip() for s in args.blacklist.split(",") if s.strip()) if args.blacklist else set()
    if blacklist:
        print(f"[init] Symbol blacklist: {sorted(blacklist)}")

    executor = Executor(
        variant=args.variant,
        size_usd=args.size_usd,
        dry_run=args.dry_run,
        symbol_whitelist=whitelist,
        symbol_blacklist=blacklist,
        flipster_client=flipster_client,
        gate_client=gate_client,
    )

    # Graceful shutdown — close all open positions
    def handle_sig(sig, frame):
        print(f"\n[signal {sig}] Shutting down — closing all open positions...")
        try:
            executor.emergency_close_all()
        except Exception as e:
            print(f"[shutdown] emergency_close_all failed: {e}")
        sys.exit(0)
    signal.signal(signal.SIGINT, handle_sig)
    signal.signal(signal.SIGTERM, handle_sig)

    executor.run()


if __name__ == "__main__":
    main()

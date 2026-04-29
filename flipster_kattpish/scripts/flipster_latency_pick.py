#!/usr/bin/env python3
"""
flipster_latency_pick.py — LIVE strategy: pick off stale web-side quotes.

Hypothesis: api WS leads web WS by 40-200ms on decisive moves. MMs quoting based on
web feed → their quotes are stale during that window. Hit them with a LIMIT order
at web best. If stale, we fill at that stale level. If web already caught up, our
LIMIT is inside the spread and simply doesn't fill — no slippage risk.

ENTRY trigger:
  diff_bp = (api.mid - web.mid) / web.mid * 1e4
  |diff_bp| >= MIN_EDGE_BP AND web_age_ms < MAX_WEB_AGE_MS

EXIT:
  market close after MAX_HOLD_MS, or on STOP_BP adverse move, or TP_BP favorable.

RISK:
  - 1 concurrent position max
  - per-trade fixed USD size
  - kill switch: cum PnL <= -KILL_USD
  - auto-stop after DURATION_MIN
  - rate limit: N trades per 60s

Run:
  python3 scripts/flipster_latency_pick.py --dry-run           # no orders, log signals
  python3 scripts/flipster_latency_pick.py --live              # REAL orders
"""
from __future__ import annotations
import argparse, asyncio, hashlib, hmac, importlib.util, json, os, sys, time, urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional
import websockets

try:
    from dotenv import load_dotenv
    load_dotenv(Path(__file__).resolve().parent.parent / ".env")
except ImportError:
    pass

# ---- Binance momentum filter via ZMQ (data_publisher feeds tcp://211.181.122.24:6000) ----
import struct
from collections import deque

BINANCE_ZMQ_ADDR = "tcp://211.181.122.24:6000"
# sym (e.g. "BTC_USDT") -> deque of (ts_ms, mid)
BINANCE_HIST: dict = {}
BINANCE_HIST_MAXLEN = 4096
BINANCE_HIST_MAX_AGE_MS = 30_000
MIN_TF_AGREE = 3        # out of 4 timeframes (1s/5s/10s/20s)
MOMENTUM_TF_MS = (1000, 5000, 10000, 20000)


def _to_binance_sym(flipster_sym: str) -> str:
    """BTCUSDT.PERP → BTC_USDT (Binance ZMQ symbol format)."""
    base = flipster_sym.replace("USDT.PERP", "")
    return f"{base}_USDT"


async def binance_zmq_feed():
    """Subscribe to Binance bookticker ZMQ and maintain rolling price history per sym."""
    import zmq.asyncio
    ctx = zmq.asyncio.Context.instance()
    s = ctx.socket(zmq.SUB)
    s.setsockopt(zmq.SUBSCRIBE, b"")
    s.setsockopt(zmq.RCVHWM, 100_000)
    s.connect(BINANCE_ZMQ_ADDR)
    print(f"[binance-zmq] connected {BINANCE_ZMQ_ADDR}", flush=True)
    while not STATE.kill.is_set():
        try:
            parts = await asyncio.wait_for(s.recv_multipart(), timeout=5.0)
        except asyncio.TimeoutError:
            continue
        except Exception as e:
            print(f"[binance-zmq] err: {e}", flush=True)
            await asyncio.sleep(2)
            continue
        if len(parts) != 2 or len(parts[1]) != 120:
            continue
        sym = parts[1][16:48].rstrip(b"\x00").decode("ascii", errors="ignore")
        if not sym:
            continue
        bid, ask = struct.unpack_from("<dd", parts[1], 48)
        mid = (bid + ask) / 2.0
        ts_ms = time.time() * 1000
        dq = BINANCE_HIST.get(sym)
        if dq is None:
            dq = deque(maxlen=BINANCE_HIST_MAXLEN)
            BINANCE_HIST[sym] = dq
        dq.append((ts_ms, mid))
        cutoff = ts_ms - BINANCE_HIST_MAX_AGE_MS
        while dq and dq[0][0] < cutoff:
            dq.popleft()


def binance_momentum_agree(flipster_sym: str, side: str) -> tuple[bool, int, str]:
    """True if Binance recent returns agree with side direction in >= MIN_TF_AGREE windows.
    Returns (pass, n_agree, detail_str)."""
    bsym = _to_binance_sym(flipster_sym)
    dq = BINANCE_HIST.get(bsym)
    if not dq or len(dq) < 2:
        return False, 0, f"no_binance_data({bsym})"
    now_ms, cur_mid = dq[-1]
    if cur_mid <= 0:
        return False, 0, "zero_mid"
    agree = 0
    details = []
    for w in MOMENTUM_TF_MS:
        target = now_ms - w
        past_mid = None
        for ts, mid in dq:
            if ts <= target:
                past_mid = mid
            else:
                break
        if past_mid is None or past_mid <= 0:
            details.append(f"{w}ms=NA")
            continue
        ret_bp = (cur_mid - past_mid) / past_mid * 1e4
        ok = (side == "buy" and ret_bp > 0) or (side == "sell" and ret_bp < 0)
        if ok:
            agree += 1
        details.append(f"{w}ms={ret_bp:+.1f}bp{'✓' if ok else '✗'}")
    return (agree >= MIN_TF_AGREE, agree, " ".join(details))

# ---- Proxy pool: rotate per HTTP order; single fixed proxy for WS feed ----
_PROXY_URL: Optional[str] = None      # fallback single HTTP proxy (if pool empty)
_WS_PROXY_URL: Optional[str] = None   # WS feed proxy (same IP as cookie-issuing for CF)
_HTTP_POOL: list = []                 # list of proxy URLs for round-robin
_HTTP_POOL_IDX: int = 0
_HTTP_BAD: dict = {}                  # proxy_url -> unix_sec_until


def _pick_http_proxy() -> Optional[str]:
    """Round-robin over _HTTP_POOL, skipping blacklisted entries."""
    if not _HTTP_POOL:
        return _PROXY_URL
    global _HTTP_POOL_IDX
    now = time.time()
    for _ in range(len(_HTTP_POOL)):
        url = _HTTP_POOL[_HTTP_POOL_IDX % len(_HTTP_POOL)]
        _HTTP_POOL_IDX += 1
        if _HTTP_BAD.get(url, 0) < now:
            return url
    return _HTTP_POOL[0]  # all blacklisted — use first anyway


def _mark_bad(url: str, seconds: int = 300):
    _HTTP_BAD[url] = time.time() + seconds


class _AiohttpWSAdapter:
    """Minimal websockets-compatible adapter over aiohttp.ClientWebSocketResponse.

    Supports `async with` + `async for` iteration + `.send(text)`."""
    def __init__(self, ws, sess):
        self._ws = ws
        self._sess = sess

    async def __aenter__(self):
        return self

    async def __aexit__(self, *a):
        try: await self._ws.close()
        except Exception: pass
        try: await self._sess.close()
        except Exception: pass

    async def send(self, data):
        if isinstance(data, (bytes, bytearray)):
            await self._ws.send_bytes(data)
        else:
            await self._ws.send_str(data)

    def __aiter__(self):
        return self

    async def __anext__(self):
        msg = await self._ws.__anext__()
        import aiohttp
        if msg.type == aiohttp.WSMsgType.TEXT:
            return msg.data
        if msg.type == aiohttp.WSMsgType.BINARY:
            return msg.data
        if msg.type in (aiohttp.WSMsgType.CLOSED, aiohttp.WSMsgType.CLOSE,
                        aiohttp.WSMsgType.CLOSING, aiohttp.WSMsgType.ERROR):
            raise StopAsyncIteration
        return ""


async def _ws_connect_proxied(url: str, additional_headers=None, ping_interval=20,
                              max_size=10_000_000, **kwargs):
    """Return a connected WS wrapped to look like `websockets` (async ctx + async iter).
    Routes via WS-specific proxy when _WS_PROXY_URL is set (can differ from HTTP proxy)."""
    import aiohttp
    headers = dict(additional_headers) if additional_headers else {}
    proxy = _WS_PROXY_URL or _PROXY_URL
    sess = aiohttp.ClientSession()
    try:
        ws = await sess.ws_connect(
            url, headers=headers, proxy=proxy or None,
            heartbeat=ping_interval, max_msg_size=max_size,
        )
    except Exception:
        await sess.close()
        raise
    return _AiohttpWSAdapter(ws, sess)

MONOREPO = Path("/home/gate1/projects/quant/hft_monorepo")

def _load_pkg(name, root):
    pkg_dir = root / "python"
    spec = importlib.util.spec_from_file_location(
        name, pkg_dir / "__init__.py", submodule_search_locations=[str(pkg_dir)]
    )
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod

flipster_pkg = _load_pkg("flipster_pkg_lp", MONOREPO / "flipster_web")
FlipsterClient = flipster_pkg.client.FlipsterClient
OrderParams = flipster_pkg.order.OrderParams
Side = flipster_pkg.order.Side
OrderType = flipster_pkg.order.OrderType
MarginType = flipster_pkg.order.MarginType

# Position WS tracker (v11-style) — ground truth from Flipster private stream.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from positions_ws import WSPositionTracker  # noqa

# --- Defaults (overridable via CLI) ---
MIN_EDGE_BP = 1.0         # loose — user thesis: any api-web diff is tradeable (dynamic TP covers)
FLIP_TRIGGER_BP = 5.0     # genuine opposite sign required to flip
MAX_WEB_AGE_MS = 5_000    # with market/tickers 5Hz heartbeat + ticker prop delay, 5s is reasonable
MIN_HOLD_MS = 3000        # minimum 3s hold before TP
MAX_HOLD_MS = 20_000      # TIGHTENED 60→20s — long holds dominated by -tail; tp_fallback catches + faster
TP_BP = 15.0              # fallback TP (dynamic per-position tp_target_price takes priority)
STOP_BP_DEFAULT = 5.0     # TIGHTENED 7→5 — actual fills slip +3bp → want realized stop ~8bp not 10
HARD_STOP_BP = 15.0       # TIGHTENED 20→15 — faster tail cut
TP_FALLBACK_AGE_MS = 10_000  # after 10s, if in profit, take market — bracket missed the converge
TP_FALLBACK_MIN_BP = 1.0     # minimum favorable bp to trigger fallback profit-take
MAX_CROSS_BP = 20.0       # reject signals where cross_bp > 20bp (overshoot → reverts)
USE_DYNAMIC_TP = True     # use entry_api_mid as TP target (web catch-up price)
STOP_CONFIRM_TICKS = 1    # TIGHTENED 2→1 — stops slipping -7bp→-15bp; need faster response
MIN_HOLD_MS_FOR_STOP = 2000  # don't stop in first 2s (wait for initial price resolution)
FLIP_COOLDOWN_MS = 2000   # cooldown after any flip
STOP_REENTRY_COOLDOWN_MS = 30_000  # 30s lockout on same sym after stop (avoid losing streaks)
FEE_BP_ESTIMATE = 5.0
RATE_LIMIT_PER_MIN = 40   # raised — multi-sym flipping
ENTRY_TIMEOUT_MS = 30_000    # safety cap only (feed-stale protection); convergence is primary cancel
ENTRY_POLL_MS = 100          # poll interval during entry wait (tracker + gap check)
ENTRY_CONVERGE_BP = 0.5      # if api-web gap drops below this, edge is gone → cancel
ENTRY_MAX_SPREAD_BP = 6.0    # reject signal if either api/web spread > this — illiquid = adverse-select trap

KEY = os.environ.get("FLIPSTER_API_KEY")
SEC = os.environ.get("FLIPSTER_API_SECRET")


@dataclass
class Position:
    sym: str
    side: str
    size_usd: float
    entry_mid_api: float
    entry_fill_price: float
    entry_time_ms: float
    slot: int
    stop_confirm: int = 0
    adopted: bool = False
    # Dynamic TP: target price where we expect web to catch up to api.
    tp_target_price: float = 0.0
    # Bracket TP order (placed on exchange immediately after entry fills).
    # Exchange auto-fills when price reaches tp_target_price — no polling needed.
    tp_order_id: str = ""
    tracker_seen: bool = False  # True once private WS confirmed position exists on exchange


SIG_CONFIRM_COUNT = 2   # require N consecutive same-direction signals before entry
SIG_CONFIRM_WINDOW_MS = 2000  # all N signals must fall within this window

# --- Filter knobs (cross-book + EMA + monotonicity + vol-gate) ---
WEB_EMA_TAU_MS = 150            # short EMA on web_mid to kill step-function artifacts
API_MID_HIST_MAX_AGE_MS = 2000  # keep up to 2s of api mids per sym
MONO_WINDOW_MS = 400            # monotonicity look-back window (Binance is dense)
MONO_MIN_SAMPLES = 4            # Binance bookticker ticks fast enough for strict check
MONO_MAX_REVERSAL = 0.3         # max fraction of reversing deltas (30%)
VOL_GATE_WINDOW_MS = 1000       # Binance realized-vol window
VOL_GATE_MIN_RANGE_BP = 3.0     # reject entry if Binance 1s high-low range < this
# Orderbook staleness guard — ticker mid vs orderbook mid divergence
ORDERBOOK_STALE_DIVERGE_BP = 2.0  # if ticker_mid diverges > this from (bid+ask)/2, orderbook stale
# A. Cross-book persistence — require last N api ticks to all be crossed same direction
CROSS_PERSIST_COUNT = 2         # LOOSENED from 3 (was too restrictive in 33min → 27 opens)
CROSS_PERSIST_WINDOW_MS = 3000  # drop entries older than this
# B. Symbol adaptive cooldown — blacklist sym after loss streak
LOSS_STREAK_LOOKBACK_MS = 15 * 60_000  # 15-min window
LOSS_STREAK_THRESHOLD = 1       # AGGRESSIVE: even 1 loss → blacklist (was 2)
SYM_BLACKLIST_MS = 20 * 60_000  # 20-min lockout (was 15)

# Hard blacklist from historical analysis — symbols that consistently drain PnL.
# Tier 1: both halves of 2026-04-19 data.  Tier 2: out-of-sample worst performers.
HARD_BLACKLIST = {
    # Tier 1 (13)
    "GUNUSDT.PERP", "BNTUSDT.PERP", "GRIFFAINUSDT.PERP", "RAVEUSDT.PERP",
    "MOVEUSDT.PERP", "ENJUSDT.PERP", "GWEIUSDT.PERP", "BOMEUSDT.PERP",
    "PIEVERSEUSDT.PERP", "MUSDT.PERP", "PROMUSDT.PERP", "MERLUSDT.PERP",
    "ARPAUSDT.PERP",
    # Tier 2 (out-of-sample worst, 7)
    "BOBUSDT.PERP", "MEMEUSDT.PERP", "BLURUSDT.PERP", "SAPIENUSDT.PERP",
    "FOLKSUSDT.PERP", "RAREUSDT.PERP", "HAEDALUSDT.PERP",
    # Tier 3 (2h dry-run bleed: high trade count, sub-break-even WR)
    "SUPERUSDT.PERP", "JCTUSDT.PERP", "KERNELUSDT.PERP", "BASEDUSDT.PERP",
    # Tier 4 (this session's tail-loss offenders: -42bp to -155bp single trades)
    "IRYSUSDT.PERP", "ALCHUSDT.PERP", "BSBUSDT.PERP", "HYPEUSDT.PERP",
}
# C. Recent velocity filter (FIX for late-lead problem):
# Require Binance to still be moving in entry direction over short window —
# catches "move already peaked" cases where 1s/5s returns look good but movement is dying.
RECENT_VEL_WINDOW_MS = 400      # widened from 250 (more samples in window)
RECENT_VEL_MIN_BP = 0.5         # LOOSENED from 1.2 — only reject clear reversals, allow small positive moves
# D. Cross-edge growing check (disabled — redundant with cross-persistence)
CROSS_EDGE_GROWING_CHECK = False
# sym -> deque of (ts_ms, api_mid)
API_MID_HIST: dict = {}
# sym -> (ema_value, last_ts_ms)
WEB_MID_EMA: dict = {}
# sym -> deque of (ts_ms, direction +1/-1) — cross state on each api tick
CROSS_HIST: dict = {}
# sym -> deque of (close_ts_ms, pnl_usd, reason_str)
LOSS_HIST: dict = {}
# sym -> unblock_ts_ms (epoch ms)
SYM_BLACKLIST: dict = {}
# sym -> last cross_bp magnitude (for "edge growing" check)
LAST_CROSS_BP: dict = {}


def binance_monotonic(flipster_sym: str, side: str) -> tuple[bool, str]:
    """Monotonicity filter on Binance bookticker mid (dense tick stream).
    - If Binance has no data for the sym, pass (don't block alts not on Binance).
    - If we have >= MONO_MIN_SAMPLES in window, require start→end move in favorable
      direction (≥ half MIN_EDGE_BP) AND delta reversal ratio ≤ MONO_MAX_REVERSAL.
    """
    bsym = _to_binance_sym(flipster_sym)
    dq = BINANCE_HIST.get(bsym)
    if not dq:
        return True, f"no_binance({bsym})_passthru"
    cutoff = now_ms() - MONO_WINDOW_MS
    recent = [(t, m) for t, m in dq if t >= cutoff]
    if len(recent) < MONO_MIN_SAMPLES:
        return True, f"sparse({len(recent)})_passthru"
    start, end = recent[0][1], recent[-1][1]
    if start <= 0:
        return True, "zero_start_passthru"
    total_bp = (end - start) / start * 1e4
    half_edge = MIN_EDGE_BP * 0.5
    if side == "buy" and total_bp < half_edge:
        return False, f"move={total_bp:+.1f}bp<{half_edge:.1f}"
    if side == "sell" and total_bp > -half_edge:
        return False, f"move={total_bp:+.1f}bp>-{half_edge:.1f}"
    deltas = [recent[i + 1][1] - recent[i][1] for i in range(len(recent) - 1)]
    if not deltas:
        return True, "no_deltas_passthru"
    wrong = sum(1 for d in deltas if (side == "buy" and d < 0) or (side == "sell" and d > 0))
    ratio = wrong / len(deltas)
    if ratio > MONO_MAX_REVERSAL:
        return False, f"rev={ratio:.0%}"
    return True, f"move={total_bp:+.1f}bp,rev={ratio:.0%}"


def binance_vol_ok(flipster_sym: str) -> tuple[bool, float]:
    """True if Binance 1s range (high-low)/low in bp >= VOL_GATE_MIN_RANGE_BP.
    If Binance has no data for the sym, pass (don't block alts that aren't on Binance)."""
    bsym = _to_binance_sym(flipster_sym)
    dq = BINANCE_HIST.get(bsym)
    if not dq or len(dq) < 2:
        return True, 0.0  # no data → pass
    cutoff = now_ms() - VOL_GATE_WINDOW_MS
    recent = [m for t, m in dq if t >= cutoff]
    if len(recent) < 2:
        return True, 0.0  # sparse → pass
    lo, hi = min(recent), max(recent)
    if lo <= 0:
        return True, 0.0
    range_bp = (hi - lo) / lo * 1e4
    return range_bp >= VOL_GATE_MIN_RANGE_BP, range_bp


def update_web_ema(sym: str, web_mid: float, ts_ms: float) -> float:
    """Maintain short EMA of web_mid. Used for diff_bp calc (raw web used for orders)."""
    prev = WEB_MID_EMA.get(sym)
    if prev is None or prev[0] <= 0:
        WEB_MID_EMA[sym] = (web_mid, ts_ms)
        return web_mid
    prev_val, prev_ts = prev
    dt = max(1.0, ts_ms - prev_ts)
    import math
    alpha = 1.0 - math.exp(-dt / WEB_EMA_TAU_MS)
    new = prev_val + alpha * (web_mid - prev_val)
    WEB_MID_EMA[sym] = (new, ts_ms)
    return new


def update_cross_hist(sym: str, ts_ms: float, direction: int) -> None:
    """Append current cross-book state to per-sym history. direction: +1 (buy cross),
    -1 (sell cross), 0 (not crossed)."""
    dq = CROSS_HIST.get(sym)
    if dq is None:
        dq = deque(maxlen=64)
        CROSS_HIST[sym] = dq
    dq.append((ts_ms, direction))
    cutoff = ts_ms - CROSS_PERSIST_WINDOW_MS
    while dq and dq[0][0] < cutoff:
        dq.popleft()


def cross_persisted(sym: str, direction: int) -> tuple[bool, str]:
    """True if the last CROSS_PERSIST_COUNT api ticks on this sym were all crossed
    in the same direction."""
    dq = CROSS_HIST.get(sym)
    if not dq or len(dq) < CROSS_PERSIST_COUNT:
        return False, f"hist={len(dq) if dq else 0}<{CROSS_PERSIST_COUNT}"
    recent = list(dq)[-CROSS_PERSIST_COUNT:]
    for _, d in recent:
        if d != direction:
            return False, f"broken(dir={d})"
    return True, f"ok({CROSS_PERSIST_COUNT}x)"


def record_close_for_blacklist(sym: str, pnl_usd: float, reason: str) -> None:
    """Track closes per sym; if N stops/losses fell within LOSS_STREAK_LOOKBACK_MS,
    blacklist the sym for SYM_BLACKLIST_MS."""
    is_loss = pnl_usd < 0 or reason.startswith("stop") or reason.startswith("max_hold")
    dq = LOSS_HIST.get(sym)
    if dq is None:
        dq = deque(maxlen=32)
        LOSS_HIST[sym] = dq
    ts = now_ms()
    dq.append((ts, pnl_usd, reason, is_loss))
    cutoff = ts - LOSS_STREAK_LOOKBACK_MS
    while dq and dq[0][0] < cutoff:
        dq.popleft()
    recent_losses = sum(1 for _, _, _, loss in dq if loss)
    if recent_losses >= LOSS_STREAK_THRESHOLD:
        SYM_BLACKLIST[sym] = ts + SYM_BLACKLIST_MS
        dq.clear()  # reset counter — only block once per streak
        print(f"  [blacklist] {sym} locked {SYM_BLACKLIST_MS//60000}min "
              f"({recent_losses} losses in {LOSS_STREAK_LOOKBACK_MS//60000}min)", flush=True)


def is_blacklisted(sym: str) -> tuple[bool, float]:
    """Return (blacklisted, remaining_ms)."""
    until = SYM_BLACKLIST.get(sym, 0)
    rem = until - now_ms()
    return (rem > 0, rem)


def binance_recent_velocity_ok(flipster_sym: str, side: str) -> tuple[bool, float]:
    """Tight velocity filter — catches 'move already peaked' case.
    Compute Binance return over last RECENT_VEL_WINDOW_MS. Require it to still
    be moving in the entry direction by at least RECENT_VEL_MIN_BP.

    Pass-through when Binance data missing (don't block alts not on Binance).
    """
    bsym = _to_binance_sym(flipster_sym)
    dq = BINANCE_HIST.get(bsym)
    if not dq or len(dq) < 2:
        return True, 0.0
    now_t = now_ms()
    target = now_t - RECENT_VEL_WINDOW_MS
    past_mid = None
    for ts, mid in dq:
        if ts <= target:
            past_mid = mid
        else:
            break
    cur_mid = dq[-1][1]
    if past_mid is None or past_mid <= 0:
        return True, 0.0  # window not filled yet → pass
    bp = (cur_mid - past_mid) / past_mid * 1e4
    if side == "buy" and bp >= RECENT_VEL_MIN_BP:
        return True, bp
    if side == "sell" and bp <= -RECENT_VEL_MIN_BP:
        return True, bp
    return False, bp


@dataclass
class State:
    web: dict = field(default_factory=dict)  # sym -> (bid, ask, ts_ms) from orderbooks-v2
    web_ticker_mid: dict = field(default_factory=dict)  # sym -> (mid, ts_ms) from market/tickers (5Hz heartbeat)
    api: dict = field(default_factory=dict)  # sym -> (bid, ask, mid, ts_ms)
    sig_history: dict = field(default_factory=dict)  # sym -> deque of (ts_ms, sign)
    positions: dict = field(default_factory=dict)  # sym -> Position (1 per sym)
    sym_cooldown: dict = field(default_factory=dict)  # sym -> next_allowed_ms
    sym_lock: dict = field(default_factory=dict)     # sym -> asyncio.Lock (serialize flip for one sym)
    cum_pnl_usd: float = 0.0
    trade_count: int = 0
    fills: int = 0
    closes: int = 0
    flips: int = 0
    no_fills: int = 0
    signals: int = 0
    trade_times: list = field(default_factory=list)
    kill: asyncio.Event = field(default_factory=asyncio.Event)
    start_ms: float = 0.0
    tracker: Optional["WSPositionTracker"] = None


STATE = State()
CLIENT: Optional[FlipsterClient] = None
LIVE = False
AMOUNT_USD = 10.0
KILL_USD = 20.0

# Structured rejection log — jsonl, one entry per filter skip.
# Enabled via --reject-log PATH. Used for post-hoc filter efficacy analysis.
_REJECT_FH = None  # file handle
# Shadow log — jsonl, one entry per cross-book signal candidate with ALL filter
# inputs. Enables full backtest sweeps without re-recording feed data.
_SHADOW_FH = None
# Execution log — intended vs actual fill for each open/close. Slippage diagnostic.
_EXEC_FH = None


def _log_exec(event: str, **fields):
    if _EXEC_FH is None:
        return
    try:
        ev = {"ts_ms": int(now_ms()), "event": event, **fields}
        _EXEC_FH.write(json.dumps(ev, default=str) + "\n")
    except Exception:
        pass


# Flipster drift log — for each SIG, measure Flipster price at T+N seconds
_DRIFT_FH = None
DRIFT_OFFSETS_S = [10, 30, 60, 120, 300, 600]


async def _flipster_drift_probe(sym, side, sig_num, entry_api_mid, entry_web_mid):
    """Record Flipster api/web price at multiple future offsets after SIG.
    Measures drift on THIS exchange (vs Binance), answering: does Flipster itself
    mean-revert after api leads?"""
    if _DRIFT_FH is None:
        return
    entry_ms = int(now_ms())
    for off_s in DRIFT_OFFSETS_S:
        target_ms = entry_ms + off_s * 1000
        await asyncio.sleep(max(0, (target_ms - now_ms()) / 1000))
        api = STATE.api.get(sym); web = STATE.web.get(sym)
        if api is None or web is None:
            continue
        api_bid, api_ask, api_mid, _ = api
        web_bid, web_ask, web_ts = web
        web_mid = (web_bid + web_ask) / 2
        # Signed drift: positive = favorable to signal direction
        api_drift_bp = (api_mid - entry_api_mid) / entry_api_mid * 1e4
        web_drift_bp = (web_mid - entry_web_mid) / entry_web_mid * 1e4
        if side == "sell":
            api_drift_bp = -api_drift_bp
            web_drift_bp = -web_drift_bp
        try:
            ev = {
                "sig_num": sig_num, "sym": sym, "side": side,
                "offset_s": off_s,
                "entry_api_mid": entry_api_mid, "entry_web_mid": entry_web_mid,
                "api_mid_now": api_mid, "web_mid_now": web_mid,
                "api_drift_bp": round(api_drift_bp, 2),
                "web_drift_bp": round(web_drift_bp, 2),
            }
            _DRIFT_FH.write(json.dumps(ev) + "\n")
        except Exception:
            pass


def _log_rejection(sym: str, filter_name: str, side: str, cross_bp: float, **extra):
    if _REJECT_FH is None:
        return
    try:
        ev = {
            "ts_ms": int(now_ms()),
            "sym": sym,
            "filter": filter_name,
            "side": side,
            "cross_bp": round(cross_bp, 3) if isinstance(cross_bp, (int, float)) else cross_bp,
            **extra,
        }
        _REJECT_FH.write(json.dumps(ev, default=str) + "\n")
    except Exception:
        pass


def _log_shadow(**fields):
    """Record a cross-book candidate with all filter inputs, independent of pass/reject."""
    if _SHADOW_FH is None:
        return
    try:
        _SHADOW_FH.write(json.dumps(fields, default=str) + "\n")
    except Exception:
        pass


def _binance_returns_snapshot(flipster_sym: str) -> dict:
    """Compute Binance returns over TF_MS for current moment — for shadow logging."""
    bsym = _to_binance_sym(flipster_sym)
    dq = BINANCE_HIST.get(bsym)
    if not dq or len(dq) < 2:
        return {}
    cur_mid = dq[-1][1]
    if cur_mid <= 0:
        return {}
    now_t = dq[-1][0]
    out = {"bx_cur": cur_mid}
    for w in MOMENTUM_TF_MS:
        target = now_t - w
        past = None
        for ts, mid in dq:
            if ts <= target:
                past = mid
            else:
                break
        if past and past > 0:
            out[f"bx_{w}ms_bp"] = round((cur_mid - past) / past * 1e4, 2)
    # 1s range
    cutoff = now_t - 1000
    recent = [m for t, m in dq if t >= cutoff]
    if len(recent) >= 2:
        lo, hi = min(recent), max(recent)
        if lo > 0:
            out["bx_range_1s_bp"] = round((hi - lo) / lo * 1e4, 2)
    # Mono 400ms
    cutoff_m = now_t - 400
    mono_win = [(t, m) for t, m in dq if t >= cutoff_m]
    if len(mono_win) >= 2:
        s, e = mono_win[0][1], mono_win[-1][1]
        if s > 0:
            out["bx_mono_bp"] = round((e - s) / s * 1e4, 2)
            out["bx_mono_n"] = len(mono_win)
            deltas = [mono_win[i+1][1] - mono_win[i][1] for i in range(len(mono_win)-1)]
            if deltas:
                # reversal counts per direction
                out["bx_mono_dn"] = sum(1 for d in deltas if d < 0)
                out["bx_mono_up"] = sum(1 for d in deltas if d > 0)
    # Recent velocity 250ms + 400ms
    for w_ms in (250, 400):
        cutoff_v = now_t - w_ms
        past_v = None
        for ts, mid in dq:
            if ts <= cutoff_v:
                past_v = mid
            else:
                break
        if past_v and past_v > 0:
            out[f"bx_vel_{w_ms}ms_bp"] = round((cur_mid - past_v) / past_v * 1e4, 2)
    return out


def now_ms() -> float:
    return time.time() * 1000


def rate_limited() -> bool:
    t = now_ms()
    STATE.trade_times = [x for x in STATE.trade_times if t - x < 60_000]
    return len(STATE.trade_times) >= RATE_LIMIT_PER_MIN


def should_stop() -> bool:
    if STATE.kill.is_set():
        return True
    if STATE.cum_pnl_usd <= -KILL_USD:
        print(f"[KILL] cum_pnl={STATE.cum_pnl_usd:.2f} <= -${KILL_USD}", flush=True)
        STATE.kill.set()
        return True
    return False


async def _extract_cookies(cdp_ws_url):
    async with websockets.connect(cdp_ws_url) as ws:
        await ws.send(json.dumps({"id": 1, "method": "Storage.getCookies"}))
        resp = json.loads(await ws.recv())
        return {c["name"]: c["value"] for c in resp.get("result", {}).get("cookies", [])
                if "flipster" in c.get("domain", "")}


async def web_feed(symbols, cdp_port):
    while not STATE.kill.is_set():
        try:
            ver = json.loads(urllib.request.urlopen(
                f"http://localhost:{cdp_port}/json/version", timeout=5).read())
            cookies = await _extract_cookies(ver["webSocketDebuggerUrl"])
            cookie_str = "; ".join(f"{k}={v}" for k, v in cookies.items())
            headers = {"Cookie": cookie_str, "Origin": "https://flipster.io",
                       "User-Agent": "Mozilla/5.0 Firefox/149.0"}
            url = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"
            async with await _ws_connect_proxied(url, additional_headers=headers,
                                          ping_interval=20, max_size=10_000_000) as ws:
                # SINGLE subscribe msg with BOTH channels —
                #   orderbooks-v2: bid/ask for cross-book check (sparse for illiquid syms)
                #   tickers: midPrice at 5Hz per sym → fresh web heartbeat
                # "set" semantic: second subscribe REPLACES first, so both in one message.
                sub = {"s": {
                    "market/orderbooks-v2": {"rows": symbols},
                    "market/tickers":       {"rows": symbols},
                }}
                await ws.send(json.dumps(sub))
                print(f"[web] subscribed {len(symbols)} syms × (orderbooks-v2, tickers)",
                      flush=True)
                async for raw in ws:
                    if STATE.kill.is_set():
                        break
                    try:
                        d = json.loads(raw)
                    except json.JSONDecodeError:
                        continue
                    topic = d.get("t", {})
                    ts_ms = int(d.get("p", time.time_ns())) / 1e6

                    # --- orderbooks-v2: bid/ask for cross-book check ---
                    ob = topic.get("market/orderbooks-v2", {}).get("s", {})
                    if ob:
                        for sym, book in ob.items():
                            bids = book.get("bids", [])
                            asks = book.get("asks", [])
                            if not bids or not asks or len(bids[0]) < 2 or len(asks[0]) < 2:
                                continue
                            STATE.web[sym] = (float(bids[0][0]), float(asks[0][0]), ts_ms)

                    # --- tickers: fresh midPrice heartbeat (5Hz per sym) ---
                    tk = topic.get("market/tickers", {})
                    for sect in ("s", "u"):
                        for sym, row in tk.get(sect, {}).items():
                            mid_s = row.get("midPrice")
                            if mid_s is None:
                                continue
                            try:
                                mid = float(mid_s)
                            except (TypeError, ValueError):
                                continue
                            STATE.web_ticker_mid[sym] = (mid, ts_ms)
        except Exception as e:
            is_429 = "429" in str(e) or "rate" in str(e).lower()
            delay = 300 if is_429 else 3
            print(f"[web] err: {e} — retry {delay}s", flush=True)
            await asyncio.sleep(delay)


async def api_feed(symbols, signal_cb):
    while not STATE.kill.is_set():
        try:
            expires = int(time.time()) + 3600
            sig = hmac.new(SEC.encode(),
                           f"GET/api/v1/stream{expires}".encode(),
                           hashlib.sha256).hexdigest()
            headers = {"api-key": KEY, "api-expires": str(expires), "api-signature": sig}
            url = "wss://trading-api.flipster.io/api/v1/stream"
            async with websockets.connect(url, additional_headers=headers,
                                          ping_interval=20, max_size=10_000_000) as ws:
                topics = [f"ticker.{s}" for s in symbols]
                for i in range(0, len(topics), 50):
                    await ws.send(json.dumps({"op": "subscribe", "args": topics[i:i+50]}))
                print(f"[api] subscribed {len(topics)} ticker topics", flush=True)
                async for raw in ws:
                    if STATE.kill.is_set():
                        break
                    try:
                        d = json.loads(raw)
                    except json.JSONDecodeError:
                        continue
                    topic = d.get("topic", "")
                    if not topic.startswith("ticker."):
                        continue
                    sym = topic[len("ticker."):]
                    ts_ms = int(d.get("ts", time.time_ns())) / 1e6
                    for row in d.get("data", []):
                        for r in row.get("rows", []):
                            b = r.get("bidPrice")
                            a = r.get("askPrice")
                            if b is None or a is None:
                                continue
                            bid = float(b); ask = float(a)
                            mid = (bid + ask) / 2
                            STATE.api[sym] = (bid, ask, mid, ts_ms)
                            dq = API_MID_HIST.get(sym)
                            if dq is None:
                                dq = deque(maxlen=512)
                                API_MID_HIST[sym] = dq
                            dq.append((ts_ms, mid))
                            cutoff = ts_ms - API_MID_HIST_MAX_AGE_MS
                            while dq and dq[0][0] < cutoff:
                                dq.popleft()
                            await signal_cb(sym)
        except Exception as e:
            print(f"[api] err: {e} — retry 3s", flush=True)
            await asyncio.sleep(3)


def _extract_fill(resp: dict) -> tuple[float, float, Optional[int]]:
    pos = resp.get("position", {}) if isinstance(resp, dict) else {}
    order = resp.get("order", {}) if isinstance(resp, dict) else {}
    avg = float(pos.get("avgPrice") or order.get("avgPrice") or 0)
    size = abs(float(pos.get("size") or order.get("size") or 0))
    slot = pos.get("slot")
    if slot is None:
        slot = order.get("slot")
    return avg, size, slot


import uuid as _uuid

ONEWAY_ORDER_URL = "https://api.flipster.io/api/v2/trade/one-way/order/{sym}"


LEVERAGE = 50  # Rust/v11 style cap; per-sym capped at Flipster specs
_SYM_MAX_LEV: dict = {}  # base → max_leverage from symbols_v9.json
_SYM_LEV_CAP: dict = {}  # runtime-learned cap (lowered when TooHighLeverage errors)
_SYM_TICK: dict = {}     # base → price tick size from symbols_v9.json


def _load_sym_max_leverage():
    """Load per-symbol max leverage AND tick size from symbols_v9.json."""
    global _SYM_MAX_LEV, _SYM_TICK
    try:
        p = Path(__file__).resolve().parent / "symbols_v9.json"
        data = json.loads(p.read_text())
        for d in data:
            base = d.get("base")
            ml = int(d.get("max_leverage", 10))
            tk = float(d.get("tick", 0.0))
            if base:
                _SYM_MAX_LEV[base] = ml
                if tk > 0:
                    _SYM_TICK[base] = tk
        print(f"[cfg] loaded max_leverage for {len(_SYM_MAX_LEV)} symbols "
              f"(tick for {len(_SYM_TICK)})", flush=True)
    except Exception as e:
        print(f"[cfg] symbols_v9.json load failed: {e}", flush=True)


def _tick_round(sym: str, price: float, direction: str) -> float:
    """Round `price` to nearest valid tick for `sym`.
    `direction`: 'up' floors TP for buy-side (sell order must be >= current),
                 'down' floors for sell-side. 'near' for best nearest.
    """
    base = sym.replace("USDT.PERP", "")
    tick = _SYM_TICK.get(base)
    if not tick or tick <= 0:
        return price
    n = price / tick
    if direction == "up":
        return round((int(n) + (1 if n > int(n) else 0)) * tick, 10)
    if direction == "down":
        return round(int(n) * tick, 10)
    return round(round(n) * tick, 10)


def _leverage_for(sym: str) -> int:
    """Return min(LEVERAGE, per-sym max, runtime-learned cap). Rust-style cap."""
    base = sym.replace("USDT.PERP", "")
    sym_max = _SYM_MAX_LEV.get(base, 10)
    runtime_cap = _SYM_LEV_CAP.get(sym, 1000)
    return min(LEVERAGE, sym_max, runtime_cap)


def _submit_one_way_order(sym: str, side: str, price: float,
                          amount_usd: float, reduce_only: bool,
                          order_type: str, leverage: Optional[int] = None,
                          post_only: bool = False) -> dict:
    if leverage is None:
        leverage = _leverage_for(sym)
    """POST /api/v2/trade/one-way/order/{sym} — one-way mode endpoint.
    Rotates HTTP proxy per call; on 429, marks proxy bad for 5min and retries next."""
    now_ns = str(time.time_ns())
    body = {
        "side": side,
        "requestId": str(_uuid.uuid4()),
        "timestamp": now_ns,
        "reduceOnly": reduce_only,
        "refServerTimestamp": now_ns,
        "refClientTimestamp": now_ns,
        "leverage": leverage,
        "price": str(price),
        "amount": str(amount_usd),
        "marginType": "Cross",
        "orderType": order_type,
    }
    if post_only:
        body["postOnly"] = True
    url = ONEWAY_ORDER_URL.format(sym=sym)
    last_err = None
    for attempt in range(min(3, max(1, len(_HTTP_POOL) or 1))):
        proxy = _pick_http_proxy()
        req_kwargs = {"json": body, "timeout": 10}
        if proxy:
            req_kwargs["proxies"] = {"http": proxy, "https": proxy}
        r = CLIENT._session.post(url, **req_kwargs)
        if r.status_code == 429:
            if proxy:
                _mark_bad(proxy, 300)
            last_err = f"429 (proxy {proxy.split('@')[-1] if proxy else 'none'})"
            continue
        if r.status_code in (401, 403):
            raise PermissionError(f"Auth {r.status_code}")
        try:
            data = r.json()
        except Exception:
            data = {"raw": r.text[:200]}
        # TooHighLeverage → learn sym's cap, retry with halved leverage
        if r.status_code == 400 and "TooHighLeverage" in json.dumps(data):
            new_cap = max(1, leverage // 2)
            _SYM_LEV_CAP[sym] = new_cap
            body["leverage"] = new_cap
            print(f"  [lev-cap] {sym}: {leverage} → {new_cap} (learned from TooHighLeverage)", flush=True)
            leverage = new_cap
            continue
        if r.status_code >= 400:
            raise RuntimeError(f"API error {r.status_code}: {json.dumps(data)[:300]}")
        return data
    raise RuntimeError(f"429 exhausted: {last_err}")


async def _open_position(sym, desired_side, api_mid, api_ask, api_bid,
                          web_bid, web_ask):
    """Entry: post-only LIMIT at web's stale top-of-book price.
    Hypothesis: MMs quoting from web feed still have orders at stale levels;
    if api→web converges via SELLER side hitting our bid (or vice versa) we
    fill at a discount. If web catches up without fill → cancel (edge gone)."""
    STATE.trade_times.append(now_ms())
    # Stale-maker price:
    #   buy  → web_ask (post below current best_ask = api_ask, non-crossing)
    #   sell → web_bid (post above current best_bid = api_bid, non-crossing)
    if desired_side == "buy":
        side_api = "Long"
        raw_price = web_ask
        limit_price = _tick_round(sym, raw_price, "down")
    else:
        side_api = "Short"
        raw_price = web_bid
        limit_price = _tick_round(sym, raw_price, "up")
    entry_kind = "MAKER_STALE"
    intended_price = limit_price
    try:
        resp = _submit_one_way_order(sym, side_api, limit_price, AMOUNT_USD,
                                     reduce_only=False, order_type="ORDER_TYPE_LIMIT",
                                     post_only=True)
    except Exception as e:
        msg = str(e)[:150]
        if "429" in msg:
            STATE.no_fills += 1
        else:
            print(f"  [ORDER_ERR] {sym} {desired_side}: {msg}", flush=True)
        return False
    avg, sz, _ = _extract_fill(resp)
    # Execution log for slippage analysis
    if avg > 0 and intended_price > 0:
        if desired_side == "buy":
            slip_bp = (avg - intended_price) / intended_price * 1e4
        else:
            slip_bp = (intended_price - avg) / intended_price * 1e4
        _log_exec("open", sym=sym, side=desired_side, kind=entry_kind,
                  intended=round(intended_price, 6), actual=round(avg, 6),
                  slip_bp=round(slip_bp, 2), filled=sz > 0,
                  api_bid=api_bid, api_ask=api_ask,
                  web_bid=web_bid, web_ask=web_ask)
    order_id = (resp.get("order", {}) or {}).get("orderId") if isinstance(resp, dict) else None
    if sz == 0:
        # Poll: every ENTRY_POLL_MS check (a) tracker for fill, (b) api-web gap.
        # If gap drops below ENTRY_CONVERGE_BP → edge gone, cancel early.
        # Max wait capped by ENTRY_TIMEOUT_MS (safety).
        max_wait_s = ENTRY_TIMEOUT_MS / 1000
        poll_s = ENTRY_POLL_MS / 1000
        waited_s = 0.0
        late_fill = False
        cancel_reason = "timeout"
        while waited_s < max_wait_s:
            await asyncio.sleep(poll_s)
            waited_s += poll_s
            # (a) Check tracker for late fill
            if STATE.tracker is not None and STATE.tracker.connected:
                info = STATE.tracker.position_side_qty_entry(sym, 0)
                if info is not None:
                    tr_side, tr_qty, tr_entry = info
                    want_long = (desired_side == "buy")
                    tr_long = (tr_side == "Long")
                    if want_long == tr_long and tr_qty > 0 and tr_entry > 0:
                        avg = tr_entry
                        sz = tr_qty
                        late_fill = True
                        print(f"  [LATE_FILL] {sym} {desired_side} filled in "
                              f"{waited_s*1000:.0f}ms @{avg:.6f} qty={sz:g}", flush=True)
                        break
            # (b) Check api-web convergence — cancel if edge gone
            api_now = STATE.api.get(sym)
            web_now = STATE.web.get(sym)
            if api_now and web_now:
                api_bid_now, api_ask_now, api_mid_now, _ = api_now
                web_bid_now, web_ask_now, _ = web_now
                web_mid_now = (web_bid_now + web_ask_now) / 2.0
                if web_mid_now > 0 and api_mid_now > 0:
                    diff_now_bp = (api_mid_now - web_mid_now) / web_mid_now * 1e4
                    # Cross recheck: strict test for stale-maker edge persistence
                    if desired_side == "buy":
                        # cross gone when api_bid <= web_ask (book no longer stale-crossed)
                        if api_bid_now <= web_ask_now:
                            cancel_reason = f"cross_gone(api_bid={api_bid_now:.6f} ≤ web_ask={web_ask_now:.6f})"
                            break
                        if diff_now_bp < ENTRY_CONVERGE_BP:
                            cancel_reason = f"converged({diff_now_bp:+.1f}bp)"
                            break
                    else:
                        if api_ask_now >= web_bid_now:
                            cancel_reason = f"cross_gone(api_ask={api_ask_now:.6f} ≥ web_bid={web_bid_now:.6f})"
                            break
                        if diff_now_bp > -ENTRY_CONVERGE_BP:
                            cancel_reason = f"converged({diff_now_bp:+.1f}bp)"
                            break
        if not late_fill:
            print(f"  [ENTRY_CANCEL:{cancel_reason}] {sym} {desired_side} after "
                  f"{waited_s*1000:.0f}ms", flush=True)
            if order_id:
                try:
                    cancel_url = f"https://api.flipster.io/api/v2/trade/orders/{sym}/{order_id}"
                    cancel_ts = str(time.time_ns())
                    cancel_body = {
                        "requestId": str(_uuid.uuid4()),
                        "timestamp": cancel_ts,
                        "attribution": "TRADE_PERPETUAL_DEFAULT",
                    }
                    proxy = _pick_http_proxy()
                    req_kwargs = {"json": cancel_body, "timeout": 10}
                    if proxy:
                        req_kwargs["proxies"] = {"http": proxy, "https": proxy}
                    r = CLIENT._session.delete(cancel_url, **req_kwargs)
                    STATE.no_fills += 1
                    if r.status_code >= 400 and r.status_code != 404:
                        try:
                            detail = r.json()
                        except Exception:
                            detail = {"raw": r.text[:200]}
                        msg = json.dumps(detail)[:200]
                        if "NotFound" not in msg and "AlreadyFilled" not in msg:
                            print(f"  [CANCEL_ERR] {sym}: {r.status_code} {msg}", flush=True)
                except Exception as e:
                    emsg = str(e)
                    if "404" not in emsg and "NotFound" not in emsg:
                        print(f"  [CANCEL_ERR] {sym}: {emsg[:100]}", flush=True)
            else:
                STATE.no_fills += 1
            return False
    STATE.fills += 1
    STATE.trade_count += 1
    # Dynamic TP target: side-aware api price at signal time
    #   Long close sells → target = api_ask (where buyers will hit us as web_ask catches up)
    #   Short close buys → target = api_bid (where sellers will hit us as web_bid catches down)
    if USE_DYNAMIC_TP:
        api_tp = api_ask if desired_side == "buy" else api_bid
        if desired_side == "buy":
            tp_target = _tick_round(sym, api_tp, "up")
        else:
            tp_target = _tick_round(sym, api_tp, "down")
    else:
        tp_target = 0.0
    STATE.positions[sym] = Position(
        sym=sym, side=desired_side, size_usd=AMOUNT_USD,
        entry_mid_api=api_mid, entry_fill_price=avg,
        entry_time_ms=now_ms(), slot=0,
        tp_target_price=tp_target,
    )
    tp_bp_from_fill = abs((tp_target - avg) / avg * 1e4) if tp_target else TP_BP
    print(f"  [OPEN#{STATE.fills}:{entry_kind}] {sym} {desired_side} "
          f"avg={avg:.6f} tp_target={tp_target:.6f} ({tp_bp_from_fill:.1f}bp) "
          f"open={len(STATE.positions)}", flush=True)

    # ----- BRACKET: place TP LIMIT post-only reduce_only at tp_target -----
    # Exchange auto-fills when market reaches this price — no polling needed.
    if USE_DYNAMIC_TP and tp_target > 0:
        try:
            opp_side = "Short" if desired_side == "buy" else "Long"
            tp_resp = _submit_one_way_order(
                sym, opp_side, tp_target, AMOUNT_USD * 2,
                reduce_only=True,
                order_type="ORDER_TYPE_LIMIT",
                post_only=True,
            )
            tp_oid = (tp_resp.get("order", {}) or {}).get("orderId") if isinstance(tp_resp, dict) else None
            if tp_oid:
                STATE.positions[sym].tp_order_id = tp_oid
                print(f"  [TP_BRACKET] {sym} {desired_side} LIMIT at {tp_target:.6f} id={tp_oid[:8]}", flush=True)
        except Exception as e:
            msg = str(e)[:150]
            if "PostOnly" in msg or "would cross" in msg.lower():
                # Market already crossed TP level — can close immediately via MARKET
                print(f"  [TP_BRACKET_CROSS] {sym}: already at/past TP, instant close", flush=True)
            else:
                print(f"  [TP_BRACKET_ERR] {sym}: {msg}", flush=True)
    return True


MAKER_CLOSE_TIMEOUT_S = 3.0   # tp path: wait for post-only LIMIT close to fill, else MARKET
MAKER_CLOSE_TIMEOUT_STOP_S = 1.0  # stop/max_hold: shorter wait — still try maker first to save slippage


async def _close_position(sym, pos, api_mid, reason):
    """Close flow with bracket support:
      - If TP bracket exists (pos.tp_order_id): cancel it first (avoid double-close)
      - TP path: LIMIT post-only → wait MAKER_CLOSE_TIMEOUT_S → MARKET fallback
      - stop/max_hold: LIMIT post-only → SHORT wait MAKER_CLOSE_TIMEOUT_STOP_S → MARKET fallback
      - hard_stop: MARKET immediately (risk limit breach)
    """
    opp_side = "Short" if pos.side == "buy" else "Long"
    urgent = reason.startswith("hard_stop")  # only hard_stop skips maker attempt
    maker_wait_s = (MAKER_CLOSE_TIMEOUT_STOP_S
                    if (reason.startswith("stop") or reason.startswith("max_hold"))
                    else MAKER_CLOSE_TIMEOUT_S)
    exit_avg = 0.0
    maker_filled = False
    intended_close_price = 0.0  # for execution log

    # Cancel bracket TP if exists (before we place our own close)
    if pos.tp_order_id:
        try:
            cancel_url = f"https://api.flipster.io/api/v2/trade/orders/{sym}/{pos.tp_order_id}"
            cancel_body = {
                "requestId": str(_uuid.uuid4()),
                "timestamp": str(time.time_ns()),
                "attribution": "TRADE_PERPETUAL_DEFAULT",
            }
            proxy = _pick_http_proxy()
            req_kwargs = {"json": cancel_body, "timeout": 10}
            if proxy:
                req_kwargs["proxies"] = {"http": proxy, "https": proxy}
            CLIENT._session.delete(cancel_url, **req_kwargs)
        except Exception:
            pass  # may have filled already — continue

    if not urgent:
        # Maker LIMIT close — rest on own side, wait for fill
        api = STATE.api.get(sym)
        web = STATE.web.get(sym)
        if api is None:
            urgent = True  # no price reference → fall through to MARKET
        else:
            # Passive maker rest — use WEB side to avoid crossing:
            #   close long (sell) → place sell at web_ask (joins ask queue, passive)
            #   close short (buy) → place buy at web_bid (joins bid queue, passive)
            # Falling back to api side would often cross when api leads web.
            api_bid, api_ask, _, _ = api
            if web is not None:
                web_bid_v, web_ask_v, _ = web
                maker_price = web_ask_v if pos.side == "buy" else web_bid_v
            else:
                maker_price = api_ask if pos.side == "buy" else api_bid
            intended_close_price = maker_price
            try:
                resp = _submit_one_way_order(sym, opp_side, maker_price, pos.size_usd * 2,
                                             reduce_only=True, order_type="ORDER_TYPE_LIMIT",
                                             post_only=True)
            except Exception as e:
                msg = str(e)[:150]
                # post-only rejected (would cross) or other error → fall back to MARKET
                if "PostOnly" in msg or "would cross" in msg.lower():
                    pass
                else:
                    print(f"  [MAKER_CLOSE_ERR] {sym}: {msg} — falling back to MARKET", flush=True)
                resp = None
            if resp is not None:
                exit_avg, sz, _ = _extract_fill(resp)
                order_id = (resp.get("order", {}) or {}).get("orderId") if isinstance(resp, dict) else None
                if sz > 0 and exit_avg > 0:
                    maker_filled = True  # instantly crossed & filled
                else:
                    # Poll for fill for up to maker_wait_s
                    deadline = time.time() + maker_wait_s
                    while time.time() < deadline:
                        await asyncio.sleep(0.3)
                        # Check if position was reduced via positions endpoint would be ideal;
                        # simpler: check if our LIMIT order got filled by re-querying the order.
                        # For now, rely on timeout and cancel — then MARKET close.
                        if STATE.positions.get(sym) is None:
                            maker_filled = True
                            break
                    if not maker_filled and order_id:
                        # Cancel the resting order
                        try:
                            cancel_url = f"https://api.flipster.io/api/v2/trade/orders/{sym}/{order_id}"
                            cancel_ts = str(time.time_ns())
                            cancel_body = {
                                "requestId": str(_uuid.uuid4()),
                                "timestamp": cancel_ts,
                                "attribution": "TRADE_PERPETUAL_DEFAULT",
                            }
                            proxy = _pick_http_proxy()
                            req_kwargs = {"json": cancel_body, "timeout": 10}
                            if proxy:
                                req_kwargs["proxies"] = {"http": proxy, "https": proxy}
                            CLIENT._session.delete(cancel_url, **req_kwargs)
                        except Exception:
                            pass

    # MARKET fallback (urgent or maker didn't fill)
    if not maker_filled:
        # Expected MARKET fill price (for slippage baseline)
        api_cur = STATE.api.get(sym)
        if api_cur is not None:
            a_bid, a_ask, _, _ = api_cur
            expected_market = a_bid if pos.side == "buy" else a_ask
        else:
            expected_market = api_mid
        if intended_close_price == 0:
            intended_close_price = expected_market
        try:
            resp = _submit_one_way_order(sym, opp_side, api_mid, pos.size_usd * 2,
                                         reduce_only=True, order_type="ORDER_TYPE_MARKET")
        except Exception as e:
            msg = str(e)[:150]
            print(f"  [CLOSE_ERR] {pos.sym}: {msg}", flush=True)
            return False
        exit_avg, _, _ = _extract_fill(resp)
        if exit_avg <= 0:
            exit_avg = api_mid

    # Log execution: intended vs actual exit
    if exit_avg > 0 and intended_close_price > 0:
        # For a SELL close (long exit), we sold — higher fill is better than intended.
        # For a BUY close (short exit), we bought — lower fill is better than intended.
        if pos.side == "buy":
            slip_bp = (intended_close_price - exit_avg) / intended_close_price * 1e4
        else:
            slip_bp = (exit_avg - intended_close_price) / intended_close_price * 1e4
        _log_exec("close", sym=sym, side=pos.side, kind=("MAKER" if maker_filled else "TAKER"),
                  reason=reason.split("(")[0], intended=round(intended_close_price, 6),
                  actual=round(exit_avg, 6), slip_bp=round(slip_bp, 2))

    real_move_bp = (exit_avg - pos.entry_fill_price) / pos.entry_fill_price * 1e4
    if pos.side == "sell":
        real_move_bp = -real_move_bp
    pnl_usd = (real_move_bp / 1e4) * pos.size_usd
    STATE.cum_pnl_usd += pnl_usd
    STATE.closes += 1
    hold_ms = now_ms() - pos.entry_time_ms
    exit_kind = "MAKER" if maker_filled else "TAKER"
    print(f"  [CLOSE:{exit_kind}] {sym} {pos.side} reason={reason} "
          f"entry={pos.entry_fill_price:.6f} exit={exit_avg:.6f} "
          f"move={real_move_bp:+.2f}bp hold={hold_ms:.0f}ms "
          f"pnl=${pnl_usd:.3f} cum=${STATE.cum_pnl_usd:.2f}", flush=True)
    STATE.positions.pop(sym, None)
    # B. Adaptive cooldown — track closes per-sym and blacklist on loss streaks
    if not pos.adopted:
        record_close_for_blacklist(sym, pnl_usd, reason)
    return True


MINIMAL_FILTER = False  # set via CLI; when True, skip all "noise filters" (direct-mode exp.)


async def on_api_update(sym):
    """Sign-following state machine: diff_bp sign drives desired side per-symbol."""
    if should_stop():
        return
    if sym in HARD_BLACKLIST and not MINIMAL_FILTER:
        return  # historical toxic symbols — never trade (unless minimal mode)
    api = STATE.api.get(sym)
    web = STATE.web.get(sym)
    if api is None or web is None:
        return
    api_bid, api_ask, api_mid, api_ts = api
    web_bid, web_ask, web_ts = web
    web_mid = (web_bid + web_ask) / 2
    if web_mid <= 0:
        return
    # Freshness heartbeat: ticker mid updates at 5Hz per sym; use max of
    # (orderbook ts, ticker ts) as the effective web update time. Without
    # this, illiquid symbols sit stale in orderbook-v2 for 9+ seconds.
    tk = STATE.web_ticker_mid.get(sym)
    eff_web_ts = web_ts if tk is None else max(web_ts, tk[1])
    web_age_ms = api_ts - eff_web_ts
    if web_age_ms < 0 or web_age_ms > MAX_WEB_AGE_MS:
        return
    # Divergence guard (scenario C): if ticker mid diverged significantly
    # from orderbook mid, orderbook bid/ask are stale — can't trust cross-book
    # check. Skip rather than act on fake cross.
    if tk is not None:
        ticker_mid = tk[0]
        if ticker_mid > 0:
            diverge_bp = abs(ticker_mid - web_mid) / web_mid * 1e4
            if diverge_bp > ORDERBOOK_STALE_DIVERGE_BP:
                if STATE.signals % 40 == 0:
                    print(f"  [orderbook-stale skip] {sym} "
                          f"ticker={ticker_mid:.6f} ob_mid={web_mid:.6f} "
                          f"div={diverge_bp:.1f}bp", flush=True)
                _log_rejection(sym, "orderbook_stale", "?", 0.0,
                               diverge_bp=round(diverge_bp, 2),
                               ticker_mid=ticker_mid, ob_mid=web_mid,
                               api_mid=api_mid, web_age_ms=int(web_age_ms))
                return
    web_spread_bp = (web_ask - web_bid) / web_mid * 1e4
    api_spread_bp = (api_ask - api_bid) / api_mid * 1e4 if api_mid > 0 else 999
    # Wide spread = thin liquidity = adverse-selection trap. These are the illiquid coins
    # (PIEVERSE, RAVE, BSB, GUN) where fills come from informed traders dumping at us.
    # Enforced even in --minimal-filter because it's a math/structural guard, not a heuristic.
    if max(web_spread_bp, api_spread_bp) > ENTRY_MAX_SPREAD_BP:
        return

    # --- EMA-smoothed web mid (for diff calc only; orders still use raw web bid/ask) ---
    web_mid_ema = update_web_ema(sym, web_mid, api_ts)

    # --- Cross-book edge trigger (strict: api must have crossed web's opposite side) ---
    # Buy:  api_bid > web_ask  → api is already bidding above web's best ask (web stale short)
    # Sell: api_ask < web_bid  → api is offering below web's best bid (web stale long)
    if api_bid > web_ask:
        cross_dir = +1
        cross_bp = (api_bid - web_ask) / web_mid_ema * 1e4
    elif api_ask < web_bid:
        cross_dir = -1
        cross_bp = (web_bid - api_ask) / web_mid_ema * 1e4
    else:
        cross_dir = 0
        cross_bp = 0.0

    # A. Record every api tick's cross state (persistence history)
    update_cross_hist(sym, api_ts, cross_dir)

    cur = STATE.positions.get(sym)
    # Sign-flip exit REMOVED (proven net-negative). Open only when flat.
    if cur is not None:
        return

    if cross_dir == 0 or cross_bp < MIN_EDGE_BP or cross_bp > MAX_CROSS_BP:
        return  # books not crossed or edge too thin

    desired = "buy" if cross_dir > 0 else "sell"
    this_sign = cross_dir
    diff_bp = cross_bp if desired == "buy" else -cross_bp
    cur_side = None

    # Shadow logging — record every cross-book candidate with filter inputs for backtest
    if _SHADOW_FH is not None:
        # Snapshot state for each filter BEFORE applying any of them
        cp_dq = CROSS_HIST.get(sym)
        cp_recent = list(cp_dq)[-CROSS_PERSIST_COUNT:] if cp_dq else []
        cp_ok = len(cp_recent) >= CROSS_PERSIST_COUNT and all(d == cross_dir for _, d in cp_recent)
        prev_edge = LAST_CROSS_BP.get(sym, 0.0)
        sh_hist = STATE.sig_history.get(sym)
        sh_len_now = len(sh_hist) if sh_hist else 0
        bl_until = SYM_BLACKLIST.get(sym, 0)
        bl_active = bl_until > now_ms()
        bx = _binance_returns_snapshot(sym)
        ticker_mid_v = tk[0] if tk else None
        diverge_bp_v = (abs(ticker_mid_v - web_mid) / web_mid * 1e4) if ticker_mid_v else None
        _log_shadow(
            ts_ms=int(api_ts), sym=sym, side=desired, cross_bp=round(cross_bp, 3),
            api_bid=api_bid, api_ask=api_ask, api_mid=api_mid,
            web_bid=web_bid, web_ask=web_ask, web_mid=web_mid,
            web_age_ms=int(web_age_ms),
            ticker_mid=ticker_mid_v,
            diverge_bp=round(diverge_bp_v, 3) if diverge_bp_v is not None else None,
            cp_hist=[d for _, d in cp_recent],
            cp_ok=cp_ok,
            prev_edge=round(prev_edge, 3),
            sig_hist_len=sh_len_now,
            bl_active=bl_active,
            bl_rem_ms=max(0, int(bl_until - now_ms())),
            **bx,
        )

    # B. Symbol blacklist check — skip if recent loss streak locked this sym
    bl, bl_rem = is_blacklisted(sym)
    if bl and not MINIMAL_FILTER:
        if STATE.signals % 40 == 0:
            print(f"  [blacklist skip] {sym} {desired} {bl_rem/1000:.0f}s left", flush=True)
        _log_rejection(sym, "blacklist", desired, cross_bp, rem_ms=int(bl_rem))
        return

    # A. Cross persistence — DISABLED after 186k-sample backtest: +$155 without it.
    # Previous 18min live regression was confounded by fv bug (now fixed).
    # persist_ok, persist_detail = cross_persisted(sym, cross_dir)
    # if not persist_ok:
    #     _log_rejection(sym, "cross_persist", desired, cross_bp, detail=persist_detail)
    #     return

    # D. Cross edge must be growing or flat (catches dying leads where cross_bp shrinks)
    if CROSS_EDGE_GROWING_CHECK:
        prev_edge = LAST_CROSS_BP.get(sym, 0.0)
        LAST_CROSS_BP[sym] = cross_bp
        if prev_edge > 0 and cross_bp < prev_edge - 0.3:  # shrunk by >0.3bp
            if STATE.signals % 30 == 0:
                print(f"  [edge-shrink skip] {sym} {desired} "
                      f"edge {prev_edge:.1f}→{cross_bp:.1f}bp", flush=True)
            _log_rejection(sym, "edge_shrink", desired, cross_bp, prev_edge=prev_edge)
            return

    if not MINIMAL_FILTER:
        # C. Recent velocity — Binance must still be moving in entry direction
        vel_ok, vel_bp = binance_recent_velocity_ok(sym, desired)
        if not vel_ok:
            if STATE.signals % 30 == 0:
                print(f"  [vel-decay skip] {sym} {desired} "
                      f"recent_{RECENT_VEL_WINDOW_MS}ms={vel_bp:+.1f}bp", flush=True)
            _log_rejection(sym, "vel_decay", desired, cross_bp, recent_bp=round(vel_bp, 2))
            return

        # Sig confirm (2x same-sign)
        t_now = now_ms()
        hist = STATE.sig_history.setdefault(sym, deque(maxlen=SIG_CONFIRM_COUNT))
        while hist and (t_now - hist[0][0]) > SIG_CONFIRM_WINDOW_MS:
            hist.popleft()
        if hist and hist[-1][1] != this_sign:
            hist.clear()
        hist.append((t_now, this_sign))
        if len(hist) < SIG_CONFIRM_COUNT:
            _log_rejection(sym, "sig_confirm", desired, cross_bp, hist_len=len(hist))
            return

    # Monotonicity — DISABLED (backtest: +$141 without it).
    # mono_ok, mono_detail = binance_monotonic(sym, desired)
    # if not mono_ok:
    #     _log_rejection(sym, "mono", desired, cross_bp, detail=mono_detail)
    #     return

    # Volatility gate — DISABLED (backtest: +$151 without it).
    # vol_ok, vol_range_bp = binance_vol_ok(sym)
    # if not vol_ok:
    #     _log_rejection(sym, "vol", desired, cross_bp, range_bp=round(vol_range_bp, 2))
    #     return

    if not MINIMAL_FILTER:
        # Binance momentum filter: need 3/4 timeframes (1s/5s/10s/20s) to agree
        ok, n_agree, detail = binance_momentum_agree(sym, desired)
        if not ok:
            if STATE.signals % 20 == 0:
                print(f"  [bx-filter skip] {sym} {desired} agree={n_agree}/4 | {detail}", flush=True)
            _log_rejection(sym, "bx_momentum", desired, cross_bp,
                           agree=n_agree, detail=detail[:120])
            return

    # Per-symbol lock so concurrent api ticks for same sym don't race
    lock = STATE.sym_lock.setdefault(sym, asyncio.Lock())
    if lock.locked():
        return
    async with lock:
        # Re-read after lock
        cur = STATE.positions.get(sym)
        cur_side = cur.side if cur else None
        if desired == cur_side:
            return
        cd = STATE.sym_cooldown.get(sym, 0)
        if now_ms() < cd:
            return
        if cur is not None and (now_ms() - cur.entry_time_ms) < MIN_HOLD_MS:
            return  # don't flip too soon after opening
        if rate_limited():
            return

        STATE.signals += 1
        ts_str = time.strftime("%H:%M:%S", time.localtime())
        action = ("flip→" + desired if cur_side else "open→" + desired)
        print(f"[{ts_str}] [SIG#{STATE.signals}] {sym} diff={diff_bp:+.2f}bp "
              f"web_age={web_age_ms:.0f}ms cur={cur_side} → {action}", flush=True)

        # Shadow PnL probe — capture Flipster price at future offsets (drift on
        # Flipster itself, not Binance). Write when each offset elapses.
        entry_api_mid = api_mid
        entry_web_mid = web_mid
        sig_num = STATE.signals
        asyncio.create_task(_flipster_drift_probe(sym, desired, sig_num,
                                                   entry_api_mid, entry_web_mid))

        if not LIVE:
            return

        # Transition
        if cur is not None:
            ok = await _close_position(sym, cur, api_mid, reason=f"sign_flip({diff_bp:+.1f})")
            if not ok:
                return
            if desired is not None:
                STATE.flips += 1
        if desired is not None:
            await _open_position(sym, desired, api_mid, api_ask, api_bid,
                                  web_bid, web_ask)
        STATE.sym_cooldown[sym] = now_ms() + FLIP_COOLDOWN_MS


async def exit_monitor():
    """Safety net: fair-value stop (vs web exit-side), confirmation + min-hold-for-stop, TP, max-hold."""
    while not STATE.kill.is_set():
        await asyncio.sleep(0.2)
        if not STATE.positions:
            continue
        for sym in list(STATE.positions.keys()):
            pos = STATE.positions.get(sym)
            if pos is None:
                continue
            age_ms = now_ms() - pos.entry_time_ms
            api = STATE.api.get(pos.sym)
            web = STATE.web.get(pos.sym)
            if api is None:
                continue
            api_mid = api[2]

            # Fair-value P&L using API side (where MARKET close actually fills).
            # Previously used web exit-side, but when web is stale (our exploit window),
            # fv shows "+TP" while api has moved against us → actual MARKET fill is
            # much worse than fv suggests (10-50bp slippage common).
            api_bid_v = api[0]
            api_ask_v = api[1]
            if pos.side == "buy":
                fv_exit = api_bid_v  # closing long: MARKET sell hits api_bid
            else:
                fv_exit = api_ask_v  # closing short: MARKET buy pays api_ask
            move_bp_fv = (fv_exit - pos.entry_fill_price) / pos.entry_fill_price * 1e4
            if pos.side == "sell":
                move_bp_fv = -move_bp_fv

            # Detect bracket TP fill via private WS tracker. When exchange reports
            # position closed (margin=0 / dropped), the bracket LIMIT must have filled.
            # Credit MAKER PnL at tp_target and skip the normal close path.
            if STATE.tracker is not None and STATE.tracker.connected and pos.tp_order_id:
                has_exch_pos = STATE.tracker.has_position(sym)
                if has_exch_pos:
                    pos.tracker_seen = True
                elif pos.tracker_seen:
                    tp_move_bp = (pos.tp_target_price - pos.entry_fill_price) \
                        / pos.entry_fill_price * 1e4
                    if pos.side == "sell":
                        tp_move_bp = -tp_move_bp
                    pnl_usd = (tp_move_bp / 1e4) * pos.size_usd
                    STATE.cum_pnl_usd += pnl_usd
                    STATE.closes += 1
                    hold_ms = age_ms
                    print(f"  [CLOSE:BRACKET_TP] {sym} {pos.side} "
                          f"tp={pos.tp_target_price:.6f} move={tp_move_bp:+.2f}bp "
                          f"hold={hold_ms:.0f}ms pnl=${pnl_usd:.3f} "
                          f"cum=${STATE.cum_pnl_usd:.2f}", flush=True)
                    _log_exec("close", sym=sym, side=pos.side, kind="MAKER",
                              reason="bracket_tp",
                              intended=round(pos.tp_target_price, 8),
                              actual=round(pos.tp_target_price, 8),
                              slip_bp=0.0)
                    STATE.positions.pop(sym, None)
                    STATE.sym_cooldown[sym] = now_ms() + FLIP_COOLDOWN_MS
                    continue

            # TP detection DISABLED in polling — bracket LIMIT on exchange handles TP.
            # Exit_monitor only watches for STOP (adverse) and MAX_HOLD (timeout).
            # Letting the bracket work → mostly MAKER fills, much lower fees.

            reason = None
            if not pos.adopted and move_bp_fv <= -HARD_STOP_BP:
                # Catastrophic adverse move — no confirm/min-hold, close NOW.
                reason = f"hard_stop({move_bp_fv:+.1f}bp)"
            elif (not pos.adopted and age_ms >= TP_FALLBACK_AGE_MS
                  and move_bp_fv >= TP_FALLBACK_MIN_BP):
                # Bracket missed convergence — take any profit at market rather than wait.
                reason = f"tp_fallback({move_bp_fv:+.1f}bp,age={age_ms/1000:.0f}s)"
            elif age_ms >= MAX_HOLD_MS:
                reason = f"max_hold({age_ms/1000:.0f}s,{move_bp_fv:+.1f}bp)"
            elif not pos.adopted and age_ms >= MIN_HOLD_MS_FOR_STOP and move_bp_fv <= -STOP_BP:
                pos.stop_confirm += 1
                if pos.stop_confirm >= STOP_CONFIRM_TICKS:
                    reason = f"stop({move_bp_fv:+.1f}bp,confirm={pos.stop_confirm})"
                else:
                    continue
            else:
                pos.stop_confirm = 0
                continue

            lock = STATE.sym_lock.setdefault(sym, asyncio.Lock())
            if lock.locked():
                continue
            async with lock:
                pos = STATE.positions.get(sym)
                if pos is None:
                    continue
                await _close_position(sym, pos, api_mid, reason=reason)


async def auto_stop_timer(duration_min):
    if duration_min <= 0:
        print(f"[cfg] auto-stop DISABLED (runs until killed)", flush=True)
        return
    await asyncio.sleep(duration_min * 60)
    print(f"[AUTOSTOP] {duration_min}min reached", flush=True)
    STATE.kill.set()


async def status_loop():
    while not STATE.kill.is_set():
        await asyncio.sleep(30)
        elapsed = (now_ms() - STATE.start_ms) / 1000
        tracker_open = len(STATE.tracker.positions) if (STATE.tracker and STATE.tracker.connected) else -1
        print(f"[status t={elapsed:.0f}s] sig={STATE.signals} "
              f"open={STATE.fills} close={STATE.closes} flip={STATE.flips} "
              f"skip={STATE.no_fills} "
              f"open_pos={len(STATE.positions)} tracker={tracker_open} "
              f"cum_pnl=${STATE.cum_pnl_usd:.2f}", flush=True)


async def tracker_adopt_loop():
    """Periodically reconcile tracker positions with local state: adopt orphans (close them)."""
    await asyncio.sleep(5)
    while not STATE.kill.is_set():
        await asyncio.sleep(15)
        if not (STATE.tracker and STATE.tracker.connected):
            continue
        for key, fields in list(STATE.tracker.positions.items()):
            if "/" not in key:
                continue
            sym, slot_s = key.split("/", 1)
            try:
                slot = int(slot_s)
            except ValueError:
                continue
            if sym in STATE.positions:
                continue
            # Orphan — close it (not part of our active state)
            pos_qty = fields.get("position")
            if pos_qty is None:
                continue
            try:
                if float(pos_qty) == 0:
                    continue
            except (TypeError, ValueError):
                continue
            mid = fields.get("midPrice") or fields.get("bidPrice")
            if not mid:
                continue
            try:
                CLIENT.close_position(sym, slot, float(mid))
                print(f"[orphan-close] {sym}/{slot} — not tracked locally", flush=True)
            except Exception as e:
                msg = str(e)[:100]
                if "404" not in msg:
                    print(f"[orphan-close] {sym}/{slot} FAIL: {msg}", flush=True)


async def reconcile_positions_from_flipster(cookies: dict):
    """One-shot private-WS snapshot: adopt any non-zero-qty positions into STATE.positions
    so exit_monitor can TP/max-hold-close them. Entry time = now (fresh 48h clock)."""
    cookie_hdr = "; ".join(f"{k}={v}" for k, v in cookies.items())
    hdrs = [("Origin", "https://flipster.io"), ("Cookie", cookie_hdr),
            ("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
                           "(KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36")]
    url = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"
    snap = {}
    try:
        async with websockets.connect(url, additional_headers=hdrs, max_size=10_000_000) as ws:
            await ws.send(json.dumps({"s": {"private/positions": {"rows": ["*"]}}}))
            end = time.time() + 6
            while time.time() < end:
                try:
                    raw = await asyncio.wait_for(ws.recv(), timeout=1)
                except asyncio.TimeoutError:
                    continue
                d = json.loads(raw)
                pos = d.get("t", {}).get("private/positions", {})
                for sect in ("s", "u"):
                    for k, row in pos.get(sect, {}).items():
                        snap.setdefault(k, {}).update(row)
    except Exception as e:
        print(f"[reconcile] snapshot FAIL: {e}", flush=True)
        return
    adopted = 0
    for key, f in snap.items():
        if "/" not in key:
            continue
        sym, slot_s = key.split("/", 1)
        try:
            slot = int(slot_s)
        except ValueError:
            continue
        try:
            qty = float(f.get("position", 0) or 0)
        except (TypeError, ValueError):
            qty = 0
        if qty == 0:
            continue
        try:
            entry = float(f.get("avgEntryPrice") or 0)
        except (TypeError, ValueError):
            entry = 0
        if entry <= 0:
            continue
        side = "buy" if qty > 0 else "sell"
        size_usd = abs(qty) * entry
        STATE.positions[sym] = Position(
            sym=sym, side=side, size_usd=size_usd,
            entry_mid_api=entry, entry_fill_price=entry,
            entry_time_ms=now_ms(), slot=slot,
            adopted=True,
        )
        adopted += 1
        print(f"[reconcile] adopted {sym} {side} qty={qty} entry={entry} size=${size_usd:.2f}", flush=True)
    print(f"[reconcile] adopted {adopted} existing positions; total tracked: {len(STATE.positions)}", flush=True)


async def main():
    global AMOUNT_USD, KILL_USD, MIN_EDGE_BP, FLAT_BP, MAX_WEB_AGE_MS, MAX_HOLD_MS
    global STOP_BP, RATE_LIMIT_PER_MIN, CLIENT, LIVE

    ap = argparse.ArgumentParser()
    ap.add_argument("--symbols", default="ALL",
        help="'ALL' to load every Binance/Flipster overlap sym from symbols_v9.json, "
             "or comma-separated bases.")
    ap.add_argument("--amount-usd", type=float, default=10.0)
    ap.add_argument("--kill-usd", type=float, default=200.0)
    ap.add_argument("--duration-min", type=float, default=0,
                    help="Auto-stop timer in minutes. 0 or negative = run forever (until killed).")
    ap.add_argument("--cdp-port", type=int, default=9230)
    ap.add_argument("--min-edge-bp", type=float, default=MIN_EDGE_BP)
    ap.add_argument("--flat-bp", type=float, default=2.0)
    ap.add_argument("--max-web-age-ms", type=float, default=MAX_WEB_AGE_MS)
    ap.add_argument("--stop-bp", type=float, default=STOP_BP_DEFAULT)
    ap.add_argument("--max-hold-ms", type=float, default=MAX_HOLD_MS)
    ap.add_argument("--rate-limit-per-min", type=int, default=40)
    ap.add_argument("--proxies-file", type=str, default="scripts/proxies.txt",
                    help="Path to proxies.txt (ip:port:user:pass per line). Empty to disable.")
    ap.add_argument("--proxy-url", type=str, default="",
                    help="Override proxy URL directly (http://user:pass@host:port). "
                         "If empty, first entry of --proxies-file is used.")
    ap.add_argument("--ws-proxy-url", type=str, default="",
                    help="Separate proxy URL for WS connections (when HTTP proxy IP got WS-banned).")
    ap.add_argument("--leverage", type=int, default=50,
                    help="Per-order leverage (Rust/v11 style defaults to 50x; caps at Flipster per-sym max).")
    ap.add_argument("--reject-log", type=str, default="",
                    help="Path to jsonl file. If set, each filter rejection is appended for post-hoc analysis.")
    ap.add_argument("--shadow-log", type=str, default="",
                    help="Path to jsonl file. If set, every cross-book candidate is logged with "
                         "all filter inputs. Enables offline backtest sweeps.")
    ap.add_argument("--exec-log", type=str, default="",
                    help="Path to jsonl file. Logs every open/close with intended vs actual "
                         "fill price → slippage diagnostic.")
    ap.add_argument("--drift-log", type=str, default="",
                    help="Path to jsonl. For each SIG, logs Flipster api/web price at "
                         f"offsets {DRIFT_OFFSETS_S}s → Flipster-native drift measurement.")
    ap.add_argument("--minimal-filter", action="store_true",
                    help="Skip all noise filters (blacklist, vel_decay, sig_confirm, bx_momentum). "
                         "Direct-mode experiment: any api-web cross fires signal immediately.")
    grp = ap.add_mutually_exclusive_group(required=True)
    grp.add_argument("--live", action="store_true",
                     help="REAL orders. Cannot combine with --dry-run.")
    grp.add_argument("--dry-run", action="store_true",
                     help="Log signals only, no orders.")
    args = ap.parse_args()

    AMOUNT_USD = args.amount_usd
    KILL_USD = args.kill_usd
    MIN_EDGE_BP = args.min_edge_bp
    FLAT_BP = args.flat_bp
    MAX_WEB_AGE_MS = args.max_web_age_ms
    MAX_HOLD_MS = args.max_hold_ms
    STOP_BP = args.stop_bp
    RATE_LIMIT_PER_MIN = args.rate_limit_per_min
    LIVE = args.live

    global _REJECT_FH, _SHADOW_FH, _EXEC_FH, _DRIFT_FH, MINIMAL_FILTER
    MINIMAL_FILTER = args.minimal_filter
    if MINIMAL_FILTER:
        print("[cfg] MINIMAL FILTER mode — only MIN_EDGE_BP + MAX_WEB_AGE_MS", flush=True)
    if args.reject_log:
        _REJECT_FH = open(args.reject_log, "a", buffering=1)  # line-buffered
        print(f"[cfg] rejection log → {args.reject_log}", flush=True)
    if args.shadow_log:
        _SHADOW_FH = open(args.shadow_log, "a", buffering=1)
        print(f"[cfg] shadow log → {args.shadow_log}", flush=True)
    if args.exec_log:
        _EXEC_FH = open(args.exec_log, "a", buffering=1)
        print(f"[cfg] exec log → {args.exec_log}", flush=True)
    if args.drift_log:
        _DRIFT_FH = open(args.drift_log, "a", buffering=1)
        print(f"[cfg] drift log → {args.drift_log}", flush=True)

    if args.symbols.strip().upper() == "ALL":
        syms_path = Path(__file__).resolve().parent / "symbols_v9.json"
        bases = [d["base"] for d in json.loads(syms_path.read_text())]
        print(f"[cfg] using ALL {len(bases)} overlap symbols from symbols_v9.json", flush=True)
    else:
        bases = [s.strip().upper() for s in args.symbols.split(",") if s.strip()]
    symbols = [f"{b}USDT.PERP" for b in bases]

    STATE.start_ms = now_ms()

    print(f"[cfg] mode={'LIVE' if LIVE else 'DRY-RUN'}", flush=True)
    print(f"[cfg] symbols={symbols}", flush=True)
    print(f"[cfg] amount=${AMOUNT_USD} kill=-${KILL_USD} duration={args.duration_min}min", flush=True)
    print(f"[cfg] min_edge_bp={MIN_EDGE_BP} max_web_age_ms={MAX_WEB_AGE_MS} "
          f"max_hold_ms={MAX_HOLD_MS} stop_bp={STOP_BP} flat_bp={FLAT_BP}", flush=True)
    print(f"[cfg] rate_limit_per_min={RATE_LIMIT_PER_MIN}", flush=True)

    global LEVERAGE
    LEVERAGE = args.leverage
    _load_sym_max_leverage()
    # Resolve proxies: HTTP pool (rotated per order) + WS single proxy
    global _PROXY_URL, _WS_PROXY_URL, _HTTP_POOL
    pool = []
    if args.proxies_file:
        pf = Path(args.proxies_file)
        if pf.exists():
            for line in pf.read_text().splitlines():
                line = line.strip()
                if not line or line.startswith("#"):
                    continue
                ip, port, user, pwd = line.split(":")
                pool.append(f"http://{user}:{pwd}@{ip}:{port}")
    if args.proxy_url:
        pool = [args.proxy_url] + [p for p in pool if p != args.proxy_url]
    _HTTP_POOL = pool
    if _HTTP_POOL:
        _PROXY_URL = _HTTP_POOL[0]  # fallback
    if args.ws_proxy_url:
        _WS_PROXY_URL = args.ws_proxy_url
    elif _HTTP_POOL:
        # Default WS proxy = same as cookie-issuing IP
        # (user logged in via proxy_chain's upstream, usually proxies.txt[0] or [1])
        _WS_PROXY_URL = _HTTP_POOL[0]
    print(f"[cfg] http_pool={len(_HTTP_POOL)} proxies (rotating per order)", flush=True)
    if _WS_PROXY_URL:
        print(f"[cfg] ws_proxy={_WS_PROXY_URL.split('@')[-1]}", flush=True)

    if LIVE:
        from curl_cffi import requests as cc_requests
        from flipster_pkg_lp.browser import BrowserManager as _BM
        from flipster_pkg_lp.client import _HEADERS
        bm = _BM(chrome_debug_port=args.cdp_port)
        bm._running = True
        CLIENT = FlipsterClient(browser=bm)
        cdp_ws_url = bm.cdp_ws_url
        cookies = await _extract_cookies(cdp_ws_url)
        if "session_id_bolts" not in cookies:
            raise RuntimeError("No Flipster session — log in via VNC first")
        CLIENT._cookies = cookies
        # curl_cffi.Session: NO session-wide proxy — each .post() picks a proxy from pool
        CLIENT._session = cc_requests.Session(impersonate="chrome131")
        for k, v in cookies.items():
            CLIENT._session.cookies.set(k, v)
        chrome_ver = json.loads(urllib.request.urlopen(
            f"http://localhost:{args.cdp_port}/json/version", timeout=5).read())
        real_ua = chrome_ver.get("User-Agent",
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36")
        headers = dict(_HEADERS)
        headers["User-Agent"] = real_ua
        headers["sec-ch-ua"] = '"Chromium";v="141", "Not?A_Brand";v="24", "Google Chrome";v="141"'
        headers["sec-ch-ua-mobile"] = "?0"
        headers["sec-ch-ua-platform"] = '"Linux"'
        CLIENT._session.headers.update(headers)
        print(f"[client] curl_cffi chrome131 impersonation, UA={real_ua[:70]}...", flush=True)
        print(f"[client] attached to Chrome:{args.cdp_port}, {len(cookies)} cookies", flush=True)

    # Private positions WS tracker (v11-style) — ground truth for open positions
    if LIVE:
        STATE.tracker = WSPositionTracker(cookies=cookies)
        await reconcile_positions_from_flipster(cookies)

    async def signal_cb(sym):
        await on_api_update(sym)

    tasks = [
        asyncio.create_task(web_feed(symbols, args.cdp_port)),
        asyncio.create_task(api_feed(symbols, signal_cb)),
        asyncio.create_task(binance_zmq_feed()),
        asyncio.create_task(auto_stop_timer(args.duration_min)),
        asyncio.create_task(status_loop()),
    ]
    if LIVE:
        tasks.append(asyncio.create_task(exit_monitor()))
        if STATE.tracker is not None:
            tasks.append(asyncio.create_task(
                STATE.tracker.run(lambda m: print(m, flush=True))))

    await STATE.kill.wait()
    for t in tasks:
        t.cancel()

    print(f"\n=== FINAL ===", flush=True)
    print(f"  signals={STATE.signals}  opens={STATE.fills}  closes={STATE.closes}  "
          f"flips={STATE.flips}  skips={STATE.no_fills}", flush=True)
    print(f"  trades={STATE.trade_count}  cum_pnl=${STATE.cum_pnl_usd:.2f}", flush=True)

    # Cleanup all open positions via one-way close
    if LIVE and CLIENT is not None and STATE.positions:
        print(f"[cleanup] closing {len(STATE.positions)} open positions", flush=True)
        for sym, pos in list(STATE.positions.items()):
            api_mid = STATE.api.get(sym, (0, 0, 0, 0))[2] or pos.entry_fill_price
            opp = "Short" if pos.side == "buy" else "Long"
            try:
                _submit_one_way_order(sym, opp, api_mid, pos.size_usd * 2,
                                      reduce_only=True, order_type="ORDER_TYPE_MARKET")
                print(f"  closed {sym}", flush=True)
            except Exception as e:
                print(f"  FAIL {sym}: {str(e)[:100]}", flush=True)

    if CLIENT is not None:
        # Don't CLIENT.stop() — would terminate the user's externally-managed browser.
        if CLIENT._session is not None:
            CLIENT._session.close()


if __name__ == "__main__":
    if not KEY or not SEC:
        sys.exit("FLIPSTER_API_KEY / FLIPSTER_API_SECRET not set")
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        print("\n[ctrl-c] killing", flush=True)
        STATE.kill.set()

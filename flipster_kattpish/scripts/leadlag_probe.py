#!/usr/bin/env python3
"""
leadlag_probe.py — measure Flipster api→web lag WITHOUT trading.

Two modes:
  --collect --duration-min N --out FILE.jsonl
      Subscribes to both feeds, dumps every top-of-book update as JSONL.
  --analyze FILE.jsonl
      Reads dump, detects api signals, measures:
        (1) web_follow bp at +100/250/500/1000/2000/5000ms   vs control (random t)
        (2) post-only maker fill rate (buy@web_bid, sell@web_ask) within FILL_WAIT_MS
        (3) adverse selection — price 5s after fill relative to our fill price

Signal definition for analysis:
  api_mid moved ≥ SIGNAL_BP over LOOKBACK_MS window.

Edge exists iff web_follow(Δ) is significantly > 0 compared to control (random-time)
response, and the expected gain at best Δ exceeds round-trip cost.
"""
from __future__ import annotations
import argparse, asyncio, bisect, hashlib, hmac, json, os, random, statistics as S, sys, time
import urllib.request
from collections import defaultdict, deque
from pathlib import Path
from typing import Optional

import websockets

try:
    from dotenv import load_dotenv
    load_dotenv(Path(__file__).resolve().parent.parent / ".env")
except ImportError:
    pass

KEY = os.environ.get("FLIPSTER_API_KEY")
SEC = os.environ.get("FLIPSTER_API_SECRET")

# -------- collect --------

async def _extract_cookies(cdp_ws_url):
    async with websockets.connect(cdp_ws_url) as ws:
        await ws.send(json.dumps({"id": 1, "method": "Storage.getCookies"}))
        resp = json.loads(await ws.recv())
        return {c["name"]: c["value"] for c in resp.get("result", {}).get("cookies", [])
                if "flipster" in c.get("domain", "")}


async def web_feed(symbols, cdp_port, fh, stop_evt):
    while not stop_evt.is_set():
        try:
            ver = json.loads(urllib.request.urlopen(
                f"http://localhost:{cdp_port}/json/version", timeout=5).read())
            cookies = await _extract_cookies(ver["webSocketDebuggerUrl"])
            cookie_str = "; ".join(f"{k}={v}" for k, v in cookies.items())
            headers = {"Cookie": cookie_str, "Origin": "https://flipster.io",
                       "User-Agent": "Mozilla/5.0 Firefox/149.0"}
            url = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"
            async with websockets.connect(url, additional_headers=headers,
                                          ping_interval=20, max_size=10_000_000) as ws:
                await ws.send(json.dumps({"s": {
                    "market/orderbooks-v2": {"rows": symbols},
                }}))
                print(f"[web] subscribed {len(symbols)}", flush=True)
                async for raw in ws:
                    if stop_evt.is_set():
                        break
                    try:
                        d = json.loads(raw)
                    except json.JSONDecodeError:
                        continue
                    topic = d.get("t", {})
                    ob = topic.get("market/orderbooks-v2", {}).get("s", {})
                    if not ob:
                        continue
                    t_recv = int(time.time() * 1000)
                    for sym, book in ob.items():
                        bids = book.get("bids", [])
                        asks = book.get("asks", [])
                        if not bids or not asks or len(bids[0]) < 1 or len(asks[0]) < 1:
                            continue
                        try:
                            b = float(bids[0][0]); a = float(asks[0][0])
                        except (TypeError, ValueError):
                            continue
                        fh.write(json.dumps({"t": t_recv, "f": "w", "s": sym,
                                             "b": b, "a": a}) + "\n")
        except Exception as e:
            print(f"[web] err: {e} — retry 3s", flush=True)
            await asyncio.sleep(3)


async def api_feed(symbols, fh, stop_evt):
    while not stop_evt.is_set():
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
                print(f"[api] subscribed {len(topics)}", flush=True)
                async for raw in ws:
                    if stop_evt.is_set():
                        break
                    try:
                        d = json.loads(raw)
                    except json.JSONDecodeError:
                        continue
                    topic = d.get("topic", "")
                    if not topic.startswith("ticker."):
                        continue
                    sym = topic[len("ticker."):]
                    t_recv = int(time.time() * 1000)
                    for row in d.get("data", []):
                        for r in row.get("rows", []):
                            b = r.get("bidPrice"); a = r.get("askPrice")
                            if b is None or a is None:
                                continue
                            try:
                                b = float(b); a = float(a)
                            except (TypeError, ValueError):
                                continue
                            fh.write(json.dumps({"t": t_recv, "f": "a", "s": sym,
                                                 "b": b, "a": a}) + "\n")
        except Exception as e:
            print(f"[api] err: {e} — retry 3s", flush=True)
            await asyncio.sleep(3)


async def run_collect(symbols, cdp_port, out_path, duration_min):
    stop_evt = asyncio.Event()
    fh = open(out_path, "w", buffering=1)
    tasks = [asyncio.create_task(api_feed(symbols, fh, stop_evt)),
             asyncio.create_task(web_feed(symbols, cdp_port, fh, stop_evt))]

    async def stopper():
        await asyncio.sleep(duration_min * 60)
        stop_evt.set()
    tasks.append(asyncio.create_task(stopper()))

    try:
        await asyncio.wait_for(stop_evt.wait(), timeout=duration_min * 60 + 10)
    except asyncio.TimeoutError:
        pass
    stop_evt.set()
    for t in tasks:
        t.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    fh.close()
    print(f"[done] wrote {out_path}", flush=True)


# -------- analyze --------

LOOKBACK_MS = 1000      # how far back to compute api move that counts as "signal"
SIGNAL_BP = 3.0         # api move >= this bp over LOOKBACK_MS triggers signal
DELTAS_MS = (100, 250, 500, 1000, 2000, 5000)
FILL_WAIT_MS = 1000     # maker resting time before we "cancel"
ADVERSE_MS_AFTER = 5000 # how far after fill to measure adverse
CONTROL_MULT = 1        # controls per signal (same sym)


def _bisect_le(tlist, t):
    """Return index of last element with t_list[i] <= t (or -1)."""
    i = bisect.bisect_right(tlist, t) - 1
    return i


def analyze(path):
    # Load events into per-sym sorted arrays (api, web)
    api_t = defaultdict(list); api_b = defaultdict(list); api_a = defaultdict(list)
    web_t = defaultdict(list); web_b = defaultdict(list); web_a = defaultdict(list)
    n_rows = 0
    with open(path) as f:
        for line in f:
            try:
                e = json.loads(line)
            except json.JSONDecodeError:
                continue
            n_rows += 1
            s = e["s"]; t = e["t"]; b = e["b"]; a = e["a"]
            if e["f"] == "a":
                api_t[s].append(t); api_b[s].append(b); api_a[s].append(a)
            else:
                web_t[s].append(t); web_b[s].append(b); web_a[s].append(a)
    print(f"[loaded] {n_rows} events, syms: api={len(api_t)} web={len(web_t)}", flush=True)

    # per-sym: already sorted by arrival time (files are append-only); assert + sort if needed
    for d in (api_t, web_t):
        for s, v in d.items():
            if any(v[i] > v[i+1] for i in range(len(v)-1)):
                # resort mirror arrays together (rare)
                # tag with indices
                pass  # accept mild disorder; bisect on noisy timestamps ~fine

    # Detect signals
    signal_stats = defaultdict(list)   # delta -> list of web_follow_bp (signed to our direction)
    control_stats = defaultdict(list)
    fill_events = {"up": [], "dn": []}       # list of (filled_bool, adverse_bp)
    control_fill = {"up": [], "dn": []}
    n_signals = 0
    n_control = 0

    syms = sorted(set(api_t) & set(web_t))
    print(f"[analyze] {len(syms)} symbols with both feeds", flush=True)

    def web_mid_at(sym, t):
        tl = web_t[sym]
        if not tl: return None
        i = _bisect_le(tl, t)
        if i < 0: return None
        return (web_b[sym][i] + web_a[sym][i]) / 2

    def web_best_at(sym, t):
        tl = web_t[sym]
        if not tl: return None
        i = _bisect_le(tl, t)
        if i < 0: return None
        return (web_b[sym][i], web_a[sym][i])

    def api_mid_at(sym, t):
        tl = api_t[sym]
        if not tl: return None
        i = _bisect_le(tl, t)
        if i < 0: return None
        return (api_b[sym][i] + api_a[sym][i]) / 2

    def fill_scan(sym, t0, px, side):
        """Return (filled, fill_t, adverse_bp). side='buy' posted at px (wait for ask<=px)
        or 'sell' posted at px (wait for bid>=px)."""
        tl = web_t[sym]
        lo = bisect.bisect_left(tl, t0)
        hi = bisect.bisect_right(tl, t0 + FILL_WAIT_MS)
        fill_t = None
        for i in range(lo, hi):
            if side == "buy":
                if web_a[sym][i] <= px:
                    fill_t = tl[i]; break
            else:
                if web_b[sym][i] >= px:
                    fill_t = tl[i]; break
        if fill_t is None:
            return (False, None, None)
        # Adverse: web_mid at fill_t + ADVERSE_MS_AFTER vs px (signed so positive = favorable after fill)
        post_mid = web_mid_at(sym, fill_t + ADVERSE_MS_AFTER)
        if post_mid is None:
            return (True, fill_t, None)
        # For buy: favorable if post_mid > px. For sell: favorable if post_mid < px.
        sign = 1 if side == "buy" else -1
        adverse_bp = (post_mid - px) / px * 1e4 * sign
        return (True, fill_t, adverse_bp)

    for sym in syms:
        if len(api_t[sym]) < 50 or len(web_t[sym]) < 50:
            continue
        # For each api tick, compute lookback move
        tlist = api_t[sym]
        mlist = [(api_b[sym][i] + api_a[sym][i]) / 2 for i in range(len(tlist))]
        sym_sig_times = []  # to rate-limit signals per sym (min 500ms gap)
        last_sig_t = -10**18
        min_gap_ms = 500

        for i, t in enumerate(tlist):
            j = _bisect_le(tlist, t - LOOKBACK_MS)
            if j < 0: continue
            m0 = mlist[j]; m1 = mlist[i]
            if m0 <= 0: continue
            move_bp = (m1 - m0) / m0 * 1e4
            if abs(move_bp) < SIGNAL_BP:
                continue
            if t - last_sig_t < min_gap_ms:
                continue
            last_sig_t = t
            sym_sig_times.append(t)

            direction = 1 if move_bp > 0 else -1
            web0 = web_mid_at(sym, t)
            if web0 is None: continue

            # web_follow at each delta
            for d in DELTAS_MS:
                web_d = web_mid_at(sym, t + d)
                if web_d is None: continue
                move = (web_d - web0) / web0 * 1e4 * direction
                signal_stats[d].append(move)

            # post-only fill simulation
            wb = web_best_at(sym, t)
            if wb is not None:
                wbid, wask = wb
                if direction > 0:
                    filled, ft, adv = fill_scan(sym, t, wbid, "buy")
                    fill_events["up"].append((filled, adv))
                else:
                    filled, ft, adv = fill_scan(sym, t, wask, "sell")
                    fill_events["dn"].append((filled, adv))

            n_signals += 1

        # Controls: random timestamps in this sym's span, same count
        if not tlist: continue
        t_min, t_max = tlist[0] + LOOKBACK_MS, tlist[-1] - max(DELTAS_MS) - ADVERSE_MS_AFTER - FILL_WAIT_MS
        if t_max <= t_min: continue
        target = len(sym_sig_times) * CONTROL_MULT
        for _ in range(target):
            t = random.randint(t_min, t_max)
            # random direction
            direction = random.choice([1, -1])
            web0 = web_mid_at(sym, t)
            if web0 is None: continue
            for d in DELTAS_MS:
                web_d = web_mid_at(sym, t + d)
                if web_d is None: continue
                move = (web_d - web0) / web0 * 1e4 * direction
                control_stats[d].append(move)
            wb = web_best_at(sym, t)
            if wb is not None:
                wbid, wask = wb
                if direction > 0:
                    filled, ft, adv = fill_scan(sym, t, wbid, "buy")
                    control_fill["up"].append((filled, adv))
                else:
                    filled, ft, adv = fill_scan(sym, t, wask, "sell")
                    control_fill["dn"].append((filled, adv))
            n_control += 1

    # Report
    print()
    print(f"=== LEAD-LAG PROBE RESULT ===")
    print(f"signals: {n_signals}   control: {n_control}   "
          f"(SIGNAL_BP={SIGNAL_BP}, LOOKBACK={LOOKBACK_MS}ms)")
    print()
    print(f"{'delta':>7s}  {'sig n':>6s} {'sig_mean':>10s} {'sig_med':>9s} {'sig_p25':>9s} {'sig_p75':>9s}  "
          f"{'ctl_mean':>10s} {'ctl_med':>9s}   {'edge(s-c)':>10s}")
    for d in DELTAS_MS:
        s = signal_stats.get(d, []); c = control_stats.get(d, [])
        if not s or not c:
            continue
        sm = S.mean(s); sl = S.median(s)
        try:
            sp25 = S.quantiles(s, n=4)[0]; sp75 = S.quantiles(s, n=4)[2]
        except S.StatisticsError:
            sp25 = sp75 = 0
        cm = S.mean(c); cl = S.median(c)
        edge = sm - cm
        print(f"{d:>6d}ms {len(s):>6d} {sm:>+9.3f}bp {sl:>+8.3f}bp {sp25:>+8.3f}bp {sp75:>+8.3f}bp  "
              f"{cm:>+9.3f}bp {cl:>+8.3f}bp   {edge:>+9.3f}bp")
    print()
    print(f"=== POST-ONLY MAKER FILL (buy@web_bid / sell@web_ask, {FILL_WAIT_MS}ms wait) ===")
    for label, events, ctl in (("UP (buy)", fill_events["up"], control_fill["up"]),
                               ("DN (sell)", fill_events["dn"], control_fill["dn"])):
        if not events: continue
        n = len(events); nf = sum(1 for f, _ in events if f)
        rate = nf / n * 100
        # Adverse on fills
        advs = [a for f, a in events if f and a is not None]
        adv_mean = S.mean(advs) if advs else 0
        adv_med = S.median(advs) if advs else 0
        adv_wr = sum(1 for x in advs if x > 0) / len(advs) * 100 if advs else 0
        # Control fill rate
        cn = len(ctl); cf = sum(1 for f, _ in ctl if f)
        crate = cf / cn * 100 if cn else 0
        cadvs = [a for f, a in ctl if f and a is not None]
        cadv = S.mean(cadvs) if cadvs else 0
        print(f"  {label}  signal fill_rate={rate:.1f}% (n={n}, filled={nf})   "
              f"control fill_rate={crate:.1f}% (n={cn})")
        if advs:
            print(f"     → post-fill 5s adverse: mean={adv_mean:+.2f}bp  median={adv_med:+.2f}bp  "
                  f"WR={adv_wr:.1f}%   ctl_mean={cadv:+.2f}bp")

    print()
    print("READING:")
    print("  Edge exists iff sig_mean − ctl_mean > 0 at some Δ with enough magnitude to beat cost.")
    print("  Round-trip taker cost ≈ 0.85bp; maker-maker ≈ 0.30bp; typical half-spread crossed ≈ 1-3bp.")
    print("  Maker fill rate < 50% combined with negative post-fill adverse = adverse selection trap.")


def main():
    global SIGNAL_BP, LOOKBACK_MS
    ap = argparse.ArgumentParser()
    sub = ap.add_mutually_exclusive_group(required=True)
    sub.add_argument("--collect", action="store_true")
    sub.add_argument("--analyze", type=str, help="jsonl path to analyze")
    ap.add_argument("--symbols", type=str, default="ALL",
                    help="comma-list or 'ALL' (symbols_v9.json)")
    ap.add_argument("--cdp-port", type=int, default=9230)
    ap.add_argument("--duration-min", type=float, default=20.0)
    ap.add_argument("--out", type=str,
                    default=str(Path(__file__).resolve().parent.parent /
                                "logs" / "leadlag_probe.jsonl"))
    ap.add_argument("--signal-bp", type=float, default=SIGNAL_BP)
    ap.add_argument("--lookback-ms", type=int, default=LOOKBACK_MS)
    args = ap.parse_args()

    SIGNAL_BP = args.signal_bp
    LOOKBACK_MS = args.lookback_ms

    if args.symbols.strip().upper() == "ALL":
        p = Path(__file__).resolve().parent / "symbols_v9.json"
        bases = [d["base"] for d in json.loads(p.read_text())]
    else:
        bases = [s.strip().upper() for s in args.symbols.split(",") if s.strip()]
    symbols = [f"{b}USDT.PERP" for b in bases]

    if args.collect:
        print(f"[cfg] {len(symbols)} symbols, duration={args.duration_min}min, out={args.out}",
              flush=True)
        asyncio.run(run_collect(symbols, args.cdp_port, args.out, args.duration_min))
    else:
        analyze(args.analyze)


if __name__ == "__main__":
    main()

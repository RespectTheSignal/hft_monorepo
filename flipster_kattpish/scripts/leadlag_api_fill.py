#!/usr/bin/env python3
"""leadlag_api_fill.py — REALISTIC fill model.

Previous probe assumed we fill at web's stale top-of-book. Wrong — Flipster's
matching engine already moved to api-side price. Real TAKER fill = api_ask (buy)
or api_bid (sell) at signal time.

Model:
  At cross-book signal t (api_bid > web_ask for buy, api_ask < web_bid for sell):
  - Entry px_in = api_ask(t+delay)  [buy]  or  api_bid(t+delay)  [sell]
  - Exit  px_out = api_bid(t+delay+hold)  [buy close]  or  api_ask(t+delay+hold)  [sell close]
  - Net = gross - 2 × taker_fee_bp

This matches what the actual bot does (line 931: taker_price = api_ask).
"""
from __future__ import annotations
import argparse, bisect, json, statistics as S
from collections import defaultdict
from pathlib import Path


def _bisect_le(tlist, t):
    return bisect.bisect_right(tlist, t) - 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("jsonl")
    ap.add_argument("--min-cross-bp", type=float, default=1.0)
    ap.add_argument("--min-gap-ms", type=int, default=500)
    ap.add_argument("--hold-ms", type=int, default=5000)
    ap.add_argument("--taker-fee-bp", type=float, default=0.425)
    ap.add_argument("--max-spread-bp", type=float, default=6.0)
    args = ap.parse_args()

    api_t = defaultdict(list); api_b = defaultdict(list); api_a = defaultdict(list)
    web_t = defaultdict(list); web_b = defaultdict(list); web_a = defaultdict(list)
    with open(args.jsonl) as f:
        for line in f:
            try: e = json.loads(line)
            except: continue
            s = e["s"]; t = e["t"]; b = e["b"]; a = e["a"]
            if e["f"] == "a":
                api_t[s].append(t); api_b[s].append(b); api_a[s].append(a)
            else:
                web_t[s].append(t); web_b[s].append(b); web_a[s].append(a)

    delays = [0, 50, 100, 150, 200, 300, 500, 1000]
    buckets = {d: [] for d in delays}
    n_events = 0

    syms = sorted(set(api_t) & set(web_t))
    for sym in syms:
        if len(api_t[sym]) < 50 or len(web_t[sym]) < 50:
            continue
        atl = api_t[sym]; ab = api_b[sym]; aa = api_a[sym]
        wtl = web_t[sym]; wb = web_b[sym]; wa = web_a[sym]
        last_sig_t = -10**18
        for i, t in enumerate(atl):
            j = _bisect_le(wtl, t)
            if j < 0: continue
            wbid = wb[j]; wask = wa[j]
            abid = ab[i]; aask = aa[i]
            if wask <= 0 or wbid <= 0 or abid <= 0 or aask <= 0: continue
            web_mid0 = (wbid + wask) / 2
            api_mid0 = (abid + aask) / 2
            if web_mid0 <= 0 or api_mid0 <= 0: continue

            # Spread filter (matches bot)
            web_sp = (wask - wbid) / web_mid0 * 1e4
            api_sp = (aask - abid) / api_mid0 * 1e4
            if max(web_sp, api_sp) > args.max_spread_bp:
                continue

            cross_up = (abid - wask) / web_mid0 * 1e4
            cross_dn = (wbid - aask) / web_mid0 * 1e4
            side = None
            if cross_up >= args.min_cross_bp and t - last_sig_t > args.min_gap_ms:
                side = "buy"; cross_bp = cross_up
            elif cross_dn >= args.min_cross_bp and t - last_sig_t > args.min_gap_ms:
                side = "sell"; cross_bp = cross_dn
            if side is None: continue
            last_sig_t = t
            n_events += 1

            for delay in delays:
                # Entry at api_ask (buy) or api_bid (sell) at t+delay
                ai_entry = _bisect_le(atl, t + delay)
                if ai_entry < 0: continue
                if side == "buy":
                    px_in = aa[ai_entry]  # fill at current api_ask
                else:
                    px_in = ab[ai_entry]  # fill at current api_bid
                # Exit at api_bid (for buy) or api_ask (for sell) after hold
                ai_exit = _bisect_le(atl, t + delay + args.hold_ms)
                if ai_exit < 0 or ai_exit == ai_entry: continue
                if side == "buy":
                    px_out = ab[ai_exit]
                    gross_bp = (px_out - px_in) / px_in * 1e4
                else:
                    px_out = aa[ai_exit]
                    gross_bp = (px_in - px_out) / px_in * 1e4
                net_bp = gross_bp - 2 * args.taker_fee_bp
                buckets[delay].append({
                    "cross_bp": cross_bp, "gross": gross_bp, "net": net_bp,
                    "api_sp": api_sp, "web_sp": web_sp, "side": side,
                })

    print(f"[events] n={n_events}  (min_cross={args.min_cross_bp}, "
          f"hold={args.hold_ms}ms, fee={args.taker_fee_bp}bp × 2)")
    print()
    print(f"{'delay':>7s}  {'n':>6s}  {'gross_mn':>9s}  {'net_mn':>9s}  {'net_med':>9s}  "
          f"{'WR_net':>7s}")
    for d in delays:
        ev = buckets[d]
        if not ev: continue
        gross = [e["gross"] for e in ev]
        net = [e["net"] for e in ev]
        wr = sum(1 for x in net if x > 0) / len(net) * 100
        print(f"{d:>6d}ms  {len(ev):>6d}  {S.mean(gross):>+8.2f}bp  {S.mean(net):>+8.2f}bp  "
              f"{S.median(net):>+8.2f}bp  {wr:>6.1f}%")

    print()
    print("=== BY cross_bp bucket (at delay=0ms) ===")
    ev0 = buckets[0]
    for lo, hi in [(1, 2), (2, 3), (3, 5), (5, 10), (10, 20), (20, 999)]:
        b = [e for e in ev0 if lo <= e["cross_bp"] < hi]
        if not b: continue
        gs = [e["gross"] for e in b]
        ns = [e["net"] for e in b]
        asp = [e["api_sp"] for e in b]
        wr = sum(1 for x in ns if x > 0) / len(ns) * 100
        print(f"  cross [{lo},{hi}): n={len(b):5d}  gross={S.mean(gs):+.2f}bp  "
              f"net={S.mean(ns):+.2f}bp  WR={wr:.1f}%  avg_api_sp={S.mean(asp):.2f}bp")


if __name__ == "__main__":
    main()

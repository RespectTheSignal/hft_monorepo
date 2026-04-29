#!/usr/bin/env python3
"""leadlag_persistence.py — how long does a cross-book condition persist?

For each cross event at time t:
  - Measure duration until condition disappears (api_bid drops below web_ask OR web_ask rises to meet api_bid)
  - Also measure: if we submit a taker at web_ask+slippage after X ms, would we still fill at a good price?

This tells us the execution-latency budget.
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
    ap.add_argument("--min-cross-bp", type=float, default=3.0)
    ap.add_argument("--min-gap-ms", type=int, default=500)
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

    # For each cross event: simulate entering at t+delay, at web_ask(t+delay)
    # P&L vs mid(t+delay+5s)
    delays = [0, 50, 100, 200, 300, 500, 1000]
    # buckets: cross_bp size vs P&L at each delay
    buckets_net = {d: [] for d in delays}
    buckets_cross_gone = {d: 0 for d in delays}
    buckets_still_filled_favor = {d: 0 for d in delays}
    n_events = 0
    cross_durations = []

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
            if wask <= 0 or wbid <= 0: continue
            web_mid0 = (wbid + wask) / 2
            if web_mid0 <= 0: continue

            cross_up = (abid - wask) / web_mid0 * 1e4
            cross_dn = (wbid - aask) / web_mid0 * 1e4
            side = None
            if cross_up >= args.min_cross_bp and t - last_sig_t > args.min_gap_ms:
                side = "buy"; entry_best = wask; signal_cross = cross_up
            elif cross_dn >= args.min_cross_bp and t - last_sig_t > args.min_gap_ms:
                side = "sell"; entry_best = wbid; signal_cross = cross_dn
            if side is None: continue
            last_sig_t = t
            n_events += 1

            # Cross duration: scan web + api until cross gone
            # cross gone when: buy: web_ask > api_bid ; sell: web_bid < api_ask
            # Approximate by walking forward on web feed
            gone_t = None
            k = j
            while k < len(wtl):
                tk = wtl[k]
                # Find matching api (nearest api_t <= tk)
                ai = _bisect_le(atl, tk)
                if ai < 0: k += 1; continue
                b_a = ab[ai]; a_a = aa[ai]
                if side == "buy":
                    if wa[k] >= b_a:  # web_ask caught up to api_bid
                        gone_t = tk; break
                else:
                    if wb[k] <= a_a:
                        gone_t = tk; break
                k += 1
            if gone_t is not None:
                cross_durations.append(gone_t - t)

            # For each delay: if we "send" taker at t+delay, fill at web_ask(t+delay) or web_bid(t+delay)
            # then close 5s later.
            for delay in delays:
                t_fill = t + delay
                jf = _bisect_le(wtl, t_fill)
                if jf < 0: continue
                if side == "buy":
                    entry_px = wa[jf]
                else:
                    entry_px = wb[jf]
                # Close 5s later (taker)
                jc = _bisect_le(wtl, t_fill + 5000)
                if jc < 0 or jc == jf: continue
                if side == "buy":
                    exit_px = wb[jc]
                    gross = (exit_px - entry_px) / entry_px * 1e4
                else:
                    exit_px = wa[jc]
                    gross = (entry_px - exit_px) / entry_px * 1e4
                net = gross - 0.85  # 2 × taker fee 0.425
                buckets_net[delay].append(net)
                # Did cross still exist at t_fill?
                ai = _bisect_le(atl, t_fill)
                if ai >= 0:
                    b_a = ab[ai]; a_a = aa[ai]
                    if side == "buy":
                        still = (b_a > wa[jf])
                    else:
                        still = (wb[jf] > a_a)
                    if not still:
                        buckets_cross_gone[delay] += 1

    print(f"[events] n={n_events}  (min_cross_bp={args.min_cross_bp})")
    if cross_durations:
        cd = sorted(cross_durations)
        print(f"[cross duration]  n={len(cd)}  mean={S.mean(cd):.0f}ms  "
              f"median={S.median(cd):.0f}ms  p25={cd[len(cd)//4]:.0f}ms  "
              f"p75={cd[3*len(cd)//4]:.0f}ms  p90={cd[9*len(cd)//10]:.0f}ms")
    print()
    print(f"{'delay':>6s}  {'n':>6s}  {'net_mean':>10s}  {'net_med':>9s}  {'WR_net':>7s}  "
          f"{'cross_gone%':>12s}")
    for d in delays:
        nets = buckets_net[d]
        if not nets: continue
        wr = sum(1 for x in nets if x > 0) / len(nets) * 100
        gone_pct = buckets_cross_gone[d] / len(nets) * 100
        print(f"{d:>5d}ms  {len(nets):>6d}  {S.mean(nets):>+9.2f}bp  "
              f"{S.median(nets):>+8.2f}bp  {wr:>6.1f}%  {gone_pct:>11.1f}%")


if __name__ == "__main__":
    main()

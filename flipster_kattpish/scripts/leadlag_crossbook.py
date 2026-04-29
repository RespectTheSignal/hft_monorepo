#!/usr/bin/env python3
"""leadlag_crossbook.py — extra analysis: api↔web cross-book events.

Cross-book = api_bid > web_ask  (web_ask is stale; we can TAKER BUY at web_ask below mid)
          or api_ask < web_bid  (web_bid is stale; we can TAKER SELL at web_bid above mid)

For these, entry cost = 0 (fill at web top, which is below/above fair mid by the cross_bp).
Only cost is taker fee 0.425bp + exit cost.

Measures:
  - cross event frequency
  - size distribution of cross_bp
  - mid move in our direction at +1s/+5s after cross
  - exit P&L if we close taker 5s later
"""
from __future__ import annotations
import argparse, bisect, json, statistics as S, sys
from collections import defaultdict
from pathlib import Path


def _bisect_le(tlist, t):
    i = bisect.bisect_right(tlist, t) - 1
    return i


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("jsonl")
    ap.add_argument("--min-cross-bp", type=float, default=1.0)
    ap.add_argument("--min-gap-ms", type=int, default=500)
    ap.add_argument("--hold-ms", type=int, default=5000)
    ap.add_argument("--taker-fee-bp", type=float, default=0.425)
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
    print(f"[loaded] api_syms={len(api_t)} web_syms={len(web_t)}")

    events_up = []  # buy at web_ask: cross_bp, half_spread_exit, mid_move_1s, mid_move_5s, pnl_taker_taker
    events_dn = []
    n_checked = 0

    syms = sorted(set(api_t) & set(web_t))
    for sym in syms:
        if len(api_t[sym]) < 50 or len(web_t[sym]) < 50:
            continue
        atl = api_t[sym]; ab = api_b[sym]; aa = api_a[sym]
        wtl = web_t[sym]; wb = web_b[sym]; wa = web_a[sym]
        last_sig_t = -10**18
        for i, t in enumerate(atl):
            # Find latest web tick with ts <= t
            j = _bisect_le(wtl, t)
            if j < 0: continue
            wbid = wb[j]; wask = wa[j]
            abid = ab[i]; aask = aa[i]
            if wask <= 0 or wbid <= 0: continue
            web_mid0 = (wbid + wask) / 2
            if web_mid0 <= 0: continue

            cross_up = (abid - wask) / web_mid0 * 1e4   # >0: api_bid above web_ask = stale ask
            cross_dn = (wbid - aask) / web_mid0 * 1e4   # >0: web_bid above api_ask = stale bid

            if cross_up >= args.min_cross_bp and t - last_sig_t > args.min_gap_ms:
                # Buy at web_ask (taker lifts stale ask). Fill at price = wask.
                entry_px = wask
                # Hold 5s, exit taker sell at web_bid(t+hold)
                k = _bisect_le(wtl, t + args.hold_ms)
                if k < 0 or k == j: continue
                exit_bid = wb[k]
                # taker sell fee on both legs
                gross_bp = (exit_bid - entry_px) / entry_px * 1e4
                net_bp = gross_bp - 2 * args.taker_fee_bp
                mid_5s = (wb[k] + wa[k]) / 2
                mid_1s_idx = _bisect_le(wtl, t + 1000)
                mid_1s = (wb[mid_1s_idx] + wa[mid_1s_idx]) / 2 if mid_1s_idx >= 0 else None
                events_up.append({
                    "sym": sym, "cross_bp": cross_up,
                    "mid_move_1s_bp": (mid_1s - web_mid0)/web_mid0*1e4 if mid_1s else None,
                    "mid_move_5s_bp": (mid_5s - web_mid0)/web_mid0*1e4,
                    "gross_bp": gross_bp, "net_bp": net_bp,
                })
                last_sig_t = t

            elif cross_dn >= args.min_cross_bp and t - last_sig_t > args.min_gap_ms:
                entry_px = wbid  # sell at web_bid (taker hits stale bid)
                k = _bisect_le(wtl, t + args.hold_ms)
                if k < 0 or k == j: continue
                exit_ask = wa[k]
                # For sell: gross = (entry - exit) / entry  (we sold high, buy back low)
                gross_bp = (entry_px - exit_ask) / entry_px * 1e4
                net_bp = gross_bp - 2 * args.taker_fee_bp
                mid_5s = (wb[k] + wa[k]) / 2
                mid_1s_idx = _bisect_le(wtl, t + 1000)
                mid_1s = (wb[mid_1s_idx] + wa[mid_1s_idx]) / 2 if mid_1s_idx >= 0 else None
                sign = -1  # sell direction, favorable = mid drops
                events_dn.append({
                    "sym": sym, "cross_bp": cross_dn,
                    "mid_move_1s_bp": ((mid_1s - web_mid0)/web_mid0*1e4)*sign if mid_1s else None,
                    "mid_move_5s_bp": ((mid_5s - web_mid0)/web_mid0*1e4)*sign,
                    "gross_bp": gross_bp, "net_bp": net_bp,
                })
                last_sig_t = t
            n_checked += 1

    print(f"[scanned] {n_checked} api ticks, cross_up={len(events_up)}, cross_dn={len(events_dn)}")
    print(f"[params] min_cross_bp={args.min_cross_bp}  hold_ms={args.hold_ms}  "
          f"taker_fee={args.taker_fee_bp}bp × 2 legs")
    print()

    def stats(events, label):
        if not events:
            print(f"  {label}: no events")
            return
        cross = [e["cross_bp"] for e in events]
        gross = [e["gross_bp"] for e in events]
        net = [e["net_bp"] for e in events]
        m5 = [e["mid_move_5s_bp"] for e in events if e["mid_move_5s_bp"] is not None]
        wr_gross = sum(1 for x in gross if x > 0) / len(gross) * 100
        wr_net = sum(1 for x in net if x > 0) / len(net) * 100
        print(f"  {label}: n={len(events)}")
        print(f"    cross_bp:    mean={S.mean(cross):.2f}  med={S.median(cross):.2f}  "
              f"p75={S.quantiles(cross, n=4)[2]:.2f}  p90={S.quantiles(cross, n=10)[8]:.2f}")
        print(f"    mid_move@5s: mean={S.mean(m5):+.2f}bp  med={S.median(m5):+.2f}bp")
        print(f"    gross:       mean={S.mean(gross):+.2f}bp  med={S.median(gross):+.2f}bp  WR={wr_gross:.1f}%")
        print(f"    NET (-fees): mean={S.mean(net):+.2f}bp  med={S.median(net):+.2f}bp  WR={wr_net:.1f}%")

    print("=== cross-book takeoff events ===")
    stats(events_up, "UP (api_bid > web_ask → taker buy at web_ask)")
    print()
    stats(events_dn, "DN (api_ask < web_bid → taker sell at web_bid)")
    print()

    # Bucket by cross size
    for bucket_min, bucket_max in [(1, 3), (3, 5), (5, 10), (10, 20), (20, 999)]:
        up_b = [e for e in events_up if bucket_min <= e["cross_bp"] < bucket_max]
        dn_b = [e for e in events_dn if bucket_min <= e["cross_bp"] < bucket_max]
        all_b = up_b + dn_b
        if not all_b: continue
        gross = [e["gross_bp"] for e in all_b]
        net = [e["net_bp"] for e in all_b]
        wr_net = sum(1 for x in net if x > 0) / len(net) * 100
        print(f"cross_bp [{bucket_min},{bucket_max}): n={len(all_b)}  "
              f"gross_mean={S.mean(gross):+.2f}bp  net_mean={S.mean(net):+.2f}bp  "
              f"WR_net={wr_net:.1f}%")


if __name__ == "__main__":
    main()

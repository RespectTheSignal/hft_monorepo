#!/usr/bin/env python3
"""leadlag_stale_limit.py — test: BUY LIMIT @ web_ask (stale cross-side price).

Scenario: cross-up signal (api_bid > web_ask). Post BUY LIMIT at web_ask.
  - If real stale sell orders exist at web_ask, we lift them at discount.
  - If only feed lag, no fill unless price later retraces to web_ask.

Fill detection proxy:
  - Future web_ask drops to ≤ our_price, within FILL_WAIT_MS
    OR a market sell prints at ≤ our_price (approximated same way)

For any fill, compute:
  - Fill_t and prevailing api_mid at fill time (how stale the book was)
  - Post-fill 5s mid (adverse selection) → did price continue against us?

Compare:
  - Signal cases (cross-up for buy, cross-dn for sell)
  - Control (random times, same price-below-ask posting)
"""
from __future__ import annotations
import argparse, bisect, json, random, statistics as S
from collections import defaultdict


def _bisect_le(tlist, t):
    return bisect.bisect_right(tlist, t) - 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("jsonl")
    ap.add_argument("--min-cross-bp", type=float, default=1.0)
    ap.add_argument("--min-gap-ms", type=int, default=500)
    ap.add_argument("--fill-wait-ms", type=int, default=2000)
    ap.add_argument("--hold-after-fill-ms", type=int, default=5000)
    ap.add_argument("--taker-fee-bp", type=float, default=0.425)
    ap.add_argument("--maker-fee-bp", type=float, default=0.15)
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

    def web_mid_at(sym, t):
        tl = web_t[sym]
        if not tl: return None
        i = _bisect_le(tl, t)
        if i < 0: return None
        return (web_b[sym][i] + web_a[sym][i]) / 2

    def api_mid_at(sym, t):
        tl = api_t[sym]
        if not tl: return None
        i = _bisect_le(tl, t)
        if i < 0: return None
        return (api_b[sym][i] + api_a[sym][i]) / 2

    # STRICT fill detection: fill only if web_ask moves STRICTLY BELOW P
    # (meaning a new seller appeared at a lower level, which would cross our bid).
    # Starting equality (web_ask == P right after signal) is NOT a fill.
    def fill_buy(sym, t0, P):
        tl = web_t[sym]
        # Skip the first tick at exactly t0 (it's likely the signal-time tick)
        lo = bisect.bisect_right(tl, t0)
        hi = bisect.bisect_right(tl, t0 + args.fill_wait_ms)
        for i in range(lo, hi):
            if web_a[sym][i] < P:   # strict less-than — new seller UNDER our bid
                return tl[i]
        return None

    def fill_sell(sym, t0, P):
        tl = web_t[sym]
        lo = bisect.bisect_right(tl, t0)
        hi = bisect.bisect_right(tl, t0 + args.fill_wait_ms)
        for i in range(lo, hi):
            if web_b[sym][i] > P:
                return tl[i]
        return None

    syms = sorted(set(api_t) & set(web_t))
    n_sig_up = n_sig_dn = 0
    sig_fills = {"up": [], "dn": []}
    ctl_fills = {"up": [], "dn": []}

    for sym in syms:
        if len(api_t[sym]) < 50 or len(web_t[sym]) < 50:
            continue
        atl = api_t[sym]; ab = api_b[sym]; aa = api_a[sym]
        wtl = web_t[sym]; wb = web_b[sym]; wa = web_a[sym]
        last_sig_t = -10**18
        sig_times = []

        for i, t in enumerate(atl):
            j = _bisect_le(wtl, t)
            if j < 0: continue
            wbid = wb[j]; wask = wa[j]
            abid = ab[i]; aask = aa[i]
            if wask <= 0 or wbid <= 0 or abid <= 0 or aask <= 0: continue
            web_mid0 = (wbid + wask) / 2
            api_mid0 = (abid + aask) / 2
            web_sp = (wask - wbid) / web_mid0 * 1e4
            api_sp = (aask - abid) / api_mid0 * 1e4
            if max(web_sp, api_sp) > args.max_spread_bp:
                continue

            cross_up = (abid - wask) / web_mid0 * 1e4
            cross_dn = (wbid - aask) / web_mid0 * 1e4
            side = None
            if cross_up >= args.min_cross_bp and t - last_sig_t > args.min_gap_ms:
                side = "up"; P = wask; cross_bp = cross_up
            elif cross_dn >= args.min_cross_bp and t - last_sig_t > args.min_gap_ms:
                side = "dn"; P = wbid; cross_bp = cross_dn
            if side is None: continue
            last_sig_t = t
            sig_times.append(t)

            if side == "up":
                n_sig_up += 1
                ft = fill_buy(sym, t, P)
            else:
                n_sig_dn += 1
                ft = fill_sell(sym, t, P)

            if ft is None:
                sig_fills[side].append({"filled": False, "cross_bp": cross_bp})
                continue

            # Post-fill: hold for hold_after_fill_ms, exit TAKER (sell at web_bid for buy)
            k_exit = _bisect_le(wtl, ft + args.hold_after_fill_ms)
            if k_exit < 0:
                continue
            if side == "up":
                exit_px = wb[k_exit]
                gross = (exit_px - P) / P * 1e4
            else:
                exit_px = wa[k_exit]
                gross = (P - exit_px) / P * 1e4
            # Fees: maker open (rebate/low fee), taker close
            net = gross - args.maker_fee_bp - args.taker_fee_bp
            # Also: what was api_mid at fill time (how much did book move vs our stale price)
            mid_at_fill = api_mid_at(sym, ft)
            book_move = 0
            if mid_at_fill:
                if side == "up":
                    book_move = (mid_at_fill - P) / P * 1e4  # how far mid moved up from our fill
                else:
                    book_move = (P - mid_at_fill) / P * 1e4
            sig_fills[side].append({
                "filled": True, "cross_bp": cross_bp,
                "delay_ms": ft - t, "book_move_bp": book_move,
                "gross": gross, "net": net,
            })

        # Controls: random times
        if not atl: continue
        span = (atl[0], atl[-1])
        if span[1] - span[0] < 60_000: continue
        for _ in sig_times:
            tr = random.randint(span[0], span[1])
            j = _bisect_le(wtl, tr)
            if j < 0: continue
            wbid = wb[j]; wask = wa[j]
            web_mid0 = (wbid + wask) / 2
            # random direction
            if random.random() < 0.5:
                side = "up"; P = wask
                ft = fill_buy(sym, tr, P)
            else:
                side = "dn"; P = wbid
                ft = fill_sell(sym, tr, P)
            if ft is None:
                ctl_fills[side].append({"filled": False})
                continue
            k_exit = _bisect_le(wtl, ft + args.hold_after_fill_ms)
            if k_exit < 0: continue
            if side == "up":
                exit_px = wb[k_exit]
                gross = (exit_px - P) / P * 1e4
            else:
                exit_px = wa[k_exit]
                gross = (P - exit_px) / P * 1e4
            net = gross - args.maker_fee_bp - args.taker_fee_bp
            ctl_fills[side].append({"filled": True, "gross": gross, "net": net})

    print(f"[params] min_cross={args.min_cross_bp}bp  fill_wait={args.fill_wait_ms}ms  "
          f"hold={args.hold_after_fill_ms}ms  max_sp={args.max_spread_bp}bp")
    print(f"[params] fees: maker_open={args.maker_fee_bp}bp  taker_close={args.taker_fee_bp}bp")
    print()

    def report(label, events, ctl):
        if not events:
            print(f"  {label}: no events")
            return
        n = len(events)
        nf = sum(1 for e in events if e["filled"])
        frate = nf / n * 100
        cn = len(ctl); cnf = sum(1 for e in ctl if e["filled"])
        cfrate = cnf / cn * 100 if cn else 0
        print(f"  {label}: signals n={n}  fills={nf} ({frate:.2f}%)  "
              f"control_fills={cnf}/{cn} ({cfrate:.2f}%)")
        fills = [e for e in events if e["filled"]]
        if fills:
            gross = [e["gross"] for e in fills]
            net = [e["net"] for e in fills]
            dly = [e["delay_ms"] for e in fills]
            bmv = [e["book_move_bp"] for e in fills]
            wr = sum(1 for x in net if x > 0) / len(net) * 100
            # Overall EV per signal (not per fill)
            ev_per_sig = sum(net) / n  # unfilled contribute 0
            print(f"     on-fill: gross_mean={S.mean(gross):+.2f}bp  net_mean={S.mean(net):+.2f}bp  "
                  f"net_med={S.median(net):+.2f}bp  WR={wr:.1f}%")
            print(f"     fill_delay: mean={S.mean(dly):.0f}ms  med={S.median(dly):.0f}ms  "
                  f"p75={S.quantiles(dly, n=4)[2]:.0f}ms")
            print(f"     book_moved_at_fill: mean={S.mean(bmv):+.2f}bp  med={S.median(bmv):+.2f}bp")
            print(f"     EV per signal (fills + unfilled 0): {ev_per_sig:+.3f}bp")

            cfills = [e for e in ctl if e["filled"]]
            if cfills:
                cnet = [e["net"] for e in cfills]
                cwr = sum(1 for x in cnet if x > 0) / len(cnet) * 100
                print(f"     control on-fill: net_mean={S.mean(cnet):+.2f}bp  WR={cwr:.1f}%")

    print("=== BUY LIMIT @ web_ask (cross-up signal) ===")
    report("UP", sig_fills["up"], ctl_fills["up"])
    print()
    print("=== SELL LIMIT @ web_bid (cross-dn signal) ===")
    report("DN", sig_fills["dn"], ctl_fills["dn"])

    # By cross_bp bucket
    print()
    print("=== by cross_bp bucket (combined up+dn) ===")
    all_ev = sig_fills["up"] + sig_fills["dn"]
    for lo, hi in [(1, 2), (2, 3), (3, 5), (5, 10), (10, 20), (20, 999)]:
        b = [e for e in all_ev if lo <= e.get("cross_bp", 0) < hi]
        if not b: continue
        nf = sum(1 for e in b if e["filled"])
        frate = nf / len(b) * 100
        fills = [e for e in b if e["filled"]]
        if not fills:
            print(f"  cross [{lo},{hi}): n={len(b)}  fill_rate={frate:.2f}% (no fills)")
            continue
        net_sum = sum(e["net"] for e in fills)
        ev = net_sum / len(b)
        print(f"  cross [{lo},{hi}): n={len(b):5d}  fill_rate={frate:>5.2f}%  "
              f"on-fill net_mean={S.mean([e['net'] for e in fills]):+.2f}bp  "
              f"EV/signal={ev:+.3f}bp")


if __name__ == "__main__":
    main()

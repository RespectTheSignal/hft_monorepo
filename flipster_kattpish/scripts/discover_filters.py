#!/usr/bin/env python3
"""Discover high-WR filter regions from shadow log.

Strategy:
  1. Load shadow candidates, group by symbol
  2. Per symbol, bulk-fetch Binance bookticker for full time range
  3. Classify each candidate (would_tp / would_stop / timeout / nodata)
  4. Per feature, bucket candidates and compute win rate
  5. Report features with strong WR divergence across buckets
"""
from __future__ import annotations
import argparse, json, sys, urllib.request, urllib.parse, statistics as stat
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

QDB_HTTP = "http://211.181.122.102:9000/exec"


def to_binance_sym(fsym: str) -> str:
    return fsym.replace("USDT.PERP", "_USDT")


def fetch_binance_range(bsym: str, t0_ms: int, t1_ms: int) -> list:
    """Bulk fetch Binance bookticker for a time range."""
    all_rows = []
    cur = t0_ms
    while cur < t1_ms:
        t0 = datetime.fromtimestamp(cur/1000, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
        t1 = datetime.fromtimestamp(t1_ms/1000, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
        sql = (f"SELECT timestamp, bid_price, ask_price FROM binance_bookticker "
               f"WHERE symbol='{bsym}' AND timestamp >= '{t0}' AND timestamp <= '{t1}' "
               f"ORDER BY timestamp LIMIT 10000")
        try:
            url = QDB_HTTP + "?" + urllib.parse.urlencode({"query": sql})
            r = json.loads(urllib.request.urlopen(url, timeout=30).read())
            rows = r.get("dataset", [])
        except Exception as e:
            print(f"  QDB err {bsym}: {e}", file=sys.stderr)
            return all_rows
        if not rows: break
        for row in rows:
            try:
                dt = datetime.fromisoformat(row[0].replace("Z","+00:00"))
                ts = int(dt.timestamp() * 1000)
                mid = (float(row[1]) + float(row[2])) / 2
                all_rows.append((ts, mid))
            except Exception:
                continue
        if len(rows) < 10000:
            break
        cur = all_rows[-1][0] + 1
    return all_rows


def classify_from_array(arr: list, start_idx: int, side: str, tp_bp: float, stop_bp: float, window_ms: int):
    """Returns (outcome, extras_dict) with max_favor/max_adverse + time-to-each +
    final_bp (price at end of window, for timeout drift analysis)."""
    if start_idx >= len(arr): return "nodata", {}
    entry_mid = arr[start_idx][1]
    entry_ts = arr[start_idx][0]
    if entry_mid <= 0: return "nodata", {}
    deadline_ts = entry_ts + window_ms
    max_favor = 0.0
    max_adverse = 0.0
    t_to_tp = None
    t_to_stop = None
    last_bp = 0.0
    outcome = "timeout"
    for i in range(start_idx, len(arr)):
        ts, mid = arr[i]
        if ts > deadline_ts: break
        if mid <= 0: continue
        bp = (mid - entry_mid) / entry_mid * 1e4
        if side == "sell": bp = -bp
        last_bp = bp
        if bp > max_favor: max_favor = bp
        if bp < max_adverse: max_adverse = bp
        if t_to_tp is None and bp >= tp_bp: t_to_tp = ts - entry_ts
        if t_to_stop is None and bp <= -stop_bp: t_to_stop = ts - entry_ts
    extras = {"max_favor_bp": round(max_favor, 2),
              "max_adverse_bp": round(max_adverse, 2),
              "final_bp": round(last_bp, 2),
              "t_to_tp_ms": t_to_tp, "t_to_stop_ms": t_to_stop}
    if t_to_tp is not None and t_to_stop is not None:
        outcome = "would_tp" if t_to_tp <= t_to_stop else "would_stop"
    elif t_to_tp is not None: outcome = "would_tp"
    elif t_to_stop is not None: outcome = "would_stop"
    return outcome, extras


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--shadow", type=Path, default="/home/gate1/projects/quant/flipster_kattpish/logs/shadow.jsonl")
    ap.add_argument("--tp-bp", type=float, default=45.0)
    ap.add_argument("--stop-bp", type=float, default=30.0)
    ap.add_argument("--window-sec", type=int, default=60)
    ap.add_argument("--max-per-sym", type=int, default=500)
    ap.add_argument("--out", type=Path, default=Path("/tmp/scored_candidates.jsonl"))
    args = ap.parse_args()

    print(f"Loading shadow log from {args.shadow}...")
    by_sym = defaultdict(list)
    with open(args.shadow) as f:
        for line in f:
            try: ev = json.loads(line)
            except: continue
            if "cross_bp" in ev and "ts_ms" in ev and "sym" in ev:
                by_sym[ev["sym"]].append(ev)
    total = sum(len(v) for v in by_sym.values())
    print(f"  {total} candidates across {len(by_sym)} symbols\n")

    scored = []
    out_f = open(args.out, "w")
    for sym_idx, (sym, events) in enumerate(sorted(by_sym.items(), key=lambda kv: -len(kv[1]))):
        events = sorted(events, key=lambda e: e["ts_ms"])
        if args.max_per_sym and len(events) > args.max_per_sym:
            # Keep evenly sampled
            step = len(events) // args.max_per_sym
            events = events[::step]
        if not events: continue
        t_min = events[0]["ts_ms"]
        t_max = events[-1]["ts_ms"] + args.window_sec * 1000
        bsym = to_binance_sym(sym)
        arr = fetch_binance_range(bsym, t_min, t_max)
        print(f"[{sym_idx+1}/{len(by_sym)}] {sym:22s} events={len(events):4d} binance_rows={len(arr):5d}",
              end="", flush=True)
        if not arr:
            print(" → NO_BINANCE")
            for ev in events:
                ev["outcome"] = "nodata"
                out_f.write(json.dumps(ev, default=str) + "\n")
                scored.append(ev)
            continue
        # For each event, binary-search nearest-after Binance tick
        arr_ts = [x[0] for x in arr]
        import bisect
        tp_ct = 0; st_ct = 0; to_ct = 0; nd_ct = 0
        for ev in events:
            idx = bisect.bisect_left(arr_ts, ev["ts_ms"])
            if idx >= len(arr):
                ev["outcome"] = "nodata"; nd_ct += 1
            else:
                # Ensure not too far
                if arr_ts[idx] - ev["ts_ms"] > 5000:
                    ev["outcome"] = "nodata"; nd_ct += 1
                else:
                    out, extras = classify_from_array(arr, idx, ev["side"],
                                                        args.tp_bp, args.stop_bp,
                                                        args.window_sec * 1000)
                    ev["outcome"] = out
                    ev.update(extras)
                    if out == "would_tp": tp_ct += 1
                    elif out == "would_stop": st_ct += 1
                    elif out == "timeout": to_ct += 1
                    else: nd_ct += 1
            out_f.write(json.dumps(ev, default=str) + "\n")
            scored.append(ev)
        print(f" → TP={tp_ct} STOP={st_ct} TO={to_ct} ND={nd_ct}")
    out_f.close()
    print(f"\n→ {len(scored)} scored → {args.out}")

    # Feature analysis
    outcomes = [e["outcome"] for e in scored]
    n_tp = sum(1 for o in outcomes if o == "would_tp")
    n_st = sum(1 for o in outcomes if o == "would_stop")
    n_to = sum(1 for o in outcomes if o == "timeout")
    n_nd = sum(1 for o in outcomes if o == "nodata")
    scoreable = n_tp + n_st
    print(f"\n=== Overall (scoreable only): TP={n_tp}  STOP={n_st}  WR={n_tp/scoreable*100:.1f}% ===")
    print(f"    (timeout={n_to}, nodata={n_nd})")

    # Analyze features
    print(f"\n=== Feature → Win rate (bucketed, scoreable only) ===")
    numeric_features = [
        "cross_bp", "web_age_ms", "diverge_bp", "prev_edge", "sig_hist_len",
        "bx_range_1s_bp", "bx_mono_bp", "bx_mono_n",
        "bx_vel_250ms_bp", "bx_vel_400ms_bp",
        "bx_1000ms_bp", "bx_5000ms_bp", "bx_10000ms_bp", "bx_20000ms_bp",
    ]
    # Only use scoreable events
    s_events = [e for e in scored if e["outcome"] in ("would_tp", "would_stop")]

    def signed_bx_bp(ev, key):
        """For buy: positive = favorable. For sell: negative = favorable → flip sign."""
        v = ev.get(key)
        if v is None: return None
        return v if ev["side"] == "buy" else -v

    # For direction-dependent features (velocity, momentum), flip sign for sells.
    dir_features = {"bx_vel_250ms_bp", "bx_vel_400ms_bp", "bx_1000ms_bp",
                    "bx_5000ms_bp", "bx_10000ms_bp", "bx_20000ms_bp", "bx_mono_bp"}
    for feat in numeric_features:
        vals = []
        for e in s_events:
            if feat in dir_features:
                v = signed_bx_bp(e, feat)
            else:
                v = e.get(feat)
            if v is None: continue
            vals.append((v, 1 if e["outcome"] == "would_tp" else 0))
        if len(vals) < 50:
            print(f"  {feat:22s} insufficient samples ({len(vals)})")
            continue
        vals.sort()
        # Split into 5 quintiles
        n = len(vals)
        print(f"  {feat:22s} n={n:4d}  (quintile WR%):", end="")
        for q in range(5):
            lo, hi = q*n//5, (q+1)*n//5
            bucket = vals[lo:hi]
            if not bucket: continue
            wr = sum(x[1] for x in bucket) / len(bucket) * 100
            rng_lo = bucket[0][0]
            rng_hi = bucket[-1][0]
            print(f"  [{rng_lo:+.1f}..{rng_hi:+.1f}]={wr:.0f}%", end="")
        print()


if __name__ == "__main__":
    main()

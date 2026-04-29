#!/usr/bin/env python3
"""One-pass scorer: for each candidate, fetch Binance once and compute
max_favor / max_adverse / final_bp at MULTIPLE checkpoints.
Output jsonl has: max_favor_{W}s, max_adverse_{W}s, final_{W}s for each W.
"""
import argparse, json, sys, urllib.request, urllib.parse, bisect
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

QDB_HTTP = "http://211.181.122.102:9000/exec"
WINDOWS_SEC = [60, 180, 300, 600, 900, 1200, 1800]
MAX_WIN_MS = max(WINDOWS_SEC) * 1000


def to_binance_sym(fsym): return fsym.replace("USDT.PERP", "_USDT")


def fetch_range(bsym, t0_ms, t1_ms):
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
            rows = json.loads(urllib.request.urlopen(url, timeout=30).read()).get("dataset", [])
        except Exception:
            return all_rows
        if not rows: break
        for r in rows:
            try:
                dt = datetime.fromisoformat(r[0].replace("Z","+00:00"))
                ts = int(dt.timestamp() * 1000)
                mid = (float(r[1]) + float(r[2])) / 2
                all_rows.append((ts, mid))
            except Exception:
                continue
        if len(rows) < 10000: break
        cur = all_rows[-1][0] + 1
    return all_rows


def snapshot_at_checkpoints(arr, start_idx, side, windows_ms):
    """For each checkpoint in windows_ms, return max_favor/max_adverse/final at that window."""
    if start_idx >= len(arr): return None
    entry = arr[start_idx][1]
    entry_ts = arr[start_idx][0]
    if entry <= 0: return None
    out = {}
    max_f = 0.0; max_a = 0.0; last_bp = 0.0
    checkpoint_idx = 0
    sorted_windows = sorted(windows_ms)
    next_deadline = entry_ts + sorted_windows[0]
    for i in range(start_idx, len(arr)):
        ts, mid = arr[i]
        if ts > entry_ts + max(windows_ms): break
        if mid <= 0: continue
        bp = (mid - entry) / entry * 1e4
        if side == "sell": bp = -bp
        last_bp = bp
        if bp > max_f: max_f = bp
        if bp < max_a: max_a = bp
        # Cross checkpoint boundaries
        while checkpoint_idx < len(sorted_windows) and ts > entry_ts + sorted_windows[checkpoint_idx]:
            w = sorted_windows[checkpoint_idx]
            out[w] = {"max_favor": round(max_f, 2),
                      "max_adverse": round(max_a, 2),
                      "final": round(last_bp, 2)}
            checkpoint_idx += 1
    # Fill remaining checkpoints (if data ended early)
    while checkpoint_idx < len(sorted_windows):
        w = sorted_windows[checkpoint_idx]
        out[w] = {"max_favor": round(max_f, 2),
                  "max_adverse": round(max_a, 2),
                  "final": round(last_bp, 2)}
        checkpoint_idx += 1
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--shadow", type=Path, default="/home/gate1/projects/quant/flipster_kattpish/logs/shadow.jsonl")
    ap.add_argument("--max-per-sym", type=int, default=150)
    ap.add_argument("--out", type=Path, default=Path("/tmp/scored_multi_window.jsonl"))
    args = ap.parse_args()

    by_sym = defaultdict(list)
    with open(args.shadow) as f:
        for line in f:
            try: ev = json.loads(line)
            except: continue
            if "cross_bp" in ev and "ts_ms" in ev and "sym" in ev:
                by_sym[ev["sym"]].append(ev)
    total = sum(len(v) for v in by_sym.values())
    print(f"Loaded {total} candidates across {len(by_sym)} symbols")

    out_f = open(args.out, "w")
    windows_ms = [w * 1000 for w in WINDOWS_SEC]
    scored_count = 0
    for i, (sym, events) in enumerate(sorted(by_sym.items(), key=lambda kv: -len(kv[1]))):
        events = sorted(events, key=lambda e: e["ts_ms"])
        if args.max_per_sym and len(events) > args.max_per_sym:
            step = len(events) // args.max_per_sym
            events = events[::step]
        if not events: continue
        t_min = events[0]["ts_ms"]
        t_max = events[-1]["ts_ms"] + MAX_WIN_MS
        bsym = to_binance_sym(sym)
        arr = fetch_range(bsym, t_min, t_max)
        print(f"[{i+1}/{len(by_sym)}] {sym:22s} events={len(events):4d} binance_rows={len(arr):5d}", flush=True)
        if not arr:
            for ev in events:
                ev["nodata"] = True
                out_f.write(json.dumps(ev, default=str) + "\n")
                scored_count += 1
            continue
        arr_ts = [x[0] for x in arr]
        for ev in events:
            idx = bisect.bisect_left(arr_ts, ev["ts_ms"])
            if idx >= len(arr) or arr_ts[idx] - ev["ts_ms"] > 5000:
                ev["nodata"] = True
                out_f.write(json.dumps(ev, default=str) + "\n")
                continue
            snap = snapshot_at_checkpoints(arr, idx, ev["side"], windows_ms)
            if snap is None:
                ev["nodata"] = True
            else:
                for w_ms, s in snap.items():
                    w_s = w_ms // 1000
                    ev[f"max_favor_{w_s}s"] = s["max_favor"]
                    ev[f"max_adverse_{w_s}s"] = s["max_adverse"]
                    ev[f"final_{w_s}s"] = s["final"]
            out_f.write(json.dumps(ev, default=str) + "\n")
            scored_count += 1
    out_f.close()
    print(f"\n→ {scored_count} candidates scored → {args.out}")


if __name__ == "__main__":
    main()

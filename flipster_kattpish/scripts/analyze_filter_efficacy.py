#!/usr/bin/env python3
"""Per-filter efficacy analysis via post-hoc replay.

For each rejected signal in the rejection jsonl, fetch Binance bookticker
around the rejection time and compute the outcome if we had entered:
  - would_tp:   favorable move >= TP_BP reached before adverse move >= STOP_BP
  - would_stop: adverse move >= STOP_BP reached first
  - timeout:   neither reached within WINDOW_SEC

Aggregate per filter. Each "rejection" can be scored — a filter that
mostly blocks would_stop trades is saving us money; a filter that mostly
blocks would_tp trades is costing us.
"""
from __future__ import annotations
import argparse, json, sys, urllib.request, urllib.parse
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

QDB_HTTP = "http://211.181.122.102:9000/exec"
TP_BP = 35.0          # matches live TP_BP
STOP_BP = 40.0        # matches live STOP_BP_DEFAULT
WINDOW_SEC = 60       # look this far ahead
AVG_TRADE_USD = 50    # fixed-size

# Sampling to keep analysis manageable
MAX_REJECTIONS_PER_FILTER = 500


def to_binance_sym(fsym: str) -> str:
    return fsym.replace("USDT.PERP", "_USDT")


def fetch_qdb(sql: str):
    url = QDB_HTTP + "?" + urllib.parse.urlencode({"query": sql})
    r = json.loads(urllib.request.urlopen(url, timeout=15).read())
    return [c["name"] for c in r.get("columns", [])], r.get("dataset", [])


def ms_to_iso(ms: int) -> str:
    return datetime.fromtimestamp(ms / 1000, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def classify_rejection(sym: str, side: str, ts_ms: int,
                       tp_bp: float, stop_bp: float, window_sec: int):
    """Fetch Binance ticks from ts_ms to ts_ms+window and classify."""
    bsym = to_binance_sym(sym)
    t0 = ms_to_iso(ts_ms)
    t1 = ms_to_iso(ts_ms + window_sec * 1000)
    sql = (f"SELECT timestamp, bid_price, ask_price FROM binance_bookticker "
           f"WHERE symbol='{bsym}' AND timestamp >= '{t0}' AND timestamp <= '{t1}' "
           f"ORDER BY timestamp LIMIT 5000")
    try:
        cols, rows = fetch_qdb(sql)
    except Exception:
        return "nodata", 0, 0
    if not rows:
        return "nodata", 0, 0
    # entry mid at start
    first = rows[0]
    entry_mid = (float(first[1]) + float(first[2])) / 2
    if entry_mid <= 0:
        return "nodata", 0, 0
    max_favor_bp = 0
    max_adverse_bp = 0
    time_to_tp = None
    time_to_stop = None
    # Walk forward
    for r in rows:
        mid = (float(r[1]) + float(r[2])) / 2
        if mid <= 0:
            continue
        move_bp = (mid - entry_mid) / entry_mid * 1e4
        if side == "sell":
            move_bp = -move_bp
        if move_bp > max_favor_bp:
            max_favor_bp = move_bp
        if move_bp < max_adverse_bp:
            max_adverse_bp = move_bp
        # epoch from iso
        tick_ts = r[0]
        if max_favor_bp >= tp_bp and time_to_tp is None:
            time_to_tp = tick_ts
        if max_adverse_bp <= -stop_bp and time_to_stop is None:
            time_to_stop = tick_ts
        if time_to_tp and time_to_stop:
            break
    if time_to_tp and time_to_stop:
        return ("would_tp", max_favor_bp, max_adverse_bp) if time_to_tp <= time_to_stop \
               else ("would_stop", max_favor_bp, max_adverse_bp)
    if time_to_tp:
        return "would_tp", max_favor_bp, max_adverse_bp
    if time_to_stop:
        return "would_stop", max_favor_bp, max_adverse_bp
    return "timeout", max_favor_bp, max_adverse_bp


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", type=Path, default="/home/gate1/projects/quant/flipster_kattpish/logs/filter_rejections.jsonl")
    ap.add_argument("--tp-bp", type=float, default=TP_BP)
    ap.add_argument("--stop-bp", type=float, default=STOP_BP)
    ap.add_argument("--window-sec", type=int, default=WINDOW_SEC)
    ap.add_argument("--max-per-filter", type=int, default=MAX_REJECTIONS_PER_FILTER)
    args = ap.parse_args()

    if not args.log.exists():
        print(f"No rejection log at {args.log}"); sys.exit(1)

    rejects = defaultdict(list)
    with open(args.log) as f:
        for line in f:
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            rejects[ev["filter"]].append(ev)

    print(f"=== Loaded rejections per filter ===")
    for flt, evs in sorted(rejects.items(), key=lambda x: -len(x[1])):
        print(f"  {flt:18s}  n={len(evs)}")
    print()

    # Sample each filter
    import random
    results = {}
    for flt, evs in rejects.items():
        sample = random.sample(evs, min(args.max_per_filter, len(evs)))
        print(f"Scoring {flt:18s}  sampled {len(sample)}/{len(evs)}...")
        stats = {"would_tp": 0, "would_stop": 0, "timeout": 0, "nodata": 0,
                 "favor_sum": 0.0, "adverse_sum": 0.0}
        for ev in sample:
            outcome, favor, adverse = classify_rejection(
                ev["sym"], ev["side"], ev["ts_ms"],
                args.tp_bp, args.stop_bp, args.window_sec,
            )
            stats[outcome] += 1
            stats["favor_sum"] += favor
            stats["adverse_sum"] += adverse
        results[flt] = stats

    # Report
    print()
    print(f"=== FILTER EFFICACY (TP={args.tp_bp}bp, STOP={args.stop_bp}bp, window={args.window_sec}s) ===")
    print(f"{'filter':20s} {'n':>5s} {'WIN':>5s} {'LOSS':>5s} {'MEH':>5s} {'ND':>4s}  "
          f"{'est_$saved':>12s}  verdict")
    print("-" * 95)
    for flt, s in sorted(results.items(), key=lambda kv: -kv[1]['would_stop']):
        n = s["would_tp"] + s["would_stop"] + s["timeout"] + s["nodata"]
        if n == 0: continue
        scored = s["would_tp"] + s["would_stop"] + s["timeout"]
        if scored == 0: continue
        win_rate = s["would_tp"] / scored * 100
        # Estimated $ saved per rejection (rejections are things we DIDN'T do)
        # If filter blocked a would_stop → saved $STOP_BP/10000 * AVG_TRADE_USD
        # If filter blocked a would_tp  → cost us $TP_BP/10000 * AVG_TRADE_USD
        saved = (s["would_stop"] * args.stop_bp - s["would_tp"] * args.tp_bp) / 1e4 * AVG_TRADE_USD
        # Scale up to full rejection count (we sampled)
        scale = len(rejects[flt]) / max(1, scored + s["nodata"])
        saved_full = saved * scale
        if saved_full > 0.5:
            verdict = "✓ saves $"
        elif saved_full < -0.5:
            verdict = "✗ COSTS $"
        else:
            verdict = "~ neutral"
        print(f"{flt:20s} {len(rejects[flt]):>5d} {s['would_tp']:>5d} {s['would_stop']:>5d} "
              f"{s['timeout']:>5d} {s['nodata']:>4d}  "
              f"${saved_full:>+10.2f}  {verdict}")
    print()
    print("Legend: WIN=would_tp (filter cost us profit), LOSS=would_stop (filter saved loss),")
    print("        MEH=timeout, ND=no Binance data")


if __name__ == "__main__":
    main()

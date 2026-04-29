#!/usr/bin/env python3
"""Sweep TP/STOP combinations using pre-stored trajectory (max_favor + max_adverse + t_to_each)."""
from __future__ import annotations
import argparse, json
from pathlib import Path

FEE_BP = 8.0
AVG_USD = 50


def outcome_at(max_favor, max_adverse, t_tp, t_stop, tp_bp, stop_bp):
    """Given trajectory summary and (tp, stop) thresholds, compute outcome.
    Note: t_tp and t_stop are computed at SCORE-TIME tp/stop. For different
    thresholds, we only know they are HIT if max ≥ threshold. Ordering may not
    match our original tp/stop, so we approximate."""
    tp_hit = max_favor >= tp_bp
    stop_hit = abs(max_adverse) >= stop_bp
    if tp_hit and stop_hit:
        # Ambiguous — use original ordering if thresholds align roughly
        # If neither t_tp nor t_stop valid, default to 'stop' (conservative)
        if t_tp is not None and t_stop is not None:
            return "would_tp" if t_tp <= t_stop else "would_stop"
        return "would_stop"  # pessimistic fallback
    if tp_hit: return "would_tp"
    if stop_hit: return "would_stop"
    return "timeout"


def evaluate(cands, tp_bp, stop_bp):
    tp_n = 0; stop_n = 0; to_n = 0; nd_n = 0
    for ev in cands:
        if ev.get("outcome") == "nodata":
            nd_n += 1; continue
        mf = ev.get("max_favor_bp")
        ma = ev.get("max_adverse_bp")
        if mf is None or ma is None:
            nd_n += 1; continue
        o = outcome_at(mf, ma, ev.get("t_to_tp_ms"), ev.get("t_to_stop_ms"), tp_bp, stop_bp)
        if o == "would_tp": tp_n += 1
        elif o == "would_stop": stop_n += 1
        else: to_n += 1
    scoreable = tp_n + stop_n
    wr = tp_n / scoreable * 100 if scoreable else 0
    gross = tp_n * tp_bp - stop_n * stop_bp
    net_bp = gross - (tp_n + stop_n) * FEE_BP
    net_usd = net_bp / 1e4 * AVG_USD
    return {"tp": tp_n, "stop": stop_n, "to": to_n, "nd": nd_n,
            "wr": wr, "gross_bp": gross, "net_usd": net_usd}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--scored", type=Path, default="/tmp/scored_trajectory.jsonl")
    ap.add_argument("--top", type=int, default=20)
    args = ap.parse_args()

    cands = []
    with open(args.scored) as f:
        for line in f:
            try: cands.append(json.loads(line))
            except: continue
    print(f"Loaded {len(cands)} scored candidates\n")

    # Sweep
    tp_range = [5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 60, 75, 100]
    stop_range = [5, 10, 15, 20, 25, 30, 40, 50, 75, 100]
    results = []
    for tp in tp_range:
        for stop in stop_range:
            r = evaluate(cands, tp, stop)
            r["tp_bp"] = tp; r["stop_bp"] = stop
            results.append(r)

    # Top by net $
    results.sort(key=lambda r: -r["net_usd"])
    print(f"=== TOP {args.top} TP/STOP COMBINATIONS BY NET $ ===")
    print(f"{'TP':>4} {'STOP':>4}  {'tp':>5} {'stop':>5} {'to':>5} {'WR%':>6}  {'net$':>10}")
    for r in results[:args.top]:
        print(f"{r['tp_bp']:>4} {r['stop_bp']:>4}  {r['tp']:>5} {r['stop']:>5} {r['to']:>5} "
              f"{r['wr']:>5.1f}%  ${r['net_usd']:>+8.2f}")

    # Top by WR
    wr_results = sorted([r for r in results if r["tp"] + r["stop"] >= 100], key=lambda r: -r["wr"])
    print(f"\n=== TOP {args.top} by WR (min 100 scoreable) ===")
    print(f"{'TP':>4} {'STOP':>4}  {'tp':>5} {'stop':>5} {'WR%':>6}  {'net$':>10}")
    for r in wr_results[:args.top]:
        print(f"{r['tp_bp']:>4} {r['stop_bp']:>4}  {r['tp']:>5} {r['stop']:>5} "
              f"{r['wr']:>5.1f}%  ${r['net_usd']:>+8.2f}")


if __name__ == "__main__":
    main()

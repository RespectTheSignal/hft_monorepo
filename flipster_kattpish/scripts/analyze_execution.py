#!/usr/bin/env python3
"""Analyze execution slippage from exec_log.jsonl.

For each open/close, shows intended vs actual fill price, and slippage in bp.
Aggregates by event type (open/close) and kind (MAKER/TAKER).
"""
from __future__ import annotations
import argparse, json, statistics as stat
from collections import defaultdict
from pathlib import Path


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", type=Path,
                    default="/home/gate1/projects/quant/flipster_kattpish/logs/exec.jsonl")
    ap.add_argument("--show-samples", type=int, default=10)
    args = ap.parse_args()

    if not args.log.exists():
        print(f"No exec log at {args.log}"); return
    rows = []
    with open(args.log) as f:
        for line in f:
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    if not rows:
        print("Empty log"); return

    print(f"=== Loaded {len(rows)} execution events ===\n")

    # Group
    by_group = defaultdict(list)
    for r in rows:
        key = (r["event"], r["kind"])
        by_group[key].append(r["slip_bp"])
    # Also group close by reason
    by_close_reason = defaultdict(list)
    for r in rows:
        if r["event"] == "close":
            by_close_reason[(r.get("kind"), r.get("reason", "?"))].append(r["slip_bp"])

    print(f"{'group':20s} {'n':>5s} {'mean_slip_bp':>12s} {'median':>10s} "
          f"{'p95':>8s} {'worst':>8s}")
    print("-" * 75)
    for key, slips in sorted(by_group.items()):
        if not slips: continue
        mean = sum(slips) / len(slips)
        med = stat.median(slips)
        p95 = sorted(slips)[int(len(slips) * 0.95)] if len(slips) >= 20 else max(slips)
        worst = max(slips)
        label = f"{key[0]} {key[1]}"
        print(f"{label:20s} {len(slips):>5d} {mean:>+12.2f} {med:>+10.2f} "
              f"{p95:>+8.2f} {worst:>+8.2f}")

    print("\n=== Closes broken by (kind, reason) ===")
    for key, slips in sorted(by_close_reason.items()):
        if not slips: continue
        mean = sum(slips) / len(slips)
        worst = max(slips)
        print(f"  close {key[0]} {key[1]:12s}  n={len(slips):4d}  "
              f"mean_slip=+{mean:6.2f}bp  worst=+{worst:.1f}bp")

    # Show worst N events
    worst_opens = sorted([r for r in rows if r["event"] == "open"],
                         key=lambda r: -r["slip_bp"])[:args.show_samples]
    worst_closes = sorted([r for r in rows if r["event"] == "close"],
                          key=lambda r: -r["slip_bp"])[:args.show_samples]
    print(f"\n=== Top {args.show_samples} worst OPEN slippages ===")
    for r in worst_opens:
        print(f"  {r['sym']:22s} {r['side']:5s} {r['kind']:5s}  "
              f"intended={r['intended']}  actual={r['actual']}  slip=+{r['slip_bp']:.1f}bp")
    print(f"\n=== Top {args.show_samples} worst CLOSE slippages ===")
    for r in worst_closes:
        print(f"  {r['sym']:22s} {r['side']:5s} {r['kind']:5s} {r.get('reason','?'):10s}  "
              f"intended={r['intended']}  actual={r['actual']}  slip=+{r['slip_bp']:.1f}bp")


if __name__ == "__main__":
    main()

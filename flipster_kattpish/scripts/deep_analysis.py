#!/usr/bin/env python3
"""Deep analysis comparing 60s vs 300s configs: per-symbol, stability, drawdown.

Requires pre-scored candidates from discover_filters.py with trajectory fields:
  max_favor_bp, max_adverse_bp, t_to_tp_ms, t_to_stop_ms
"""
from __future__ import annotations
import argparse, json, statistics as stat
from collections import defaultdict
from pathlib import Path

FEE_BP = 8.0
AVG_USD = 50


def outcome(mf, ma, t_tp, t_stop, tp_bp, stop_bp):
    tp_hit = mf >= tp_bp
    stop_hit = abs(ma) >= stop_bp
    if tp_hit and stop_hit:
        if t_tp is not None and t_stop is not None:
            return "tp" if t_tp <= t_stop else "stop"
        return "stop"
    if tp_hit: return "tp"
    if stop_hit: return "stop"
    return "to"


def evaluate_per_sym(cands, tp_bp, stop_bp):
    per_sym = defaultdict(lambda: {"tp":0,"stop":0,"to":0,"nd":0})
    for ev in cands:
        sym = ev["sym"]
        mf = ev.get("max_favor_bp"); ma = ev.get("max_adverse_bp")
        if mf is None or ma is None:
            per_sym[sym]["nd"] += 1; continue
        o = outcome(mf, ma, ev.get("t_to_tp_ms"), ev.get("t_to_stop_ms"), tp_bp, stop_bp)
        per_sym[sym][o] += 1
    # Compute per-sym stats
    out = []
    for sym, s in per_sym.items():
        sc = s["tp"] + s["stop"]
        if sc == 0: continue
        wr = s["tp"] / sc * 100
        gross = s["tp"] * tp_bp - s["stop"] * stop_bp
        net = gross - (s["tp"] + s["stop"]) * FEE_BP
        out.append({"sym": sym, **s, "wr": wr, "net_usd": net/1e4*AVG_USD, "scoreable": sc})
    out.sort(key=lambda x: -x["net_usd"])
    return out


def evaluate_cumulative(cands, tp_bp, stop_bp):
    """Chronological cumulative PnL for drawdown analysis."""
    cands_sorted = sorted([c for c in cands if c.get("max_favor_bp") is not None],
                           key=lambda c: c["ts_ms"])
    cum = 0.0
    peak = 0.0
    max_dd = 0.0
    wins = 0; losses = 0
    trajectory = []
    for ev in cands_sorted:
        o = outcome(ev["max_favor_bp"], ev["max_adverse_bp"],
                    ev.get("t_to_tp_ms"), ev.get("t_to_stop_ms"), tp_bp, stop_bp)
        if o == "tp":
            cum += (tp_bp - FEE_BP) / 1e4 * AVG_USD; wins += 1
        elif o == "stop":
            cum -= (stop_bp + FEE_BP) / 1e4 * AVG_USD; losses += 1
        else:
            continue  # timeout: no pnl in sweep
        peak = max(peak, cum)
        max_dd = max(max_dd, peak - cum)
        trajectory.append(cum)
    return {"final": cum, "peak": peak, "max_dd": max_dd,
            "wins": wins, "losses": losses,
            "trajectory": trajectory}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--scored-60s", type=Path, default="/tmp/scored_trajectory.jsonl")
    ap.add_argument("--scored-300s", type=Path, default="/tmp/scored_300s.jsonl")
    ap.add_argument("--scored-1800s", type=Path, default="/tmp/scored_1800s.jsonl")
    args = ap.parse_args()

    def load(p):
        cands = []
        with open(p) as f:
            for line in f:
                try: cands.append(json.loads(line))
                except: continue
        return cands

    print("Loading data...")
    data = {}
    if args.scored_60s.exists(): data["60s"] = load(args.scored_60s)
    if args.scored_300s.exists(): data["300s"] = load(args.scored_300s)
    if args.scored_1800s.exists(): data["1800s"] = load(args.scored_1800s)
    for k, v in data.items(): print(f"  {k}: {len(v)} candidates")

    # Test specific configs
    configs = {
        "60s_TP30_STOP100": ("60s", 30, 100),
        "60s_TP25_STOP100": ("60s", 25, 100),
        "300s_TP40_STOP100": ("300s", 40, 100),
        "300s_TP35_STOP100": ("300s", 35, 100),
        "300s_TP45_STOP100": ("300s", 45, 100),
        "1800s_TP75_STOP100": ("1800s", 75, 100),
    }

    print("\n=== CUMULATIVE PnL + DRAWDOWN ===")
    print(f"{'config':25s} {'wins':>5} {'losses':>6} {'WR%':>6} {'peak':>9} {'final':>9} {'max_dd':>9}")
    for label, (w, tp, stop) in configs.items():
        if w not in data: continue
        r = evaluate_cumulative(data[w], tp, stop)
        sc = r["wins"] + r["losses"]
        wr = r["wins"]/sc*100 if sc else 0
        print(f"{label:25s} {r['wins']:>5} {r['losses']:>6} {wr:>5.1f}% "
              f"${r['peak']:>+7.2f} ${r['final']:>+7.2f} ${r['max_dd']:>+7.2f}")

    # Per-symbol for top 2 configs
    for label, (w, tp, stop) in [("60s_TP30_STOP100", ("60s", 30, 100)),
                                  ("300s_TP40_STOP100", ("300s", 40, 100))]:
        if w not in data: continue
        print(f"\n=== TOP 15 PROFITABLE SYMBOLS ({label}) ===")
        sym_stats = evaluate_per_sym(data[w], tp, stop)
        profitable = [s for s in sym_stats if s["net_usd"] > 0]
        losing = [s for s in sym_stats if s["net_usd"] < 0]
        print(f"  profitable syms: {len(profitable)} / losing: {len(losing)}")
        for s in sym_stats[:15]:
            print(f"  {s['sym']:22s} n={s['scoreable']:3d}  tp={s['tp']:3d} stop={s['stop']:3d}  "
                  f"WR={s['wr']:.0f}%  net=${s['net_usd']:+.2f}")
        print(f"\n  === BOTTOM 10 (biggest losers) ===")
        for s in sym_stats[-10:]:
            print(f"  {s['sym']:22s} n={s['scoreable']:3d}  tp={s['tp']:3d} stop={s['stop']:3d}  "
                  f"WR={s['wr']:.0f}%  net=${s['net_usd']:+.2f}")

    # Sensitivity: how close to break-even?
    print("\n=== SENSITIVITY — config robustness ===")
    print("300s window, TP variations at STOP=100:")
    for tp in [25, 30, 35, 40, 45, 50, 60]:
        r = evaluate_cumulative(data.get("300s", []), tp, 100)
        sc = r["wins"] + r["losses"]
        wr = r["wins"]/sc*100 if sc else 0
        print(f"  TP={tp}  WR={wr:.1f}%  final=${r['final']:+.2f}  peak=${r['peak']:+.2f}  dd=${r['max_dd']:+.2f}")

    # In/out sample split
    if "300s" in data:
        cands = sorted(data["300s"], key=lambda c: c["ts_ms"])
        mid = len(cands) // 2
        print(f"\n=== IN-SAMPLE (first half) vs OUT-OF-SAMPLE (second half) ===")
        for tp, stop in [(30, 100), (35, 100), (40, 100), (45, 100)]:
            r_in = evaluate_cumulative(cands[:mid], tp, stop)
            r_out = evaluate_cumulative(cands[mid:], tp, stop)
            print(f"  TP={tp} STOP={stop}:  IN=${r_in['final']:+.2f}  OUT=${r_out['final']:+.2f}  "
                  f"IN_wr={r_in['wins']/(r_in['wins']+r_in['losses'])*100 if r_in['wins']+r_in['losses'] else 0:.0f}%  "
                  f"OUT_wr={r_out['wins']/(r_out['wins']+r_out['losses'])*100 if r_out['wins']+r_out['losses'] else 0:.0f}%")


if __name__ == "__main__":
    main()

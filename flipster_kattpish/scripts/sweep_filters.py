#!/usr/bin/env python3
"""Grid-sweep filter configurations over pre-scored candidates.

Input: scored_candidates.jsonl (from discover_filters.py)
       Each line has filter inputs + pre-computed outcome (would_tp/stop/timeout/nodata)

Usage:
  # Evaluate a specific config
  python sweep_filters.py --config single

  # Sweep over threshold ranges for specified filters
  python sweep_filters.py --sweep vel_250_min --values '-inf,-0.5,0.0,0.5,1.0'
"""
from __future__ import annotations
import argparse, json, itertools
from pathlib import Path
from collections import defaultdict

TP_BP = 45.0
STOP_BP = 30.0
FEE_BP = 8.0   # taker round-trip; maker cuts this
AVG_TRADE_USD = 50


def signed_bx(ev, key):
    """For buy: positive = favorable. For sell: negate."""
    v = ev.get(key)
    if v is None: return None
    return v if ev["side"] == "buy" else -v


def passes(ev, cfg):
    # Cross-book edge — always required
    if ev["cross_bp"] < cfg.get("min_edge_bp", 6.0): return False
    # Web age
    if ev.get("web_age_ms") is not None and ev["web_age_ms"] > cfg.get("max_web_age_ms", 5000): return False
    # Orderbook staleness (divergence)
    dv = ev.get("diverge_bp")
    if dv is not None and dv > cfg.get("max_diverge_bp", 2.0): return False
    # Blacklist
    if ev.get("bl_active") and cfg.get("blacklist_enabled", True): return False

    # --- Signed (direction-aware) Binance features ---
    # Velocity 250ms
    v250 = signed_bx(ev, "bx_vel_250ms_bp")
    if v250 is not None:
        # Dead zone rejection
        dz_lo = cfg.get("vel250_dead_lo")
        dz_hi = cfg.get("vel250_dead_hi")
        if dz_lo is not None and dz_hi is not None and dz_lo <= v250 <= dz_hi:
            return False
        # Absolute minimum
        if cfg.get("vel250_min_bp") is not None and v250 < cfg["vel250_min_bp"]:
            return False
    elif cfg.get("vel250_required"): return False

    # Velocity 400ms
    v400 = signed_bx(ev, "bx_vel_400ms_bp")
    if v400 is not None and cfg.get("vel400_min_bp") is not None and v400 < cfg["vel400_min_bp"]:
        return False

    # 1s return
    bx1s = signed_bx(ev, "bx_1000ms_bp")
    if bx1s is not None and cfg.get("bx1s_min_bp") is not None and bx1s < cfg["bx1s_min_bp"]:
        return False

    # 5s return
    bx5s = signed_bx(ev, "bx_5000ms_bp")
    if bx5s is not None and cfg.get("bx5s_min_bp") is not None and bx5s < cfg["bx5s_min_bp"]:
        return False

    # Mono sample count (Binance tick density)
    mn = ev.get("bx_mono_n")
    if mn is not None and cfg.get("mono_n_min") is not None and mn < cfg["mono_n_min"]:
        return False

    # Range
    rg = ev.get("bx_range_1s_bp")
    if rg is not None and cfg.get("range_1s_min_bp") is not None and rg < cfg["range_1s_min_bp"]:
        return False

    # Momentum 3/4 TF (Binance momentum filter)
    if cfg.get("bx_mom_agree_min"):
        agree = 0; n = 0
        for w in (1000, 5000, 10000, 20000):
            v = signed_bx(ev, f"bx_{w}ms_bp")
            if v is None: continue
            n += 1
            if v > 0: agree += 1
        if n >= 3 and agree < cfg["bx_mom_agree_min"]: return False

    # Sig confirm (history)
    if cfg.get("sig_confirm_min") and (ev.get("sig_hist_len", 0) + 1) < cfg["sig_confirm_min"]:
        return False
    # Cross persist
    if cfg.get("cross_persist_min") and len(ev.get("cp_hist", [])) < cfg["cross_persist_min"]:
        return False

    # MIN cross_bp (edge strength)
    if cfg.get("min_edge_strong_bp") and ev["cross_bp"] < cfg["min_edge_strong_bp"]:
        return False

    return True


def evaluate(cands, cfg):
    """Score a config over all pre-scored candidates."""
    total = 0; tp = 0; stop = 0; to_ct = 0; nd = 0
    for ev in cands:
        if not passes(ev, cfg): continue
        total += 1
        o = ev.get("outcome")
        if o == "would_tp": tp += 1
        elif o == "would_stop": stop += 1
        elif o == "timeout": to_ct += 1
        elif o == "nodata": nd += 1
    scoreable = tp + stop
    wr = tp / scoreable * 100 if scoreable else 0
    gross_bp = tp * TP_BP - stop * STOP_BP
    net_bp = gross_bp - (tp + stop) * FEE_BP
    net_usd = net_bp / 1e4 * AVG_TRADE_USD
    return {"entries": total, "tp": tp, "stop": stop, "timeout": to_ct, "nodata": nd,
            "wr_pct": wr, "gross_bp": gross_bp, "net_usd": net_usd}


BASELINE = {
    "min_edge_bp": 6.0,
    "max_web_age_ms": 5000,
    "max_diverge_bp": 2.0,
    "blacklist_enabled": True,
    # These were the original filter set (all disabled in current code but we can toggle)
    "cross_persist_min": 2,
    "sig_confirm_min": 2,
    "bx_mom_agree_min": 3,
    # New candidates
    "vel250_dead_lo": None,  # e.g., -0.8
    "vel250_dead_hi": None,  # e.g., 0.0
    "vel250_min_bp": None,
    "vel400_min_bp": None,
    "bx1s_min_bp": None,
    "bx5s_min_bp": None,
    "mono_n_min": None,
    "range_1s_min_bp": None,
    "min_edge_strong_bp": None,
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--scored", type=Path, default="/tmp/scored_candidates.jsonl")
    ap.add_argument("--mode", choices=("single", "grid", "topk"), default="grid")
    ap.add_argument("--top", type=int, default=20)
    args = ap.parse_args()

    cands = []
    with open(args.scored) as f:
        for line in f:
            try: cands.append(json.loads(line))
            except: continue
    print(f"Loaded {len(cands)} scored candidates")
    base = evaluate(cands, BASELINE)
    print(f"\n=== BASELINE (original full filter stack) ===")
    print(f"  entries={base['entries']} tp={base['tp']} stop={base['stop']} to={base['timeout']} nd={base['nodata']}")
    print(f"  WR={base['wr_pct']:.1f}%  net=${base['net_usd']:+.2f}")

    # Grid sweep over KEY new filter combinations
    configs = []
    # Strategy 1: just dead-zone rejection
    for dz_lo, dz_hi in [(None, None), (-1.0, 0.0), (-0.8, 0.0), (-0.5, 0.2), (-1.0, 0.5)]:
        for bx1s_min in [None, 0, 3, 5, 10]:
            for vel250_min in [None, -1, 0, 0.5]:
                for mono_n_min in [None, 4, 8]:
                    for range_min in [None, 0, 3, 10]:
                        # Disable original filters for isolation
                        cfg = dict(BASELINE)
                        cfg["cross_persist_min"] = None
                        cfg["sig_confirm_min"] = None
                        cfg["bx_mom_agree_min"] = None
                        cfg["vel250_dead_lo"] = dz_lo
                        cfg["vel250_dead_hi"] = dz_hi
                        cfg["vel250_min_bp"] = vel250_min
                        cfg["bx1s_min_bp"] = bx1s_min
                        cfg["mono_n_min"] = mono_n_min
                        cfg["range_1s_min_bp"] = range_min
                        configs.append(cfg)

    # Deduplicate by frozen key
    seen = set()
    uniq = []
    for c in configs:
        key = json.dumps({k: v for k, v in c.items() if k not in ("blacklist_enabled",)}, sort_keys=True, default=str)
        if key not in seen:
            seen.add(key)
            uniq.append(c)
    print(f"\nSweeping {len(uniq)} unique configs...")
    results = []
    for cfg in uniq:
        s = evaluate(cands, cfg)
        if s["entries"] < 50: continue  # need enough sample
        if s["stop"] + s["tp"] < 20: continue
        results.append((cfg, s))

    # Sort by net_usd desc
    results.sort(key=lambda x: -x[1]["net_usd"])
    print(f"\n=== TOP {args.top} CONFIGS BY NET $ ===")
    print(f"{'rank':>4} {'entries':>7} {'tp':>5} {'stop':>5} {'WR%':>6} {'net$':>8}  config (non-None filter params)")
    for i, (cfg, s) in enumerate(results[:args.top]):
        active = {k: v for k, v in cfg.items()
                  if v is not None and k not in ("blacklist_enabled", "min_edge_bp", "max_web_age_ms", "max_diverge_bp")
                  and k not in BASELINE or BASELINE.get(k) != v}
        # Simplify
        simple = []
        for k in ["vel250_dead_lo","vel250_dead_hi","vel250_min_bp","bx1s_min_bp","mono_n_min","range_1s_min_bp"]:
            if cfg.get(k) is not None: simple.append(f"{k}={cfg[k]}")
        print(f"{i+1:>4} {s['entries']:>7} {s['tp']:>5} {s['stop']:>5} {s['wr_pct']:>5.1f}% ${s['net_usd']:>+7.2f}  {' '.join(simple)}")

    # Also sort by WR
    print(f"\n=== TOP {args.top} CONFIGS BY WIN RATE (min 100 entries) ===")
    wr_sorted = sorted([r for r in results if r[1]["entries"] >= 100], key=lambda x: -x[1]["wr_pct"])
    for i, (cfg, s) in enumerate(wr_sorted[:args.top]):
        simple = []
        for k in ["vel250_dead_lo","vel250_dead_hi","vel250_min_bp","bx1s_min_bp","mono_n_min","range_1s_min_bp"]:
            if cfg.get(k) is not None: simple.append(f"{k}={cfg[k]}")
        print(f"{i+1:>4} {s['entries']:>7} {s['tp']:>5} {s['stop']:>5} {s['wr_pct']:>5.1f}% ${s['net_usd']:>+7.2f}  {' '.join(simple)}")


if __name__ == "__main__":
    main()

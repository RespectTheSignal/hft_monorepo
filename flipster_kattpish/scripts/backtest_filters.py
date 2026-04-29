#!/usr/bin/env python3
"""Backtest filter configurations over recorded shadow log.

Reads:
  - logs/shadow.jsonl: cross-book candidates with all filter inputs
  - QuestDB binance_bookticker: ms-resolution price history for outcome scoring

For each candidate, applies a set of filter predicates. Candidates that PASS
all active filters are counted as "entries". For each entry, fetches Binance
bookticker from ts_ms to ts_ms+WINDOW_S and computes:
  - would_tp:   favorable move >= TP_BP before adverse >= STOP_BP
  - would_stop: adverse move >= STOP_BP first
  - timeout:   neither within window

Supports grid sweeps over filter parameters.
"""
from __future__ import annotations
import argparse, json, sys, urllib.request, urllib.parse, itertools
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

QDB_HTTP = "http://211.181.122.102:9000/exec"
AVG_TRADE_USD = 50
TP_BP = 35.0
STOP_BP = 40.0
WINDOW_S = 60
FEE_BP = 8.0   # round-trip taker; optimistic when using maker exit


def fetch_binance(sym: str, t0_ms: int, t1_ms: int):
    bsym = sym.replace("USDT.PERP", "_USDT")
    t0 = datetime.fromtimestamp(t0_ms/1000, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
    t1 = datetime.fromtimestamp(t1_ms/1000, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
    sql = (f"SELECT bid_price, ask_price FROM binance_bookticker "
           f"WHERE symbol='{bsym}' AND timestamp >= '{t0}' AND timestamp <= '{t1}' "
           f"ORDER BY timestamp LIMIT 5000")
    try:
        url = QDB_HTTP + "?" + urllib.parse.urlencode({"query": sql})
        r = json.loads(urllib.request.urlopen(url, timeout=10).read())
        return r.get("dataset", [])
    except Exception:
        return []


def classify(sym: str, side: str, ts_ms: int, tp_bp: float, stop_bp: float, window_s: int):
    rows = fetch_binance(sym, ts_ms, ts_ms + window_s * 1000)
    if not rows:
        return "nodata", 0.0
    first = rows[0]
    entry = (float(first[0]) + float(first[1])) / 2
    if entry <= 0:
        return "nodata", 0.0
    max_favor = 0.0
    max_adverse = 0.0
    for r in rows:
        mid = (float(r[0]) + float(r[1])) / 2
        if mid <= 0: continue
        bp = (mid - entry) / entry * 1e4
        if side == "sell": bp = -bp
        if bp > max_favor: max_favor = bp
        if bp < max_adverse: max_adverse = bp
        if max_favor >= tp_bp and abs(max_adverse) < stop_bp:
            return "would_tp", max_favor
        if abs(max_adverse) >= stop_bp and max_favor < tp_bp:
            return "would_stop", max_adverse
    if max_favor >= tp_bp:
        return "would_tp", max_favor
    if abs(max_adverse) >= stop_bp:
        return "would_stop", max_adverse
    return "timeout", max_favor + max_adverse  # signed sum


def apply_filters(ev: dict, cfg: dict) -> tuple[bool, str]:
    """Return (pass_all, first_failing_filter_name)."""
    # Min edge
    if ev["cross_bp"] < cfg.get("min_edge_bp", 6.0):
        return False, "min_edge"
    # Web age
    if ev.get("web_age_ms") is not None and ev["web_age_ms"] > cfg.get("max_web_age_ms", 5000):
        return False, "web_age"
    # Orderbook staleness (divergence)
    dv = ev.get("diverge_bp")
    if dv is not None and dv > cfg.get("divergence_bp", 2.0):
        return False, "orderbook_stale"
    # Blacklist
    if ev.get("bl_active") and cfg.get("blacklist_enabled", True):
        return False, "blacklist"
    # Cross persistence
    if cfg.get("cross_persist_enabled", True):
        hist = ev.get("cp_hist", [])
        need = cfg.get("cross_persist_count", 2)
        direction = +1 if ev["side"] == "buy" else -1
        if len(hist) < need or not all(d == direction for d in hist[-need:]):
            return False, "cross_persist"
    # Edge shrink (not growing)
    if cfg.get("edge_shrink_enabled", False):
        prev = ev.get("prev_edge", 0.0) or 0.0
        if prev > 0 and ev["cross_bp"] < prev - 0.3:
            return False, "edge_shrink"
    # Recent velocity
    if cfg.get("vel_decay_enabled", True):
        w = cfg.get("vel_window_ms", 400)
        key = f"bx_vel_{w}ms_bp"
        vel = ev.get(key)
        thr = cfg.get("vel_min_bp", 0.5)
        if vel is not None:
            if ev["side"] == "buy" and vel < thr: return False, "vel_decay"
            if ev["side"] == "sell" and vel > -thr: return False, "vel_decay"
    # Sig confirm
    if cfg.get("sig_confirm_enabled", True):
        need = cfg.get("sig_confirm_count", 2)
        # sig_hist_len is the length BEFORE this signal appended; actual count = +1
        if (ev.get("sig_hist_len", 0) + 1) < need:
            return False, "sig_confirm"
    # Mono (Binance monotonicity)
    if cfg.get("mono_enabled", True):
        n = ev.get("bx_mono_n")
        if n is not None and n >= 4:
            move = ev.get("bx_mono_bp", 0)
            up = ev.get("bx_mono_up", 0)
            dn = ev.get("bx_mono_dn", 0)
            total = up + dn
            half_edge = cfg.get("min_edge_bp", 6.0) * 0.5
            wrong = dn if ev["side"] == "buy" else up
            rev = wrong / total if total else 0
            if ev["side"] == "buy" and move < half_edge: return False, "mono"
            if ev["side"] == "sell" and move > -half_edge: return False, "mono"
            if rev > cfg.get("mono_max_rev", 0.3): return False, "mono"
    # Vol gate
    if cfg.get("vol_enabled", True):
        rg = ev.get("bx_range_1s_bp")
        if rg is not None and rg < cfg.get("vol_min_range_bp", 3.0):
            return False, "vol"
    # Binance momentum 3/4 TF
    if cfg.get("bx_momentum_enabled", True):
        agree = 0
        n_checked = 0
        for w in (1000, 5000, 10000, 20000):
            bp = ev.get(f"bx_{w}ms_bp")
            if bp is None: continue
            n_checked += 1
            if (ev["side"] == "buy" and bp > 0) or (ev["side"] == "sell" and bp < 0):
                agree += 1
        if n_checked >= 3 and agree < cfg.get("bx_momentum_min_agree", 3):
            return False, "bx_momentum"
    return True, ""


def score_config(candidates: list, cfg: dict, outcome_cache: dict, cap_entries: int = 500) -> dict:
    stats = {"candidates": len(candidates), "entries": 0,
             "wins": 0, "losses": 0, "timeouts": 0, "nodata": 0,
             "gross_bp": 0.0, "net_bp": 0.0,
             "by_reject": defaultdict(int)}
    sample_entries = []
    for ev in candidates:
        passed, failed_at = apply_filters(ev, cfg)
        if not passed:
            stats["by_reject"][failed_at] += 1
            continue
        stats["entries"] += 1
        if len(sample_entries) < cap_entries:
            sample_entries.append(ev)
    # Score entries (cap for speed)
    sample_step = max(1, stats["entries"] // cap_entries)
    step = 0
    scored_count = 0
    for ev in candidates:
        passed, _ = apply_filters(ev, cfg)
        if not passed: continue
        step += 1
        if step % sample_step != 0: continue
        if scored_count >= cap_entries: break
        scored_count += 1
        cache_key = (ev["sym"], ev["side"], ev["ts_ms"])
        if cache_key not in outcome_cache:
            outcome_cache[cache_key] = classify(ev["sym"], ev["side"], ev["ts_ms"],
                                                cfg.get("tp_bp", TP_BP),
                                                cfg.get("stop_bp", STOP_BP),
                                                cfg.get("window_s", WINDOW_S))
        outcome, bp = outcome_cache[cache_key]
        if outcome == "would_tp":
            stats["wins"] += 1
            stats["gross_bp"] += cfg.get("tp_bp", TP_BP)
        elif outcome == "would_stop":
            stats["losses"] += 1
            stats["gross_bp"] -= cfg.get("stop_bp", STOP_BP)
        elif outcome == "nodata":
            stats["nodata"] += 1
        else:
            stats["timeouts"] += 1
            # treat as zero for now
    # Scale sample → full
    if scored_count > 0:
        scale = stats["entries"] / scored_count
        stats["gross_bp"] *= scale
    stats["net_bp"] = stats["gross_bp"] - (stats["wins"] + stats["losses"]) * FEE_BP
    stats["gross_usd"] = stats["gross_bp"] / 10000 * AVG_TRADE_USD
    stats["net_usd"] = stats["net_bp"] / 10000 * AVG_TRADE_USD
    return stats


BASELINE_CONFIG = {
    "min_edge_bp": 6.0,
    "max_web_age_ms": 5000,
    "divergence_bp": 2.0,
    "blacklist_enabled": True,
    "cross_persist_enabled": True,
    "cross_persist_count": 2,
    "edge_shrink_enabled": False,
    "vel_decay_enabled": True,
    "vel_window_ms": 400,
    "vel_min_bp": 0.5,
    "sig_confirm_enabled": True,
    "sig_confirm_count": 2,
    "mono_enabled": True,
    "mono_max_rev": 0.3,
    "vol_enabled": True,
    "vol_min_range_bp": 3.0,
    "bx_momentum_enabled": True,
    "bx_momentum_min_agree": 3,
    "tp_bp": 35.0,
    "stop_bp": 40.0,
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--shadow", type=Path, default="/home/gate1/projects/quant/flipster_kattpish/logs/shadow.jsonl")
    ap.add_argument("--cap-entries", type=int, default=300)
    ap.add_argument("--mode", choices=("baseline", "ablate", "sweep"), default="baseline")
    ap.add_argument("--sweep", type=str, default="vel_min_bp",
                    help="filter param to sweep (for mode=sweep)")
    ap.add_argument("--values", type=str, default="0.2,0.5,1.0,1.5",
                    help="comma-separated values for sweep")
    args = ap.parse_args()

    if not args.shadow.exists():
        print(f"No shadow log at {args.shadow}"); sys.exit(1)
    candidates = []
    with open(args.shadow) as f:
        for line in f:
            try:
                ev = json.loads(line)
                if "cross_bp" in ev and "ts_ms" in ev and "sym" in ev:
                    candidates.append(ev)
            except json.JSONDecodeError:
                continue
    print(f"Loaded {len(candidates)} candidates from {args.shadow}")
    if not candidates:
        return
    outcome_cache = {}

    def run(cfg, label):
        s = score_config(candidates, cfg, outcome_cache, cap_entries=args.cap_entries)
        print(f"\n--- {label} ---")
        print(f"  entries: {s['entries']:5d}/{s['candidates']}  "
              f"(rejected: {sum(s['by_reject'].values())})")
        print(f"  wins: {s['wins']}  losses: {s['losses']}  timeouts: {s['timeouts']}  "
              f"nodata: {s['nodata']}")
        print(f"  gross: {s['gross_bp']:+.0f}bp = ${s['gross_usd']:+.2f}")
        print(f"  net (after {FEE_BP}bp fees): ${s['net_usd']:+.2f}")
        top_reject = sorted(s['by_reject'].items(), key=lambda x: -x[1])[:5]
        print(f"  top rejects: {', '.join(f'{k}={v}' for k,v in top_reject)}")
        return s

    if args.mode == "baseline":
        run(BASELINE_CONFIG, "BASELINE (current live config)")
    elif args.mode == "ablate":
        run(BASELINE_CONFIG, "BASELINE")
        # Turn off each filter one at a time
        for flt in ["cross_persist", "sig_confirm", "vel_decay", "mono", "vol", "bx_momentum"]:
            cfg = dict(BASELINE_CONFIG)
            cfg[f"{flt}_enabled"] = False
            run(cfg, f"WITHOUT {flt}")
        # Turn off ALL of them
        cfg = dict(BASELINE_CONFIG)
        for flt in ["cross_persist", "sig_confirm", "vel_decay", "mono", "vol", "bx_momentum"]:
            cfg[f"{flt}_enabled"] = False
        run(cfg, "WITHOUT ALL (only cross-book + orderbook_stale + web_age + blacklist)")
    elif args.mode == "sweep":
        vals = [float(v) for v in args.values.split(",")]
        run(BASELINE_CONFIG, "BASELINE")
        for v in vals:
            cfg = dict(BASELINE_CONFIG)
            cfg[args.sweep] = v
            run(cfg, f"{args.sweep} = {v}")


if __name__ == "__main__":
    main()

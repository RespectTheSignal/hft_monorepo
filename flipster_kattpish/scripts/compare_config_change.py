#!/usr/bin/env python3
"""Compare gate_lead PnL before vs after each config change marker.

Reads `logs/config_changes.jsonl` for change timestamps. For each
marker, fetches positions/history from Flipster API (via NRT g1) and
buckets closes into (before, after) windows. Computes net PnL with
85% VIP11 rebate, slippage vs paper mid PnL, and full-vs-shrunk
breakdown.

Run anytime:
  scripts/compare_config_change.py
  scripts/compare_config_change.py --window 1800   # ±30min instead of default ±2h
  scripts/compare_config_change.py --marker-idx -1 # most recent only
"""
import argparse
import datetime
import json
import subprocess
import sys
import urllib.parse
import urllib.request
from pathlib import Path

PROJECT = Path(__file__).resolve().parent.parent
MARKER_FILE = PROJECT / "logs" / "config_changes.jsonl"
NRT_IP = "198.13.32.68"  # g1


def fetch_history() -> list[dict]:
    cmd = f"""python3 -c "
import json, urllib.request
ck = json.loads(open('/root/.config/flipster_kattpish/cookies.json').read())['flipster']
cookie = '; '.join(f'{{k}}={{v}}' for k,v in ck.items())
req = urllib.request.Request(
    'https://api.flipster.io/api/v2/positions/history?limit=500',
    headers={{'User-Agent':'Mozilla/5.0','Origin':'https://flipster.io','Cookie':cookie}}
)
print(urllib.request.urlopen(req,timeout=15).read().decode())
" """
    r = subprocess.run(
        ["ssh", "-o", "ConnectTimeout=10", "-o", "BatchMode=yes",
         f"root@{NRT_IP}", cmd],
        capture_output=True, text=True, timeout=30,
    )
    return json.loads(r.stdout).get("history", [])


def fetch_position_log(t_min_iso: str, t_max_iso: str) -> tuple[list, list[str]]:
    sql = (f"SELECT symbol, side, exit_reason, entry_price, exit_price, "
           f"size, pnl_bp, timestamp FROM position_log WHERE strategy='gate_lead' "
           f"AND timestamp >= '{t_min_iso}' AND timestamp <= '{t_max_iso}' "
           f"ORDER BY timestamp ASC")
    url = "http://211.181.122.102:9000/exec?query=" + urllib.parse.quote(sql)
    r = json.loads(urllib.request.urlopen(url, timeout=15).read())
    return r.get("dataset", []), [c["name"] for c in r.get("columns", [])]


def base_of(sym: str) -> str:
    return sym.replace(".PERP", "").replace("USDT", "")


def to_ms(iso: str) -> int:
    dt = datetime.datetime.strptime(iso[:19], "%Y-%m-%dT%H:%M:%S").replace(tzinfo=datetime.UTC)
    return int(dt.timestamp() * 1000) + (int(iso[20:26]) // 1000 if len(iso) > 20 else 0)


def analyze(api: list[dict], pl: list, cols: list[str]) -> dict:
    """Match API closes with position_log; aggregate."""
    joined = []
    for ap in api:
        base = base_of(ap.get("symbol", ""))
        side = ap.get("side", "").lower()
        api_open_ms = int(ap.get("openTime", 0)) / 1e6
        api_entry = float(ap.get("avgOpenPrice", 0))
        api_exit = float(ap.get("avgClosePrice", 0))
        api_qty = float(ap.get("qty", 0))
        api_notional = api_entry * api_qty
        gross = float(ap["grossRealizedPnl"])
        fee_eff = float(ap["tradingFee"]) * 0.15

        best, best_dt = None, 1e18
        for row in pl:
            rec = dict(zip(cols, row))
            if rec["symbol"] != base or rec["side"] != side:
                continue
            try:
                rec_ms = to_ms(rec["timestamp"])
            except Exception:
                continue
            dt = abs(rec_ms - api_open_ms)
            if dt < best_dt:
                best_dt, best = dt, rec
        if best and best_dt < 5000:
            sgn = 1 if side == "long" else -1
            paper_bp = best["pnl_bp"]
            actual_bp = (api_exit - api_entry) / api_entry * 1e4 * sgn if api_entry > 0 else None
            net_usd = gross - fee_eff
            net_bp = (net_usd / api_notional * 1e4) if api_notional > 0 else None
            joined.append({
                "ts_ms": api_open_ms, "base": base, "side": side,
                "reason": best.get("exit_reason", "?"),
                "paper_bp": paper_bp, "actual_bp": actual_bp,
                "slip_bp": (paper_bp - actual_bp) if actual_bp is not None else None,
                "paper_size": best["size"], "actual_notional": api_notional,
                "gross_usd": gross, "net_usd_rebate": net_usd, "net_bp_rebate": net_bp,
            })
    return joined


def summarize(joined: list, label: str) -> None:
    n = len(joined)
    if n == 0:
        print(f"  {label}: (no matched trades)")
        return
    have = [j for j in joined if j["actual_bp"] is not None]
    paper = sum(j["paper_bp"] for j in joined) / n
    actual = sum(j["actual_bp"] for j in have) / max(len(have), 1)
    net_bp = sum(j["net_bp_rebate"] for j in have if j["net_bp_rebate"]) / max(len(have), 1)
    net_usd = sum(j["net_usd_rebate"] for j in joined)
    win = sum(1 for j in have if j["actual_bp"] > 0) / max(len(have), 1) * 100
    avg_notional = sum(j["actual_notional"] for j in joined) / n
    full = [j for j in have if j["actual_notional"] >= 0.85 * j["paper_size"]]
    full_share = len(full) / max(len(have), 1) * 100
    print(f"  {label:>14s}  n={n:3d}  paper={paper:+5.1f}bp  actual={actual:+5.1f}bp  "
          f"net@rebate={net_bp:+5.2f}bp  ${net_usd:+6.2f}  win={win:.0f}%  "
          f"size avg=${avg_notional:.0f}  full%={full_share:.0f}")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--window", type=int, default=7200,
                   help="comparison window each side of marker (sec, default 2h)")
    p.add_argument("--marker-idx", type=int, default=None,
                   help="single marker by index (-1 = most recent)")
    args = p.parse_args()

    if not MARKER_FILE.exists():
        print("no markers yet — make a config change first")
        sys.exit(1)

    markers = [json.loads(l) for l in MARKER_FILE.open() if l.strip()]
    if args.marker_idx is not None:
        markers = [markers[args.marker_idx]]

    print(f"fetching Flipster API history…")
    api = fetch_history()
    print(f"  got {len(api)} closes\n")
    if not api:
        print("API returned nothing")
        return

    api_times = [int(p["openTime"]) / 1e9 for p in api]
    api_t_min, api_t_max = min(api_times), max(api_times)

    for m in markers:
        ts = m["ts"]
        before_lo = ts - args.window
        after_hi = ts + args.window
        # If API window doesn't span this marker, warn
        if api_t_max < before_lo or api_t_min > after_hi:
            print(f"\n=== marker @ {m['iso'][:19]} — NOT IN API WINDOW (API has {api_t_min:.0f}..{api_t_max:.0f}, marker={ts}) ===")
            continue

        before = [p for p in api if before_lo <= int(p["openTime"]) / 1e9 < ts]
        after = [p for p in api if ts <= int(p["openTime"]) / 1e9 < after_hi]

        # position_log over full window
        iso_lo = datetime.datetime.fromtimestamp(before_lo - 30, datetime.UTC).strftime("%Y-%m-%dT%H:%M:%S.000000Z")
        iso_hi = datetime.datetime.fromtimestamp(after_hi + 30, datetime.UTC).strftime("%Y-%m-%dT%H:%M:%S.000000Z")
        pl, cols = fetch_position_log(iso_lo, iso_hi)

        joined_before = analyze(before, pl, cols)
        joined_after = analyze(after, pl, cols)

        print(f"\n=== marker @ {m['iso'][:19]} : {m.get('change','?')} ===")
        print(f"  window ±{args.window//60}min, before n={len(before)} after n={len(after)}")
        summarize(joined_before, "BEFORE")
        summarize(joined_after, "AFTER")


if __name__ == "__main__":
    main()

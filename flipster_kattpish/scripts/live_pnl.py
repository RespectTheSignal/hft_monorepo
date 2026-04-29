#!/usr/bin/env python3
"""Read live_trades.jsonl and print PnL summary.

Usage:
    python3 scripts/live_pnl.py                # all trades
    python3 scripts/live_pnl.py --recent 1h    # last hour
    python3 scripts/live_pnl.py --recent 30m   # last 30 minutes
    python3 scripts/live_pnl.py --by-symbol    # group by symbol
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

LOG = Path(__file__).resolve().parent.parent / "logs" / "live_trades.jsonl"


def parse_recent(s: str) -> timedelta:
    if s.endswith("h"):
        return timedelta(hours=float(s[:-1]))
    if s.endswith("m"):
        return timedelta(minutes=float(s[:-1]))
    if s.endswith("s"):
        return timedelta(seconds=float(s[:-1]))
    raise ValueError(f"unknown time format: {s}")


def load_trades(recent: timedelta | None = None) -> list[dict]:
    if not LOG.exists():
        return []
    cutoff = None
    if recent:
        cutoff = datetime.now(timezone.utc) - recent
    out = []
    with open(LOG) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                t = json.loads(line)
            except json.JSONDecodeError:
                continue
            if cutoff:
                ts = t.get("ts_close", "")
                try:
                    dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
                    if dt < cutoff:
                        continue
                except Exception:
                    pass
            out.append(t)
    return out


def summarize(trades: list[dict]) -> dict:
    if not trades:
        return {"n": 0}
    n = len(trades)
    pnl_list = [t["net_after_fees_usd"] for t in trades]
    gross_list = [t["net_pnl_usd"] for t in trades]
    fees_list = [t["approx_fee_usd"] for t in trades]
    wins = sum(1 for x in pnl_list if x > 0)
    return {
        "n": n,
        "wins": wins,
        "win_pct": wins / n * 100 if n else 0,
        "total_net": sum(pnl_list),
        "total_gross": sum(gross_list),
        "total_fees": sum(fees_list),
        "avg_net": sum(pnl_list) / n,
        "best": max(pnl_list),
        "worst": min(pnl_list),
    }


def by_symbol(trades: list[dict]) -> dict[str, dict]:
    groups = {}
    for t in trades:
        groups.setdefault(t["base"], []).append(t)
    return {sym: summarize(ts) for sym, ts in groups.items()}


def fmt(v: float, w: int = 8, prec: int = 4) -> str:
    return f"{v:+{w}.{prec}f}"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--recent", type=str, help="e.g. 1h, 30m")
    p.add_argument("--by-symbol", action="store_true")
    p.add_argument("--last", type=int, default=10, help="show last N trades")
    args = p.parse_args()

    recent = parse_recent(args.recent) if args.recent else None
    trades = load_trades(recent)

    if not trades:
        print("(no trades)")
        return

    s = summarize(trades)
    period = f"recent {args.recent}" if args.recent else "all-time"
    print(f"=== Live PnL ({period}) ===")
    print(f"trades:    {s['n']}  ({s['wins']} wins = {s['win_pct']:.1f}%)")
    print(f"gross:     ${s['total_gross']:+.4f}")
    print(f"fees:      ${s['total_fees']:.4f}")
    print(f"net:       ${s['total_net']:+.4f}")
    print(f"avg/trade: ${s['avg_net']:+.4f}")
    print(f"best:      ${s['best']:+.4f}")
    print(f"worst:     ${s['worst']:+.4f}")

    if args.by_symbol:
        print("\n=== By symbol ===")
        groups = by_symbol(trades)
        print(f"{'symbol':<12} {'n':>4} {'win%':>6} {'net$':>10} {'avg$':>9}")
        for sym, st in sorted(groups.items(), key=lambda x: -x[1]["total_net"]):
            print(f"{sym:<12} {st['n']:>4} {st['win_pct']:>5.1f}% {st['total_net']:>+10.4f} {st['avg_net']:>+9.4f}")

    print(f"\n=== Last {min(args.last, len(trades))} trades ===")
    print(f"{'time':<20} {'sym':<10} {'side':<5} {'flip$':>9} {'gate$':>9} {'net$':>9}")
    for t in trades[-args.last:]:
        ts = t["ts_close"][11:19] if len(t["ts_close"]) > 19 else t["ts_close"]
        print(f"{t['ts_close'][:19]:<20} {t['base']:<10} {t['flipster_side']:<5} "
              f"{t['f_pnl_usd']:>+9.4f} {t['g_pnl_usd']:>+9.4f} "
              f"{t['net_after_fees_usd']:>+9.4f}")


if __name__ == "__main__":
    main()

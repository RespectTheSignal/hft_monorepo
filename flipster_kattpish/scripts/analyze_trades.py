#!/usr/bin/env python3
"""Parse flipster_latency_pick.py log: aggregate CLOSE events, surface losing patterns.

Produces:
  - Loss cause distribution (by reason, by symbol)
  - Top N losing trades with entry/exit context
  - JSON dump of parsed trades for downstream plotting (--out-json)
"""
from __future__ import annotations
import argparse, json, re, sys
from collections import defaultdict
from pathlib import Path

LOG = Path("/home/gate1/projects/quant/flipster_kattpish/logs/latency_pick.log")

RE_OPEN = re.compile(
    r"^\[(?P<ts>\d{2}:\d{2}:\d{2})\] \[SIG#(?P<sig>\d+)\] (?P<sym>\S+) "
    r"diff=(?P<diff>[+-]?\d+\.\d+)bp web_age=(?P<age>\d+)ms .*→ (?:open|flip)→(?P<side>\w+)"
)
RE_OPEN_FILL = re.compile(
    r"^  \[OPEN#(?P<n>\d+)\] (?P<sym>\S+) (?P<side>\w+) avg=(?P<avg>[\d.]+)"
)
RE_CLOSE = re.compile(
    r"^  \[CLOSE\] (?P<sym>\S+) (?P<side>\w+) reason=(?P<reason>\S+) "
    r"entry=(?P<entry>[\d.]+) exit=(?P<exit>[\d.]+) move=(?P<move>[+-]?\d+\.\d+)bp "
    r"hold=(?P<hold>\d+)ms pnl=\$(?P<pnl>[+-]?\d+\.\d+) cum=\$(?P<cum>[+-]?\d+\.\d+)"
)


def parse(log_path: Path) -> list[dict]:
    trades = []
    # pending[sym] = list of open records awaiting close
    pending: dict[str, list[dict]] = defaultdict(list)
    last_sig: dict[str, dict] = {}  # sym -> latest signal context
    for line in log_path.read_text().splitlines():
        m = RE_OPEN.match(line)
        if m:
            last_sig[m["sym"]] = {
                "sig_ts": m["ts"], "diff_bp": float(m["diff"]),
                "web_age_ms": int(m["age"]), "side": m["side"],
            }
            continue
        m = RE_OPEN_FILL.match(line)
        if m:
            sig = last_sig.get(m["sym"])
            pending[m["sym"]].append({
                "sym": m["sym"], "side": m["side"],
                "entry_avg": float(m["avg"]),
                "open_n": int(m["n"]),
                "sig_ts": sig["sig_ts"] if sig else None,
                "sig_diff_bp": sig["diff_bp"] if sig else None,
                "sig_web_age_ms": sig["web_age_ms"] if sig else None,
            })
            continue
        m = RE_CLOSE.match(line)
        if m:
            stack = pending.get(m["sym"])
            op = stack.pop(0) if stack else {}  # match FIFO (may be empty for adopted)
            trades.append({
                "sym": m["sym"],
                "side": m["side"],
                "entry_avg": op.get("entry_avg"),
                "open_n": op.get("open_n"),
                "sig_ts": op.get("sig_ts"),
                "sig_diff_bp": op.get("sig_diff_bp"),
                "sig_web_age_ms": op.get("sig_web_age_ms"),
                "adopted": op == {},  # True if no matching OPEN found
                "close_reason": m["reason"],
                "entry_price": float(m["entry"]),
                "exit_price": float(m["exit"]),
                "move_bp": float(m["move"]),
                "hold_ms": int(m["hold"]),
                "pnl": float(m["pnl"]),
                "cum_pnl": float(m["cum"]),
            })
    return trades


def bucketize_reason(r: str) -> str:
    if r.startswith("tp"): return "tp"
    if r.startswith("stop"): return "stop"
    if r.startswith("max_hold"): return "max_hold"
    return r.split("(")[0] or "other"


def summarize(trades: list[dict]) -> None:
    if not trades:
        print("no trades parsed")
        return
    n = len(trades)
    tp_pnl = sum(t["pnl"] for t in trades if t["pnl"] > 0)
    loss_pnl = sum(t["pnl"] for t in trades if t["pnl"] <= 0)
    wins = [t for t in trades if t["pnl"] > 0]
    losses = [t for t in trades if t["pnl"] < 0]
    breakevens = [t for t in trades if t["pnl"] == 0]

    print(f"=== Trades: {n}   wins={len(wins)} ({len(wins)/n*100:.1f}%)  "
          f"losses={len(losses)}  be={len(breakevens)} ===")
    print(f"Gross win:  ${tp_pnl:+.2f}")
    print(f"Gross loss: ${loss_pnl:+.2f}")
    print(f"Net:        ${tp_pnl + loss_pnl:+.2f}")
    if wins:
        print(f"Avg win:    ${sum(t['pnl'] for t in wins)/len(wins):+.3f}  "
              f"({sum(t['move_bp'] for t in wins)/len(wins):+.1f}bp)")
    if losses:
        print(f"Avg loss:   ${sum(t['pnl'] for t in losses)/len(losses):+.3f}  "
              f"({sum(t['move_bp'] for t in losses)/len(losses):+.1f}bp)")

    # By close reason
    print("\n=== By close reason ===")
    by_reason = defaultdict(list)
    for t in trades:
        by_reason[bucketize_reason(t["close_reason"])].append(t)
    for reason, ts in sorted(by_reason.items(), key=lambda kv: -sum(t["pnl"] for t in kv[1])):
        total = sum(t["pnl"] for t in ts)
        avg_bp = sum(t["move_bp"] for t in ts) / len(ts)
        avg_hold = sum(t["hold_ms"] for t in ts) / len(ts) / 1000
        wl = sum(1 for t in ts if t["pnl"] > 0)
        print(f"  {reason:12} n={len(ts):4d}  total=${total:+.2f}  "
              f"avg={avg_bp:+.1f}bp  hold={avg_hold:.0f}s  wins={wl}/{len(ts)}")

    # By symbol — worst 15
    print("\n=== Worst 15 symbols (by net $) ===")
    by_sym = defaultdict(list)
    for t in trades:
        by_sym[t["sym"]].append(t)
    worst = sorted(by_sym.items(), key=lambda kv: sum(t["pnl"] for t in kv[1]))[:15]
    for sym, ts in worst:
        total = sum(t["pnl"] for t in ts)
        reasons = defaultdict(int)
        for t in ts:
            reasons[bucketize_reason(t["close_reason"])] += 1
        rstr = " ".join(f"{k}={v}" for k, v in reasons.items())
        print(f"  {sym:22}  ${total:+7.2f}  n={len(ts):2d}  {rstr}")

    # Top individual losing trades
    print("\n=== Top 15 losing trades ===")
    worst_t = sorted(trades, key=lambda t: t["pnl"])[:15]
    for t in worst_t:
        sig_diff = f"{t.get('sig_diff_bp', '?')}" if t.get("sig_diff_bp") is not None else "?"
        print(f"  {t.get('sig_ts','?')} {t['sym']:22} {t['side']:4} "
              f"${t['pnl']:+6.2f}  move={t['move_bp']:+7.1f}bp  "
              f"hold={t['hold_ms']/1000:5.1f}s  reason={t['close_reason']}  "
              f"sig_diff={sig_diff}bp")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", type=Path, default=LOG)
    ap.add_argument("--out-json", type=Path, default=None,
                    help="Dump parsed trades as JSON for downstream plotting")
    args = ap.parse_args()
    trades = parse(args.log)
    summarize(trades)
    if args.out_json:
        args.out_json.write_text(json.dumps(trades, indent=1))
        print(f"\n→ {len(trades)} trades dumped to {args.out_json}")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Real-time PnL dashboard for flipster_latency_pick bot.

Fees (Flipster VIP, AFTER rebate applied — final settled cost):
    maker: 0.15 bp   |   taker: 0.425 bp
Round-trip worst case (maker open + taker close): 0.575 bp

Bot logs gross price PnL only (no fees). Exchange balance shows GROSS fees
deducted (rebate credits later). This dashboard computes the TRUE net PnL
the account will settle at once rebates are applied.

Usage:
    python3 scripts/dashboard_pnl.py                  # snapshot
    watch -n 5 python3 scripts/dashboard_pnl.py       # auto-refresh 5s
    python3 scripts/dashboard_pnl.py --log path.log   # specific log
"""
from __future__ import annotations
import argparse, re, os, sys, time
from collections import defaultdict
from pathlib import Path

MAKER_FEE_BP = 0.15
TAKER_FEE_BP = 0.425
AMOUNT_USD = 5.0  # default; parsed from log cfg if found

LOG_PATH = Path(__file__).resolve().parent.parent / "logs" / "latency_pick.log"


def fee_bp_for_close(close_kind: str) -> float:
    """Entry is always MAKER (post-only LIMIT). Close fee depends on kind."""
    if close_kind in ("MAKER", "BRACKET_TP"):
        return MAKER_FEE_BP + MAKER_FEE_BP  # maker-maker round trip
    return MAKER_FEE_BP + TAKER_FEE_BP       # maker-taker round trip


def parse_log(log_path: Path):
    if not log_path.exists():
        return None
    txt = log_path.read_text()
    # cfg amount override
    global AMOUNT_USD
    m = re.search(r"amount=\$([\d.]+)", txt)
    if m:
        AMOUNT_USD = float(m.group(1))

    closes = []
    for line in txt.splitlines():
        # BRACKET_TP has its own format (no reason= field)
        m = re.match(
            r"  \[CLOSE:(BRACKET_TP)\] (\S+) \w+ tp=[\d.]+ "
            r"move=([+-][\d.]+)bp hold=(\d+)ms pnl=\$(-?[\d.]+)", line)
        if m:
            kind, sym, move, hold, pnl = m.groups()
            closes.append({"kind": kind, "reason": "bracket_tp", "sym": sym,
                           "move_bp": float(move), "hold_ms": int(hold),
                           "gross_pnl": float(pnl)})
            continue
        m = re.match(
            r"  \[CLOSE:(\w+)\] (\S+) \w+ reason=(\w+)\S* .*?"
            r"move=([+-][\d.]+)bp hold=(\d+)ms pnl=\$(-?[\d.]+)", line)
        if m:
            kind, sym, reason, move, hold, pnl = m.groups()
            closes.append({"kind": kind, "reason": reason, "sym": sym,
                           "move_bp": float(move), "hold_ms": int(hold),
                           "gross_pnl": float(pnl)})

    # Status (latest)
    status = None
    for m in re.finditer(r"\[status t=(\d+)s\] sig=(\d+) open=(\d+) close=(\d+) "
                         r"flip=\d+ skip=(\d+) open_pos=(\d+) tracker=(-?\d+) "
                         r"cum_pnl=\$(-?[\d.]+)", txt):
        status = {"t_s": int(m.group(1)), "sig": int(m.group(2)),
                  "open": int(m.group(3)), "close": int(m.group(4)),
                  "skip": int(m.group(5)), "open_pos": int(m.group(6)),
                  "tracker": int(m.group(7)), "cum_pnl": float(m.group(8))}

    # Cancel stats
    cancel_converged = len(re.findall(r"\[ENTRY_CANCEL:converged", txt))
    cancel_timeout   = len(re.findall(r"\[ENTRY_CANCEL:timeout", txt))
    late_fills       = len(re.findall(r"\[LATE_FILL\]", txt))

    return closes, status, {"cancel_converged": cancel_converged,
                            "cancel_timeout": cancel_timeout,
                            "late_fills": late_fills}


def fmt_bp(x): return f"{x:+.2f}bp"
def fmt_usd(x): return f"${x:+.4f}"


def render(closes, status, extras):
    print("\033[2J\033[H", end="")  # clear + home
    now = time.strftime("%Y-%m-%d %H:%M:%S")
    elapsed_min = status["t_s"]/60 if status else 0
    print(f"=== FLIPSTER LATENCY-PICK LIVE DASHBOARD ===  {now}")
    print(f"Session: {elapsed_min:.1f} min  | amount=${AMOUNT_USD:.2f} "
          f"| maker={MAKER_FEE_BP}bp  taker={TAKER_FEE_BP}bp")
    if status:
        print(f"signals={status['sig']}  opens={status['open']}  "
              f"closes={status['close']}  skips={status['skip']}  "
              f"open_pos={status['open_pos']}  tracker={status['tracker']}")
    print(f"entry cancels: converged={extras['cancel_converged']}  "
          f"timeout={extras['cancel_timeout']}  late_fills={extras['late_fills']}")
    print()

    if not closes:
        print("(no closes yet)")
        return

    stats = defaultdict(lambda: {"n":0, "wins":0, "move_sum":0.0,
                                  "gross":0.0, "fees":0.0})
    for c in closes:
        s = stats[c["reason"]]
        s["n"] += 1
        if c["gross_pnl"] > 0: s["wins"] += 1
        s["move_sum"] += c["move_bp"]
        s["gross"] += c["gross_pnl"]
        fee_bp = fee_bp_for_close(c["kind"])
        s["fees"] += AMOUNT_USD * fee_bp / 1e4
        s.setdefault("fee_bp_rt", fee_bp)  # tracks most recent; for display

    # Header
    print(f"{'reason':13s} {'n':>4s} {'WR':>6s} {'avg_move':>10s} "
          f"{'fee/rt':>7s} {'gross':>10s} {'fees':>9s} {'net':>10s}")
    print("-" * 78)
    order = ["bracket_tp", "tp_fallback", "max_hold", "stop", "hard_stop"]
    other = sorted(k for k in stats if k not in order)
    tot = {"n":0, "wins":0, "gross":0.0, "fees":0.0}
    for r in order + other:
        if r not in stats: continue
        s = stats[r]
        wr = s["wins"]/s["n"]*100
        avg_m = s["move_sum"]/s["n"]
        net = s["gross"] - s["fees"]
        # average fee_bp assumed for this reason bucket
        # (use reason-typical kind to show)
        print(f"{r:13s} {s['n']:>4d} {wr:>5.1f}% {fmt_bp(avg_m):>10s} "
              f"{s['fee_bp_rt']:>5.2f}bp {fmt_usd(s['gross']):>10s} "
              f"{fmt_usd(-s['fees']):>9s} {fmt_usd(net):>10s}")
        tot["n"] += s["n"]
        tot["wins"] += s["wins"]
        tot["gross"] += s["gross"]
        tot["fees"] += s["fees"]

    print("-" * 78)
    net = tot["gross"] - tot["fees"]
    wr = tot["wins"]/tot["n"]*100 if tot["n"] else 0
    print(f"{'TOTAL':13s} {tot['n']:>4d} {wr:>5.1f}% {'':>10s} "
          f"{'':>7s} {fmt_usd(tot['gross']):>10s} "
          f"{fmt_usd(-tot['fees']):>9s} {fmt_usd(net):>10s}")

    # Rebate-view: what the exchange balance shows NOW vs. final settled
    GROSS_MAKER_BP = 2.5   # what balance deducts per maker leg before rebate
    GROSS_TAKER_BP = 4.25  # approx gross taker (0.425 / 0.1 ratio)
    gross_fees_now = 0.0
    for c in closes:
        # entry is maker; close fee based on kind
        entry_fee = AMOUNT_USD * GROSS_MAKER_BP / 1e4
        close_fee = AMOUNT_USD * (GROSS_MAKER_BP if c["kind"] in ("MAKER","BRACKET_TP")
                                  else GROSS_TAKER_BP) / 1e4
        gross_fees_now += entry_fee + close_fee
    balance_visible = tot["gross"] - gross_fees_now
    pending_rebate  = gross_fees_now - tot["fees"]
    print()
    print(f"Balance view NOW    : {fmt_usd(balance_visible)}  "
          f"(gross fees {fmt_usd(-gross_fees_now)} deducted, rebate not yet credited)")
    print(f"Pending rebate      : {fmt_usd(pending_rebate)}  (credited later)")
    print(f"TRUE net (settled)  : {fmt_usd(net)}  ← what you'll actually end up with")

    # Per-sym top 5 winners/losers
    sym_pnl = defaultdict(lambda: {"n":0, "net":0.0})
    for c in closes:
        fee_bp = fee_bp_for_close(c["kind"])
        fee = AMOUNT_USD * fee_bp / 1e4
        sym_pnl[c["sym"]]["n"] += 1
        sym_pnl[c["sym"]]["net"] += c["gross_pnl"] - fee
    winners = sorted(sym_pnl.items(), key=lambda x: -x[1]["net"])[:5]
    losers  = sorted(sym_pnl.items(), key=lambda x: x[1]["net"])[:5]
    print()
    print("Top 5 winners / losers (NET after fees):")
    for sym, v in winners:
        if v["net"] <= 0: break
        print(f"  +{sym:22s} n={v['n']:2d}  net={fmt_usd(v['net'])}")
    for sym, v in losers:
        if v["net"] >= 0: break
        print(f"  -{sym:22s} n={v['n']:2d}  net={fmt_usd(v['net'])}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", type=str, default=str(LOG_PATH))
    args = ap.parse_args()
    parsed = parse_log(Path(args.log))
    if parsed is None:
        print(f"No log at {args.log}")
        sys.exit(1)
    closes, status, extras = parsed
    render(closes, status or {"t_s": 0}, extras)


if __name__ == "__main__":
    main()

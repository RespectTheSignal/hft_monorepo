#!/usr/bin/env python3
"""Analyze actual Flipster fills vs Binance reference, computing real slippage."""
from __future__ import annotations
import argparse, json, sys, urllib.request, urllib.parse, statistics as stat
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

QDB_HTTP = "http://211.181.122.102:9000/exec"


def to_binance_sym(fsym: str) -> str:
    return fsym.replace("USDT.PERP", "_USDT")


def binance_mid_at(sym: str, ts_ms: int, window_ms: int = 500):
    """Fetch Binance mid closest to ts_ms (within window)."""
    bsym = to_binance_sym(sym)
    t0 = datetime.fromtimestamp((ts_ms - window_ms) / 1000, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
    t1 = datetime.fromtimestamp((ts_ms + window_ms) / 1000, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
    sql = (f"SELECT timestamp, bid_price, ask_price FROM binance_bookticker "
           f"WHERE symbol='{bsym}' AND timestamp >= '{t0}' AND timestamp <= '{t1}' "
           f"ORDER BY timestamp LIMIT 50")
    try:
        url = QDB_HTTP + "?" + urllib.parse.urlencode({"query": sql})
        r = json.loads(urllib.request.urlopen(url, timeout=10).read())
        rows = r.get("dataset", [])
        if not rows: return None
        # Find closest by absolute dt
        target_iso_ms = ts_ms
        best = None; best_diff = 1e18
        for row in rows:
            ts_s = row[0]
            # Parse ISO to epoch ms
            try:
                dt = datetime.fromisoformat(ts_s.replace("Z","+00:00"))
                row_ms = int(dt.timestamp() * 1000)
            except:
                continue
            diff = abs(row_ms - target_iso_ms)
            if diff < best_diff:
                best_diff = diff
                best = (float(row[1]) + float(row[2])) / 2
        return best
    except Exception:
        return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--positions", type=Path, default="/tmp/flipster_positions_history.json")
    args = ap.parse_args()

    positions = json.loads(args.positions.read_text())
    if not positions:
        print("empty"); return

    # Filter to perpetual only
    positions = [p for p in positions if p.get("symbol","").endswith(".PERP")]
    print(f"=== {len(positions)} closed positions ===\n")

    # Basic stats
    total_gross = sum(float(p.get("grossRealizedPnl", 0)) for p in positions)
    total_net = sum(float(p.get("netRealizedPnl", 0)) for p in positions)
    total_fees = sum(float(p.get("tradingFee", 0)) for p in positions)
    total_funding = sum(float(p.get("fundingFee", 0)) for p in positions)
    wins = [p for p in positions if float(p.get("netRealizedPnl", 0)) > 0]
    losses = [p for p in positions if float(p.get("netRealizedPnl", 0)) < 0]
    print(f"Gross realized: ${total_gross:+.2f}")
    print(f"Trading fees:   -${total_fees:.2f}")
    print(f"Funding:        ${-total_funding:+.2f}")
    print(f"Net realized:   ${total_net:+.2f}")
    print(f"Wins: {len(wins)}  Losses: {len(losses)}  WR: {len(wins)/len(positions)*100:.1f}%")
    if wins:
        print(f"Avg win:  ${sum(float(p['netRealizedPnl']) for p in wins)/len(wins):+.3f}")
    if losses:
        print(f"Avg loss: ${sum(float(p['netRealizedPnl']) for p in losses)/len(losses):+.3f}")

    # Entry/exit slippage vs Binance
    print("\n=== Slippage vs Binance (ms-resolution bookticker) ===")
    entry_slips = []
    exit_slips = []
    samples = []
    for p in positions:
        sym = p["symbol"]
        open_time_ms = int(p["openTime"]) // 1_000_000
        close_time_ms = int(p["closeTime"]) // 1_000_000
        open_px = float(p["avgOpenPrice"])
        close_px = float(p["avgClosePrice"])
        side = p["side"]  # Long/Short

        bx_at_open = binance_mid_at(sym, open_time_ms)
        bx_at_close = binance_mid_at(sym, close_time_ms)
        if bx_at_open:
            # Positive = paid more than Binance (bad entry)
            if side == "Long":
                entry_slip = (open_px - bx_at_open) / bx_at_open * 1e4
            else:
                entry_slip = (bx_at_open - open_px) / bx_at_open * 1e4
            entry_slips.append(entry_slip)
        else:
            entry_slip = None
        if bx_at_close:
            # Positive = got a worse close than Binance (adverse)
            if side == "Long":
                # Long close = sell. Worse = sold below Binance mid
                exit_slip = (bx_at_close - close_px) / bx_at_close * 1e4
            else:
                # Short close = buy. Worse = bought above Binance mid
                exit_slip = (close_px - bx_at_close) / bx_at_close * 1e4
            exit_slips.append(exit_slip)
        else:
            exit_slip = None
        samples.append({
            "sym": sym, "side": side,
            "entry": open_px, "exit": close_px,
            "bx_open": bx_at_open, "bx_close": bx_at_close,
            "entry_slip": entry_slip, "exit_slip": exit_slip,
            "net_pnl": float(p["netRealizedPnl"]),
            "qty_usd": abs(float(p["qty"]) * open_px),
        })

    def summ(arr, name):
        if not arr: print(f"  {name}: no data"); return
        mean = sum(arr)/len(arr)
        med = stat.median(arr)
        p95 = sorted(arr)[int(len(arr)*0.95)] if len(arr)>=20 else max(arr)
        worst = max(arr)
        print(f"  {name:20s}  n={len(arr):3d}  mean={mean:+.1f}bp  med={med:+.1f}bp  p95={p95:+.1f}bp  worst={worst:+.1f}bp")
    summ(entry_slips, "Entry slippage")
    summ(exit_slips, "Exit slippage")

    # Worst slippages
    print("\n=== Top 10 worst ENTRIES (Binance reference) ===")
    for s in sorted([x for x in samples if x['entry_slip'] is not None], key=lambda x: -x['entry_slip'])[:10]:
        print(f"  {s['sym']:22s} {s['side']:5s} entry={s['entry']:.6g} bx={s['bx_open']:.6g}  slip=+{s['entry_slip']:.1f}bp  pnl=${s['net_pnl']:+.3f}")
    print("\n=== Top 10 worst EXITS (Binance reference) ===")
    for s in sorted([x for x in samples if x['exit_slip'] is not None], key=lambda x: -x['exit_slip'])[:10]:
        print(f"  {s['sym']:22s} {s['side']:5s} exit={s['exit']:.6g} bx={s['bx_close']:.6g}  slip=+{s['exit_slip']:.1f}bp  pnl=${s['net_pnl']:+.3f}")

    # Dump detailed
    out = Path("/tmp/realized_fills_analysis.json")
    out.write_text(json.dumps(samples, indent=1, default=str))
    print(f"\n→ detailed dump to {out}")


if __name__ == "__main__":
    main()

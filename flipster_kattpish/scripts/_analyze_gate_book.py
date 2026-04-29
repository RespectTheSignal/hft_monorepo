"""Cross-validate Gate's actual account_book against our live_trades.jsonl.

For each pnl event in gate_account_book, find the matching trade in
live_trades.jsonl (by contract + timestamp window) and compute:
  - real Gate fee paid (from account_book fee events, summed for that trade)
  - real Gate PnL (from pnl event)
  - model Gate fee (from JSONL approx_fee)
  - model Gate PnL (g_pnl_usd from JSONL)
  - delta between model and reality
"""
import json
from pathlib import Path
from collections import defaultdict
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parent.parent
GATE_BOOK = ROOT / "logs" / "gate_account_book.jsonl"
LIVE_TRADES = ROOT / "logs" / "live_trades.jsonl"


def parse_change(s: str) -> float:
    """'-0.00486 USDT' → -0.00486"""
    if not s:
        return 0.0
    return float(s.replace("USDT", "").strip().split()[0])


def main():
    # Load Gate book
    gate_events = []
    with open(GATE_BOOK) as f:
        for line in f:
            try:
                gate_events.append(json.loads(line))
            except Exception:
                pass
    print(f"Gate events loaded: {len(gate_events)}")

    # Time range
    times = [e["time"] for e in gate_events if e.get("time")]
    if times:
        t_min = datetime.fromtimestamp(min(times), tz=timezone.utc)
        t_max = datetime.fromtimestamp(max(times), tz=timezone.utc)
        print(f"  time range: {t_min.isoformat()}  ..  {t_max.isoformat()}  ({(max(times)-min(times))/3600:.1f}h)")

    # Group by trade_id (each fill has a trade_id; same trade_id can have a pnl + fee event pair)
    by_tid = defaultdict(list)
    for e in gate_events:
        tid = e.get("trade_id")
        if tid:
            by_tid[tid].append(e)

    print(f"  unique trade_ids: {len(by_tid)}")

    # Aggregate: per trade_id, compute fee + pnl
    real_per_trade = []
    for tid, evs in by_tid.items():
        fee = sum(parse_change(e.get("change")) for e in evs if e.get("type") == "fee")
        pnl = sum(parse_change(e.get("change")) for e in evs if e.get("type") == "pnl")
        if pnl == 0 and fee == 0:
            continue
        contract = evs[0].get("contract")
        t = evs[0].get("time")
        text = evs[0].get("text", "")
        real_per_trade.append({
            "trade_id": tid,
            "contract": contract,
            "time": t,
            "fee": fee,
            "pnl": pnl,
            "text": text,
        })
    real_per_trade.sort(key=lambda r: r["time"])
    print(f"  trade_ids with non-zero fee or pnl: {len(real_per_trade)}")

    # Note: each round-trip in our strategy produces 2 trade_ids (open + close).
    # account_book records each fill separately. So for a strategy "trade", we
    # actually have 2 trade_ids spaced ~seconds apart. The pnl is on the CLOSE
    # event only (open has 0 pnl); fees are charged on both.
    fees_only = [r for r in real_per_trade if r["pnl"] == 0]
    pnl_events = [r for r in real_per_trade if r["pnl"] != 0]
    print(f"  fee-only events (=open fills):   {len(fees_only)}")
    print(f"  events with pnl (=close fills):  {len(pnl_events)}")

    # Total real fees paid in window
    total_fee = sum(r["fee"] for r in real_per_trade)
    total_pnl = sum(r["pnl"] for r in real_per_trade)
    print(f"\n  total real Gate fee:  ${total_fee:+.4f}")
    print(f"  total real Gate pnl:  ${total_pnl:+.4f}")
    print(f"  net (Gate side):      ${total_fee + total_pnl:+.4f}")

    # Per-close (round-trip) average fee
    if pnl_events:
        avg_fee_close = sum(r["fee"] for r in pnl_events) / len(pnl_events)
        avg_fee_open = sum(r["fee"] for r in fees_only) / max(1, len(fees_only))
        print(f"\n  avg fee per CLOSE fill:  ${avg_fee_close:.6f}")
        print(f"  avg fee per OPEN fill:   ${avg_fee_open:.6f}")
        print(f"  avg fee per round-trip:  ${avg_fee_close + avg_fee_open:.6f}")

    # Cross-match with live_trades.jsonl
    trades = []
    with open(LIVE_TRADES) as f:
        for line in f:
            try:
                t = json.loads(line)
                if t.get("ts_close") in ("shutdown", "", None):
                    continue
                trades.append(t)
            except Exception:
                pass
    print(f"\nLive trades loaded: {len(trades)}")

    # Match: for each live trade, find Gate close event by (contract, ts_close ± 3s).
    # gate_contract is "BTC_USDT" form.
    pnl_by_contract = defaultdict(list)
    for r in pnl_events:
        pnl_by_contract[r["contract"]].append(r)

    matched = 0
    sum_real_fee = 0.0
    sum_real_pnl = 0.0
    sum_model_fee = 0.0
    sum_model_pnl = 0.0
    deltas = []
    for t in trades:
        contract = t.get("gate_contract")
        if not contract:
            continue
        try:
            ts = datetime.fromisoformat(t["ts_close"].replace("Z", "+00:00")).timestamp()
        except Exception:
            continue
        cands = pnl_by_contract.get(contract, [])
        # Find pnl event closest in time within 5s
        best = None
        best_dt = 999
        for r in cands:
            dt = abs(r["time"] - ts)
            if dt < best_dt:
                best_dt = dt
                best = r
        if not best or best_dt > 5:
            continue
        # Found match. Real Gate fee = close fee + matching open fee (same trade_id? no — pair via close text)
        # On Gate, the open fill produces a fee-only event seconds before; trade_id differs.
        # We approximate Gate's open fee from the average (or look up open by ts_entry).
        # Simpler: count fee = 2 × close fee (both legs are similar size).
        real_close_fee = best["fee"]
        # Find open fee: same contract, time near ts_entry
        try:
            ts_e = datetime.fromisoformat(t["ts_entry"].replace("Z", "+00:00")).timestamp()
        except Exception:
            ts_e = 0
        open_fee = 0.0
        if ts_e:
            for r in fees_only:
                if r["contract"] == contract and abs(r["time"] - ts_e) <= 5:
                    open_fee = r["fee"]
                    break
        real_total_fee = open_fee + real_close_fee
        real_pnl = best["pnl"]
        model_fee = t["approx_fee_usd"]   # both legs combined
        model_pnl = t["g_pnl_usd"] + t["f_pnl_usd"]
        matched += 1
        sum_real_fee += real_total_fee
        sum_real_pnl += real_pnl
        sum_model_fee += model_fee / 2  # only Gate-side ≈ half of total
        sum_model_pnl += t["g_pnl_usd"]
        deltas.append({
            "ts": t["ts_close"],
            "base": t.get("base"),
            "size": t.get("size_usd"),
            "real_fee_gate": real_total_fee,
            "model_fee_gate_approx": model_fee / 2,
            "real_pnl_gate": real_pnl,
            "model_pnl_gate": t["g_pnl_usd"],
        })

    print(f"\n=== Live trades matched to Gate book: {matched}/{len(trades)} ===")
    if matched:
        print(f"  REAL  Gate fee total (matched): ${sum_real_fee:+.4f}")
        print(f"  MODEL Gate fee total (matched): ${sum_model_fee:+.4f}  (taker model ÷2)")
        ratio = sum_real_fee / sum_model_fee if sum_model_fee else float('nan')
        print(f"  ratio real/model: {ratio:.2f}x")
        print()
        print(f"  REAL  Gate pnl total (matched): ${sum_real_pnl:+.4f}")
        print(f"  MODEL Gate pnl total (matched): ${sum_model_pnl:+.4f}")
        print()
        print("  Sample deltas (first 10):")
        for d in deltas[:10]:
            print(f"    {d['ts'][:19]} {d['base']:<8} size=${d['size']:.0f}  "
                  f"real_fee=${d['real_fee_gate']:.5f}  model=${d['model_fee_gate_approx']:.5f}  "
                  f"real_pnl=${d['real_pnl_gate']:+.4f}  model_pnl=${d['model_pnl_gate']:+.4f}")

    # Real fee bp distribution by trade size
    print("\n=== Real fee bp by approx size (using close+open fee sum) ===")
    by_size_bucket = defaultdict(list)
    for d in deltas:
        if d["size"] <= 0:
            continue
        # fee bp on size_usd (both sides → divide real_fee by size_usd)
        bp = abs(d["real_fee_gate"]) / d["size"] * 1e4
        if d["size"] < 2:
            b = "$1"
        elif d["size"] < 5:
            b = "$2-5"
        elif d["size"] < 15:
            b = "$10"
        else:
            b = ">$15"
        by_size_bucket[b].append(bp)
    for b, vs in by_size_bucket.items():
        print(f"  {b:<6} n={len(vs):3d}  avg_fee_bp={sum(vs)/len(vs):.2f}  "
              f"min={min(vs):.2f}  max={max(vs):.2f}")


if __name__ == "__main__":
    main()

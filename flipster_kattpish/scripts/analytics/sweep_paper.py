#!/usr/bin/env python3
"""Sweep-following paper trade backtest.

Strategy:
  - Watch binance_trade (Binance Spot aggTrade) for large directional prints.
  - When usd_value > $threshold, enter BingX in same direction at current BBO.
  - Exit after `hold_sec` seconds at then-current BingX BBO.
  - Compute pnl in bp, before/after fees.

Schema assumptions:
  - binance_trade(timestamp, symbol='BTCUSDT', side='buy'|'sell', price, quantity, usd_value, ...)
  - bingx_bookticker(timestamp, symbol='BTC-USDT', bid_price, ask_price, ...)

Usage:
  python3 sweep_paper.py [--lookback-min 30] [--threshold 5000] [--hold-sec 5]
                         [--sweep] [--qdb http://211.181.122.102:9000]
"""
import argparse, json, sys, urllib.parse, urllib.request
import pandas as pd

QDB_DEFAULT = "http://211.181.122.102:9000"
BINGX_TAKER_BP = 5.0   # ~0.05%
BINANCE_TAKER_BP = 5.0 # ~0.05% (informational; we only trade BingX side)
ROUND_TRIP_FEE_BP = BINGX_TAKER_BP * 2  # we taker both sides on BingX


def query(qdb: str, sql: str):
    url = f"{qdb}/exec?query={urllib.parse.quote(sql)}"
    return json.loads(urllib.request.urlopen(url, timeout=120).read())


def to_df(res):
    if "error" in res:
        raise RuntimeError(f"QDB error: {res['error']}")
    cols = [c["name"] for c in res["columns"]]
    return pd.DataFrame(res["dataset"], columns=cols)


def fetch(qdb: str, lookback_min: int, threshold_usd: float):
    ev_sql = f"""SELECT timestamp t, symbol, side, usd_value
        FROM binance_trade
        WHERE usd_value > {threshold_usd}
          AND timestamp > dateadd('m', -{lookback_min}, now())
        ORDER BY timestamp"""
    bx_sql = f"""SELECT timestamp t, symbol, bid_price b, ask_price a
        FROM bingx_bookticker
        WHERE timestamp > dateadd('m', -{lookback_min + 1}, now())
        ORDER BY timestamp"""
    events = to_df(query(qdb, ev_sql))
    bx = to_df(query(qdb, bx_sql))
    if events.empty or bx.empty:
        return events, bx
    events["t"] = pd.to_datetime(events["t"])
    events["base"] = events["symbol"].str.replace("USDT", "", regex=False)
    bx["t"] = pd.to_datetime(bx["t"])
    bx["base"] = bx["symbol"].str.replace("-USDT", "", regex=False)
    return events, bx


def backtest(events: pd.DataFrame, bx: pd.DataFrame, hold_sec: float):
    if events.empty or bx.empty:
        return pd.DataFrame()
    # Tag each event with a unique id so we can re-join entry/exit cleanly
    bx_sorted = bx.sort_values("t").reset_index(drop=True)
    ev = events.copy()
    ev["evt_id"] = range(len(ev))
    ev_sorted_t = ev.sort_values("t").reset_index(drop=True)

    # Entry: bingx tick at or before event
    entries = pd.merge_asof(
        ev_sorted_t, bx_sorted[["t", "base", "b", "a"]],
        on="t", by="base", direction="backward",
    ).rename(columns={"b": "entry_bid", "a": "entry_ask"})

    # Exit: bingx tick at or before (event + hold_sec)
    ev_exit = ev_sorted_t[["evt_id", "base", "side", "t", "usd_value"]].copy()
    ev_exit["t"] = ev_exit["t"] + pd.Timedelta(seconds=hold_sec)
    ev_exit = ev_exit.sort_values("t").reset_index(drop=True)
    exits = pd.merge_asof(
        ev_exit, bx_sorted[["t", "base", "b", "a"]],
        on="t", by="base", direction="backward",
    ).rename(columns={"b": "exit_bid", "a": "exit_ask"})

    entries = entries.merge(
        exits[["evt_id", "exit_bid", "exit_ask"]], on="evt_id", how="left"
    )

    trades = entries.copy()

    def pnl(row):
        eb, ea, xb, xa = row["entry_bid"], row["entry_ask"], row["exit_bid"], row["exit_ask"]
        if pd.isna(eb) or pd.isna(ea) or pd.isna(xb) or pd.isna(xa):
            return None
        if ea <= 0 or eb <= 0:
            return None
        if row["side"] == "buy":
            # take BingX ask, sell at exit bid
            return (xb / ea - 1) * 1e4
        else:
            # short BingX bid, cover at exit ask
            return (1 - xa / eb) * 1e4

    trades["pnl_bp_gross"] = trades.apply(pnl, axis=1)
    trades["pnl_bp_net"] = trades["pnl_bp_gross"] - ROUND_TRIP_FEE_BP
    return trades


def summarize(trades: pd.DataFrame, label: str):
    valid = trades.dropna(subset=["pnl_bp_gross"])
    if len(valid) == 0:
        print(f"{label}: 0 valid trades")
        return None
    g = valid["pnl_bp_gross"]
    n = valid["pnl_bp_net"]
    print(
        f"{label}: n={len(valid):>4}  "
        f"gross_mean={g.mean():>+6.2f}bp  "
        f"net_mean={n.mean():>+6.2f}bp  "
        f"gross_med={g.median():>+6.2f}bp  "
        f"win%={(g>0).mean()*100:>5.1f}  "
        f"net_win%={(n>0).mean()*100:>5.1f}  "
        f"sum_net={n.sum():>+8.1f}bp"
    )
    return valid


def per_symbol(trades: pd.DataFrame, top_n: int = 15):
    valid = trades.dropna(subset=["pnl_bp_gross"])
    if valid.empty: return
    by = valid.groupby("base").agg(
        n=("pnl_bp_gross", "count"),
        gross_mean=("pnl_bp_gross", "mean"),
        net_mean=("pnl_bp_net", "mean"),
        win_pct=("pnl_bp_gross", lambda x: (x > 0).mean() * 100),
        net_win_pct=("pnl_bp_net", lambda x: (x > 0).mean() * 100),
        sum_net_bp=("pnl_bp_net", "sum"),
    ).sort_values("sum_net_bp", ascending=False).head(top_n)
    print(f"\nTop {top_n} symbols by sum_net_bp:")
    print(by.round(2).to_string())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--qdb", default=QDB_DEFAULT)
    ap.add_argument("--lookback-min", type=int, default=30)
    ap.add_argument("--threshold", type=float, default=5000)
    ap.add_argument("--hold-sec", type=float, default=5)
    ap.add_argument("--sweep", action="store_true",
                    help="run grid: thresholds × holds")
    args = ap.parse_args()

    print(f"=== Sweep paper backtest ===")
    print(f"qdb={args.qdb}  lookback={args.lookback_min}min  fee={ROUND_TRIP_FEE_BP}bp r/t")

    if args.sweep:
        print(f"running grid: thresholds={[1000,5000,20000]} × holds={[1,3,5,15]}\n")
        thresholds = [1000, 5000, 20000]
        holds = [1, 3, 5, 15]
        for th in thresholds:
            ev, bx = fetch(args.qdb, args.lookback_min, th)
            print(f"\n--- threshold ${th}: events={len(ev)} ---")
            for h in holds:
                tr = backtest(ev, bx, h)
                summarize(tr, f"  hold={h:>2}s")
    else:
        ev, bx = fetch(args.qdb, args.lookback_min, args.threshold)
        print(f"events={len(ev)}  bx_ticks={len(bx)}")
        tr = backtest(ev, bx, args.hold_sec)
        summarize(tr, f"hold={args.hold_sec}s")
        per_symbol(tr)


if __name__ == "__main__":
    sys.exit(main() or 0)

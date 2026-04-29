"""Grid-search a simple latency-arbitrage policy over recorded data.

Model
-----
At each binance move >= entry_threshold_bp, we check whether flipster mid
lags (sign-consistent). If so, we open a virtual position on flipster at
its current mid and close when:
  - price reflects (reaches the binance side price): realize gain
  - max_hold_ms elapses: realize at the flipster mid nearest that time
  - adverse move reaches stop_loss_bp: realize loss

Fee model: round-trip 0.375bp (flipster 2.5bp × (1 - 0.85) payback × 2).

NOTE: This is a research tool operating on historical observations. It
does NOT place or imitate real trades. It does not model detection,
slippage, rejection, or any adversarial exchange behavior.
"""
from __future__ import annotations
import itertools
import matplotlib.pyplot as plt
import numpy as np
import pandas as pd
import seaborn as sns
from qdb import query

ROUND_TRIP_FEE_BP = 0.375


def load_latency(days: int = 7) -> pd.DataFrame:
    sql = f"""
        SELECT binance_ts, flipster_ts, symbol, binance_mid, flipster_mid,
               latency_ms, price_diff_bp, direction
        FROM latency_log
        WHERE binance_ts > dateadd('d', -{days}, now())
        ORDER BY binance_ts
    """
    return query(sql)


def load_flipster_mids(symbol: str, days: int = 7) -> pd.DataFrame:
    sql = f"""
        SELECT timestamp, (bid_price + ask_price) / 2 mid
        FROM flipster_bookticker
        WHERE symbol = '{symbol}.PERP'
          AND timestamp > dateadd('d', -{days}, now())
        ORDER BY timestamp
    """
    df = query(sql)
    if df.empty:
        return df
    df["timestamp"] = pd.to_datetime(df["timestamp"])
    return df.set_index("timestamp")


def simulate_symbol(
    lat: pd.DataFrame,
    mids: pd.DataFrame,
    entry_bp: float,
    max_hold_ms: int,
    stop_bp: float,
) -> pd.DataFrame:
    if lat.empty or mids.empty:
        return pd.DataFrame()
    out = []
    mids_idx = mids.index.values.astype("datetime64[ns]")
    mid_vals = mids["mid"].values
    for _, row in lat.iterrows():
        if abs(row["price_diff_bp"]) < entry_bp:
            continue
        direction = int(row["direction"])
        entry_mid = row["flipster_mid"]
        target_bp = row["price_diff_bp"]
        t0 = np.datetime64(row["binance_ts"], "ns")
        t_end = t0 + np.timedelta64(max_hold_ms, "ms")
        lo = mids_idx.searchsorted(t0)
        hi = mids_idx.searchsorted(t_end)
        window = mid_vals[lo:hi]
        if len(window) == 0:
            continue
        # PnL in bp relative to entry_mid, signed by direction
        rel_bp = ((window - entry_mid) / entry_mid) * 10_000.0 * direction
        exit_idx = None
        exit_reason = "timeout"
        for i, v in enumerate(rel_bp):
            if v >= abs(target_bp):
                exit_idx = i
                exit_reason = "target"
                break
            if v <= -stop_bp:
                exit_idx = i
                exit_reason = "stoploss"
                break
        if exit_idx is None:
            exit_idx = len(rel_bp) - 1
        gross_bp = rel_bp[exit_idx]
        net_bp = gross_bp - ROUND_TRIP_FEE_BP
        out.append(
            dict(
                symbol=row["symbol"],
                entry_bp=entry_bp,
                max_hold_ms=max_hold_ms,
                stop_bp=stop_bp,
                gross_bp=gross_bp,
                net_bp=net_bp,
                exit_reason=exit_reason,
            )
        )
    return pd.DataFrame(out)


def grid_search(days: int = 7) -> pd.DataFrame:
    lat = load_latency(days)
    if lat.empty:
        print("no latency data yet; run the collector for a while first")
        return pd.DataFrame()
    results = []
    entry_grid = [0.05, 0.1, 0.15, 0.2]
    hold_grid = [500, 1000, 2000, 5000]
    stop_grid = [0.1, 0.2, 0.5]
    for sym, g in lat.groupby("symbol"):
        mids = load_flipster_mids(sym, days)
        if mids.empty:
            continue
        for e, h, s in itertools.product(entry_grid, hold_grid, stop_grid):
            sim = simulate_symbol(g, mids, e, h, s)
            if sim.empty:
                continue
            results.append(
                dict(
                    symbol=sym,
                    entry_bp=e,
                    max_hold_ms=h,
                    stop_bp=s,
                    n=len(sim),
                    mean_net_bp=sim["net_bp"].mean(),
                    median_net_bp=sim["net_bp"].median(),
                    total_bp=sim["net_bp"].sum(),
                    hit_rate=(sim["exit_reason"] == "target").mean(),
                )
            )
    return pd.DataFrame(results)


def plot_heatmaps(df: pd.DataFrame) -> None:
    if df.empty:
        return
    for sym, g in df.groupby("symbol"):
        for stop in sorted(g["stop_bp"].unique()):
            sub = g[g["stop_bp"] == stop]
            pivot = sub.pivot(index="entry_bp", columns="max_hold_ms", values="mean_net_bp")
            plt.figure(figsize=(7, 5))
            sns.heatmap(pivot, annot=True, fmt=".2f", cmap="RdYlGn", center=0)
            plt.title(f"{sym}  mean_net_bp  stop={stop}")
            plt.tight_layout()
            plt.savefig(f"grid_{sym}_stop{stop}.png", dpi=140)
            plt.close()


def main() -> None:
    df = grid_search()
    if df.empty:
        return
    df = df.sort_values("mean_net_bp", ascending=False)
    print(df.head(40).to_string(index=False))
    df.to_csv("grid_results.csv", index=False)
    plot_heatmaps(df)


if __name__ == "__main__":
    main()

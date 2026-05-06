"""Backpack-focused deep dive.

1. Sub-second lag for backpack vs major leaders across top 10 backpack coins
2. Realistic bid-ask backtest for backpack pairs
3. Compute fee + spread + edge feasibility per pair
"""
import os
import sys
import time
from pathlib import Path

import numpy as np
import pandas as pd

sys.path.insert(0, str(Path(__file__).parent))
from fees import round_trip_bp, fees_bp
from pair_stats import load_aligned_spread, pair_stats
from backtest_realistic import simulate_pair_realistic
from lead_lag_subsec import event_study, lead_lag_summary
from spread_calc import _series_path, fetch_series, _raw_for_coin

OUTPUT = Path(__file__).parent / "output"


# Backpack's most-liquid coins (from inventory)
BACKPACK_TOP = ["ETH", "BTC", "SOL", "PENDLE", "ZEC", "XRP", "AAVE", "HYPE",
                "PENGU", "DOGE", "SUI", "BNB", "ONDO", "LINK", "TIA", "NEAR", "JTO"]

LEADERS = ["binance", "bitget", "gate", "okx", "bybit"]


def ensure_caches(inv: pd.DataFrame, coins: list[str], exchanges: list[str]):
    """Ensure 1s bid/ask cache exists for (exchange, coin)."""
    needed = [(ex, c) for ex in exchanges for c in coins]
    print(f"Checking {len(needed)} (exchange, coin) caches...")
    for ex, c in needed:
        p = _series_path(ex, c)
        if p.exists():
            df = pd.read_parquet(p)
            if "bid_price" in df.columns:
                continue
        raw = _raw_for_coin(ex, c, inv)
        if raw is None:
            continue
        try:
            fetch_series(ex, c, raw)
        except Exception as e:
            print(f"  cache fail {ex}/{c}: {e}")


def run_lag_study():
    rows = []
    pairs = []
    for c in BACKPACK_TOP:
        for L in LEADERS:
            pairs.append((c, L, "backpack"))
    print(f"Lag study: {len(pairs)} pairs")
    for i, (coin, a, b) in enumerate(pairs):
        try:
            res = event_study(coin, a, b)
        except Exception as e:
            print(f"  ERR {coin} {a}-{b}: {e}")
            continue
        if res is None:
            continue
        s = lead_lag_summary(res)
        rows.append(s)
        print(f"  [{i+1}/{len(pairs)}] {coin} {a}×{b}: "
              f"b_lag={s['b_lag_after_a_shock_ms']} a_lag={s['a_lag_after_b_shock_ms']} "
              f"a_asym={s['leader_a_asymptote_bp']:.2f} b_resp={s['b_resp_asymptote_bp']:.2f}")
    df = pd.DataFrame(rows)
    df.to_csv(OUTPUT / "08_backpack_lag.csv", index=False)
    print(f"Wrote 08_backpack_lag.csv: {len(df)}")
    return df


def run_backtests():
    rows = []
    pairs = []
    for c in BACKPACK_TOP:
        for L in LEADERS:
            pairs.append((c, L, "backpack"))
    print(f"\nBacktest: {len(pairs)} backpack pairs")
    for i, (coin, a, b) in enumerate(pairs):
        try:
            res = simulate_pair_realistic(coin, a, b, fee_mode="taker_taker")
        except Exception as e:
            print(f"  ERR {coin} {a}-{b}: {e}")
            continue
        if res:
            rows.append(res)
            if (i+1) % 5 == 0:
                print(f"  [{i+1}/{len(pairs)}] last: {coin} {a}×{b} pnl={res['total_pnl_bp']:.0f}bp trades={res['n_trades']}")
    df = pd.DataFrame(rows)
    df.to_csv(OUTPUT / "08_backpack_backtest.csv", index=False)
    return df


def quick_pair_stats(coin, a, b):
    """Return mean spread, stddev, half-life etc using existing pair_stats."""
    return pair_stats(coin, a, b, do_heavy=True)


def run_pair_stats_table():
    rows = []
    for c in BACKPACK_TOP:
        for L in LEADERS:
            try:
                s = quick_pair_stats(c, L, "backpack")
            except Exception as e:
                continue
            if s:
                rows.append(s)
    df = pd.DataFrame(rows)
    df["rt_fee_bp"] = df.apply(lambda r: round_trip_bp(r.ex_a, r.ex_b), axis=1)
    df["entry_2sigma_bp"] = 2 * df.stddev_bp
    df["edge_after_fee_bp"] = df.entry_2sigma_bp - df.rt_fee_bp
    df.to_csv(OUTPUT / "08_backpack_pair_stats.csv", index=False)
    return df


if __name__ == "__main__":
    inv = pd.read_csv(OUTPUT / "01_inventory.csv")
    print("=== Stage 1: ensure 1s bid/ask cache for backpack pairs ===")
    ensure_caches(inv, BACKPACK_TOP, LEADERS + ["backpack"])
    print()
    print("=== Stage 2: pair stats (mean/stddev/half-life/Hurst/ADF) ===")
    stats = run_pair_stats_table()
    print(stats[["coin","ex_a","ex_b","n_obs","mean_bp","stddev_bp","entry_2sigma_bp",
                  "rt_fee_bp","edge_after_fee_bp","half_life_sec","hurst","adf_pvalue"]].to_string(index=False))
    print()
    print("=== Stage 3: realistic bid-ask backtest ===")
    bt = run_backtests()
    if not bt.empty:
        bt = bt.sort_values("total_pnl_bp", ascending=False)
        cols = ["coin","ex_a","ex_b","n_trades","win_rate","total_pnl_bp","avg_pnl_bp",
                "p50_pnl_bp","avg_hold_sec","rt_fee_bp","avg_open_cost_bp","trades_per_day"]
        print(bt[cols].to_string(index=False))
        print(f"\nProfitable: {(bt.total_pnl_bp>0).sum()}/{len(bt)}")
        print(f"Pass quality bar (≥10 trades, ≥60% win, +PnL): {((bt.n_trades>=10)&(bt.win_rate>=0.6)&(bt.total_pnl_bp>0)).sum()}")
    print()
    print("=== Stage 4: sub-second lag (raw tick event study) ===")
    lag = run_lag_study()
    if not lag.empty:
        cols = ["coin","ex_a","ex_b","b_lag_after_a_shock_ms","a_lag_after_b_shock_ms",
                "leader_a_asymptote_bp","b_resp_asymptote_bp","leader_b_asymptote_bp","a_resp_asymptote_bp"]
        print(lag[cols].round(2).to_string(index=False))

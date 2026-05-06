"""Step 4: mean-revert score per pair.

Definitions (all bp):
  rt_fee   = round-trip taker fee for the pair (sum both legs × 2 entry+exit)
  entry_k  = entry deviation from mean in σ units (default 2.0)
  edge_bp  = abs(entry_threshold_distance - target_distance) - rt_fee
            = entry_k*σ - rt_fee   (target = mean, exit at 0σ)
  expected_trades_per_day = (rate at which |z| crosses entry_k)
            ≈ crossings_per_hour × P(|z| > entry_k) × 24
  score   = max(0, edge_bp) × expected_trades_per_day × stationarity_indicator

stationarity_indicator: 1 if Hurst < 0.5 AND ADF p < 0.05 AND half_life in [3,1800]s, else 0.
"""
import sys
from pathlib import Path

import numpy as np
import pandas as pd
from scipy.stats import norm

sys.path.insert(0, str(Path(__file__).parent))
from fees import round_trip_bp

OUTPUT = Path(__file__).parent / "output"

ENTRY_K = 2.0
TARGET_K = 0.0
HALF_LIFE_MIN_SEC = 3
HALF_LIFE_MAX_SEC = 1800
HURST_MAX = 0.5
ADF_P_MAX = 0.05


def score_row(row, entry_k: float = ENTRY_K, mode: str = "taker_taker") -> dict:
    rt = round_trip_bp(row["ex_a"], row["ex_b"], mode=mode)
    sigma = row["stddev_bp"]
    entry_dev = entry_k * sigma  # bp distance from mean at entry
    target_dev = TARGET_K * sigma
    edge_gross_bp = entry_dev - target_dev  # spread captured if it reverts to target
    edge_net_bp = edge_gross_bp - rt
    # crossings_per_hour is mean-crossings; tail-cross at |z|>=k frequency:
    # for normal, P(|z|>k) → use 1-Phi(k) per side, total 2*(1-Phi(k))
    p_tail = 2 * (1 - norm.cdf(entry_k))
    # rough trade rate = mean-crossings per hour × p_tail (each crossing event implies traversal through tails)
    trades_per_day = row["crossings_per_hour"] * p_tail * 24
    # stationarity gate
    is_stationary = int(
        (row["hurst"] < HURST_MAX) and
        (row["adf_pvalue"] < ADF_P_MAX) and
        (HALF_LIFE_MIN_SEC <= row["half_life_sec"] <= HALF_LIFE_MAX_SEC) and
        np.isfinite(row["half_life_sec"])
    )
    score = max(0.0, edge_net_bp) * trades_per_day * is_stationary
    return {
        "rt_fee_bp": rt,
        "entry_dev_bp": entry_dev,
        "edge_gross_bp": edge_gross_bp,
        "edge_net_bp": edge_net_bp,
        "trades_per_day_est": trades_per_day,
        "is_stationary": is_stationary,
        "score": score,
    }


def compute_scores(stats_path: Path = OUTPUT / "03_pair_stats.csv",
                   mode: str = "taker_taker") -> pd.DataFrame:
    df = pd.read_csv(stats_path)
    extras = pd.DataFrame([score_row(r, mode=mode) for _, r in df.iterrows()])
    out = pd.concat([df, extras], axis=1)
    return out.sort_values("score", ascending=False)


if __name__ == "__main__":
    import os
    mode = os.getenv("FEE_MODE", "taker_taker")
    df = compute_scores(mode=mode)
    p = OUTPUT / f"04_pair_scores_{mode}.csv"
    df.to_csv(p, index=False)
    print(f"Wrote {p}: {len(df)} rows; mode={mode}")

    print(f"\n=== Top 20 by score ({mode}) ===")
    cols = ["coin","ex_a","ex_b","n_obs","mean_bp","stddev_bp","entry_dev_bp",
            "rt_fee_bp","edge_net_bp","trades_per_day_est","half_life_sec","hurst","adf_pvalue","score"]
    print(df.head(20)[cols].to_string(index=False))

    print(f"\n=== How many pairs survive stationarity + edge>0 ===")
    print(f"  stationary: {df['is_stationary'].sum()}")
    print(f"  stationary & edge>0: {((df['is_stationary']==1)&(df['edge_net_bp']>0)).sum()}")
    print(f"  score>0: {(df['score']>0).sum()}")

    print(f"\n=== Per-coin best pair ===")
    best = df[df.score > 0].sort_values("score", ascending=False).groupby("coin").head(1)
    print(best.head(20)[cols].to_string(index=False))

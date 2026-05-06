"""Step 3: per-pair spread statistics.

Loads cached 1s-resampled series for both legs of each pair, aligns on
timestamp, computes spread_bp = (a-b)/b * 10000, and emits stats:
    mean_bp, stddev_bp, abs_mean_bp, sharpe, range_p95_bp, range_p99_bp,
    crossings_per_hour, half_life_sec (OU fit), adf_pvalue, hurst, n_obs.
"""
import os
import sys
import time
import warnings
from pathlib import Path

import numpy as np
import pandas as pd
from statsmodels.tsa.stattools import adfuller

warnings.filterwarnings("ignore")

sys.path.insert(0, str(Path(__file__).parent))
from spread_calc import _series_path

OUTPUT = Path(__file__).parent / "output"

MIN_OBS = 1800  # at least 30 minutes of overlap
WINSOR_LOW, WINSOR_HIGH = 0.001, 0.999


def _winsorize(x: np.ndarray) -> np.ndarray:
    lo, hi = np.quantile(x, [WINSOR_LOW, WINSOR_HIGH])
    return np.clip(x, lo, hi)


def half_life_ou(spread: np.ndarray) -> float:
    """OU half-life via AR(1) on first differences. Returns seconds (NaN if invalid)."""
    s = spread - spread.mean()
    s_lag = s[:-1]
    s_diff = np.diff(s)
    # ds = -theta * s + noise → linear regression
    if len(s_lag) < 100 or s_lag.var() == 0:
        return float("nan")
    theta = -np.dot(s_lag, s_diff) / np.dot(s_lag, s_lag)
    if theta <= 0:
        return float("nan")
    return float(np.log(2) / theta)


def hurst_exponent(x: np.ndarray, max_lag: int = 100) -> float:
    """R/S-style Hurst via variance of differences at multiple lags."""
    if len(x) < 4 * max_lag:
        return float("nan")
    lags = np.unique(np.logspace(0.3, np.log10(max_lag), 20).astype(int))
    lags = lags[lags >= 2]
    tau = []
    for lag in lags:
        diffs = x[lag:] - x[:-lag]
        if diffs.std() == 0:
            return float("nan")
        tau.append(np.std(diffs))
    log_lags = np.log(lags)
    log_tau = np.log(tau)
    slope = np.polyfit(log_lags, log_tau, 1)[0]
    return float(slope)


def crossings_per_hour(spread: np.ndarray, mean: float, dt_sec: float = 1.0) -> float:
    s = spread - mean
    sgn = np.sign(s)
    crossings = int(np.sum((sgn[1:] != sgn[:-1]) & (sgn[1:] != 0)))
    hours = (len(spread) * dt_sec) / 3600.0
    return crossings / hours if hours > 0 else 0.0


def adf_pvalue(x: np.ndarray) -> float:
    try:
        if len(x) > 5000:
            # subsample to keep adfuller fast
            idx = np.linspace(0, len(x)-1, 5000).astype(int)
            x = x[idx]
        return float(adfuller(x, regression="c", autolag="AIC")[1])
    except Exception:
        return float("nan")


def load_aligned_spread(coin: str, ex_a: str, ex_b: str) -> pd.DataFrame | None:
    pa = _series_path(ex_a, coin)
    pb = _series_path(ex_b, coin)
    if not pa.exists() or not pb.exists():
        return None
    a = pd.read_parquet(pa)["mid"].rename("a")
    b = pd.read_parquet(pb)["mid"].rename("b")
    df = pd.concat([a, b], axis=1).dropna()
    if len(df) < MIN_OBS:
        return None
    df["spread_bp"] = (df["a"] - df["b"]) / df["b"] * 10000.0
    # winsorize
    df["spread_bp_w"] = _winsorize(df["spread_bp"].to_numpy())
    return df


def pair_stats(coin: str, ex_a: str, ex_b: str, do_heavy: bool = True) -> dict | None:
    df = load_aligned_spread(coin, ex_a, ex_b)
    if df is None:
        return None
    s = df["spread_bp_w"].to_numpy()
    mean = float(np.mean(s))
    std = float(np.std(s))
    out = {
        "coin": coin, "ex_a": ex_a, "ex_b": ex_b,
        "n_obs": len(s),
        "mean_bp": mean,
        "stddev_bp": std,
        "abs_mean_bp": abs(mean),
        "sharpe": mean / std if std > 0 else float("nan"),
        "range_p95_bp": float(np.quantile(np.abs(s - mean), 0.95)),
        "range_p99_bp": float(np.quantile(np.abs(s - mean), 0.99)),
        "crossings_per_hour": crossings_per_hour(s, mean),
    }
    if do_heavy:
        out["half_life_sec"] = half_life_ou(s)
        out["adf_pvalue"] = adf_pvalue(s)
        out["hurst"] = hurst_exponent(s)
    else:
        out["half_life_sec"] = float("nan")
        out["adf_pvalue"] = float("nan")
        out["hurst"] = float("nan")
    return out


def compute_all(pairs: pd.DataFrame, do_heavy: bool = True) -> pd.DataFrame:
    rows = []
    skipped = 0
    t0 = time.time()
    for i, r in enumerate(pairs.itertuples(index=False)):
        try:
            res = pair_stats(r.coin, r.ex_a, r.ex_b, do_heavy=do_heavy)
        except Exception as e:
            res = None
            print(f"  ERR {r.coin} {r.ex_a}-{r.ex_b}: {e}")
        if res is None:
            skipped += 1
        else:
            rows.append(res)
        if (i + 1) % 100 == 0:
            elapsed = time.time() - t0
            rate = (i + 1) / elapsed
            eta = (len(pairs) - i - 1) / rate
            print(f"  [{i+1}/{len(pairs)}] {elapsed:.0f}s, ~{eta:.0f}s ETA, skipped={skipped}")
    print(f"Done {time.time()-t0:.0f}s; rows={len(rows)} skipped={skipped}")
    return pd.DataFrame(rows)


if __name__ == "__main__":
    pairs = pd.read_csv(OUTPUT / "02_pair_list.csv")
    do_heavy = os.getenv("HEAVY", "1") == "1"
    stats = compute_all(pairs, do_heavy=do_heavy)
    suffix = "" if do_heavy else "_lite"
    stats.to_csv(OUTPUT / f"03_pair_stats{suffix}.csv", index=False)
    print(f"Wrote 03_pair_stats{suffix}.csv: {len(stats)} rows")
    if len(stats):
        print("\nQuick view (top by abs_mean_bp):")
        print(stats.nlargest(15, "abs_mean_bp")[["coin","ex_a","ex_b","n_obs","mean_bp","stddev_bp","abs_mean_bp","crossings_per_hour"]].to_string(index=False))

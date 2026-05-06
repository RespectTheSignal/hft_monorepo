"""Step 5/6: spread time series + distribution + correlation heatmap."""
import sys
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
import pandas as pd
import seaborn as sns

sys.path.insert(0, str(Path(__file__).parent))
from spread_calc import _series_path
from pair_stats import load_aligned_spread

OUTPUT = Path(__file__).parent / "output"
PLOTS = OUTPUT / "plots"
PLOTS.mkdir(exist_ok=True)


def plot_pair(coin: str, ex_a: str, ex_b: str):
    df = load_aligned_spread(coin, ex_a, ex_b)
    if df is None:
        return False
    fig, axes = plt.subplots(2, 1, figsize=(12, 7))
    s = df["spread_bp_w"]
    mean = s.mean(); std = s.std()
    ax = axes[0]
    ax.plot(df.index, s, lw=0.4, color="steelblue")
    ax.axhline(mean, color="black", lw=1, label=f"mean={mean:+.2f}bp")
    ax.axhline(mean + 2*std, color="orange", lw=0.8, ls="--", label=f"±2σ ({std:.2f}bp)")
    ax.axhline(mean - 2*std, color="orange", lw=0.8, ls="--")
    ax.set_title(f"{coin}  spread bp = ({ex_a} - {ex_b}) / {ex_b} × 10000   (n={len(s)})")
    ax.set_ylabel("spread (bp)")
    ax.legend(loc="upper right")
    ax.grid(alpha=0.3)

    ax = axes[1]
    ax.hist(s, bins=80, color="steelblue", alpha=0.7)
    ax.axvline(mean, color="black", lw=1)
    ax.axvline(mean + 2*std, color="orange", lw=0.8, ls="--")
    ax.axvline(mean - 2*std, color="orange", lw=0.8, ls="--")
    ax.set_title("spread distribution (winsorized 0.1%/99.9%)")
    ax.set_xlabel("spread (bp)")
    ax.grid(alpha=0.3)

    fig.tight_layout()
    fig.savefig(PLOTS / f"{coin}_{ex_a}_{ex_b}_spread.png", dpi=110)
    plt.close(fig)
    return True


def plot_correlation_heatmap(coin: str, exchanges: list[str]):
    """Pivot mid prices, compute return correlation, plot heatmap."""
    series = {}
    for ex in exchanges:
        p = _series_path(ex, coin)
        if not p.exists(): continue
        series[ex] = pd.read_parquet(p)["mid"]
    if len(series) < 3: return False
    df = pd.DataFrame(series).dropna()
    if len(df) < 1000: return False
    rets = np.log(df).diff().dropna()
    corr = rets.corr()
    fig, ax = plt.subplots(figsize=(8, 6))
    sns.heatmap(corr, annot=True, fmt=".3f", vmin=0.9, vmax=1.0, cmap="viridis", ax=ax)
    ax.set_title(f"{coin} log-return correlation (1s, last 24h)")
    fig.tight_layout()
    fig.savefig(PLOTS / f"{coin}_corr_heatmap.png", dpi=110)
    plt.close(fig)
    return True


def render_top_pairs(scores: pd.DataFrame, n: int = 30):
    print(f"Plotting top {n} pairs...")
    for i, r in enumerate(scores.head(n).itertuples(index=False)):
        ok = plot_pair(r.coin, r.ex_a, r.ex_b)
        if (i + 1) % 5 == 0:
            print(f"  [{i+1}/{n}] last: {r.coin} {r.ex_a}-{r.ex_b} ok={ok}")


def render_correlation_for_top_coins(scores: pd.DataFrame, top_coins: int = 10):
    coins = list(dict.fromkeys(scores.head(50)["coin"].tolist()))[:top_coins]
    inv = pd.read_csv(OUTPUT / "01_inventory.csv")
    print(f"Plotting correlation heatmaps for: {coins}")
    for c in coins:
        exes = sorted(inv[(inv.coin == c) & (inv.ticks_24h >= 50000)]["exchange"].unique())
        plot_correlation_heatmap(c, exes)


if __name__ == "__main__":
    scores = pd.read_csv(OUTPUT / "04_pair_scores_taker_taker.csv")
    scores = scores[scores.score > 0].sort_values("score", ascending=False)
    render_top_pairs(scores, n=30)
    render_correlation_for_top_coins(scores, top_coins=10)
    print(f"Plots in {PLOTS}")

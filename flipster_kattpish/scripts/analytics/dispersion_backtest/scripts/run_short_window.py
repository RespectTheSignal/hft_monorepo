#!/usr/bin/env python3
"""
Short-window dispersion mean-reversion backtest.

Scope (frozen by user):
  Bases: BTC, ETH, SOL
  Exchanges (CEX index pool, equal weight): binance, bitget, kucoin, mexc, gate, bingx, kraken
  Window: 2026-05-02T07:21:12 UTC -> now (~4 days), 1s grid
  Steps: 3 (load+align) -> 5 (signal grid) -> 6 (cost-adj) -> 7 (output)

Usage:
  python3 run_short_window.py [--refetch] [--step <only one of: load|align|signal|all>]

Outputs to ../data/ and ../results/ and ../plots/.
"""

from __future__ import annotations

import argparse
import os
import sys
import time
from datetime import datetime, timezone
from typing import Iterable

import numpy as np
import pandas as pd
import psycopg2

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt


# ----- Config -----------------------------------------------------------------

QDB_HOST = "211.181.122.102"
QDB_PORT = 8812
QDB_DB = "qdb"
QDB_USER = "admin"
QDB_PASS = "quest"

BASES = ["BTC", "ETH", "SOL"]
EXCHANGES = ["binance", "bitget", "kucoin", "mexc", "gate", "bingx", "kraken"]

SYMBOL_MAP = {
    "binance": {"BTC": "BTC_USDT", "ETH": "ETH_USDT", "SOL": "SOL_USDT"},
    "bitget":  {"BTC": "BTC_USDT", "ETH": "ETH_USDT", "SOL": "SOL_USDT"},
    "kucoin":  {"BTC": "XBTUSDTM", "ETH": "ETHUSDTM", "SOL": "SOLUSDTM"},
    "mexc":    {"BTC": "BTC_USDT", "ETH": "ETH_USDT", "SOL": "SOL_USDT"},
    "gate":    {"BTC": "BTC_USDT", "ETH": "ETH_USDT", "SOL": "SOL_USDT"},
    "bingx":   {"BTC": "BTC-USDT", "ETH": "ETH-USDT", "SOL": "SOL-USDT"},
    "kraken":  {"BTC": "PF_XBTUSD", "ETH": "PF_ETHUSD", "SOL": "PF_SOLUSD"},
}

WINDOW_START = pd.Timestamp("2026-05-02T07:21:12", tz="UTC")
# WINDOW_END set at runtime to "now"

WARMUP_DROP = pd.Timedelta(minutes=30)
FFILL_LIMIT_S = 3
OUTLIER_BP = 50.0  # any exchange > 50bp from cross-median => excluded from index

THRESHOLDS = [1.0, 1.5, 2.0, 2.5, 3.0]
HOLDINGS_S = [1, 5, 10, 30, 60, 300]

ROLLING_WINDOW = pd.Timedelta(minutes=30)

# Cost scenarios (round-trip bp, total)
COST_SCENARIOS = {
    "S1_taker_hedge": 3.85,    # FL taker RT (0.85) + hedge taker RT (2.0) + slip (1.0)
    "S2_maker_hedge": 2.575,   # FL maker entry/taker exit (0.575) + hedge (1.0) + slip (1.0)
    "S3_directional": 1.85,    # FL taker RT (0.85) + slip (1.0); no hedge
}

# Paths
SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
ROOT_DIR = os.path.abspath(os.path.join(SCRIPT_DIR, ".."))
DATA_DIR = os.path.join(ROOT_DIR, "data")
RESULTS_DIR = os.path.join(ROOT_DIR, "results")
PLOTS_DIR = os.path.join(ROOT_DIR, "plots")
for d in (DATA_DIR, RESULTS_DIR, PLOTS_DIR):
    os.makedirs(d, exist_ok=True)


# ----- DB helpers -------------------------------------------------------------

def qdb_connect() -> psycopg2.extensions.connection:
    return psycopg2.connect(
        host=QDB_HOST, port=QDB_PORT, dbname=QDB_DB,
        user=QDB_USER, password=QDB_PASS,
        connect_timeout=15,
        # QuestDB defaults to 60s statement_timeout; bump generously
        options="-c statement_timeout=600000",
    )


def _fetch_chunk(conn, table: str, sym: str, s_iso: str, e_iso: str) -> pd.DataFrame:
    sql = (
        f"SELECT timestamp, bid_price, ask_price FROM {table} "
        f"WHERE symbol = %s AND timestamp >= %s AND timestamp < %s "
        f"AND bid_price > 0 AND ask_price > 0 "
        f"ORDER BY timestamp"
    )
    cur = conn.cursor()
    cur.execute(sql, (sym, s_iso, e_iso))
    rows = cur.fetchall()
    cur.close()
    if not rows:
        return pd.DataFrame(columns=["timestamp", "bid_price", "ask_price"])
    df = pd.DataFrame(rows, columns=["timestamp", "bid_price", "ask_price"])
    return df


def fetch_raw(exchange: str, base: str, start: pd.Timestamp, end: pd.Timestamp) -> pd.DataFrame:
    """Return DataFrame indexed by UTC timestamp, with bid_price / ask_price.
    Chunked by day to avoid statement_timeout on multi-million-row pulls."""
    sym = SYMBOL_MAP[exchange][base]
    table = f"{exchange}_bookticker"
    # Iterate day chunks (UTC midnight boundaries)
    chunks: list[pd.DataFrame] = []
    cur_start = start
    t0 = time.time()
    with qdb_connect() as conn:
        while cur_start < end:
            # next midnight after cur_start (or end, whichever sooner)
            next_mid = (cur_start + pd.Timedelta(days=1)).normalize()
            cur_end = min(next_mid, end)
            s_iso = cur_start.tz_convert("UTC").tz_localize(None).isoformat(sep=" ", timespec="microseconds")
            e_iso = cur_end.tz_convert("UTC").tz_localize(None).isoformat(sep=" ", timespec="microseconds")
            chunk = _fetch_chunk(conn, table, sym, s_iso, e_iso)
            chunks.append(chunk)
            cur_start = cur_end
    df = pd.concat(chunks, ignore_index=True) if chunks else pd.DataFrame(columns=["timestamp", "bid_price", "ask_price"])
    if not df.empty:
        df["timestamp"] = pd.to_datetime(df["timestamp"], utc=True)
        df = df.set_index("timestamp")
        df = df.astype({"bid_price": "float64", "ask_price": "float64"})
    else:
        df.index = pd.DatetimeIndex([], tz="UTC", name="timestamp")
    elapsed = time.time() - t0
    print(f"    [{exchange:8s}/{base}] {len(df):,} rows, {elapsed:.1f}s")
    return df


def raw_path(exchange: str, base: str) -> str:
    return os.path.join(DATA_DIR, f"raw_{exchange}_{base}.parquet")


def load_or_fetch_raw(exchange: str, base: str, start: pd.Timestamp, end: pd.Timestamp,
                     refetch: bool = False) -> pd.DataFrame:
    p = raw_path(exchange, base)
    if not refetch and os.path.exists(p):
        df = pd.read_parquet(p)
        # ensure index is utc tz-aware
        if df.index.tz is None:
            df.index = df.index.tz_localize("UTC")
        return df
    df = fetch_raw(exchange, base, start, end)
    df.to_parquet(p)
    return df


# ----- Step 3: align + index --------------------------------------------------

def align_base(base: str, raw: dict[str, pd.DataFrame], start: pd.Timestamp, end: pd.Timestamp,
               full_index: pd.DatetimeIndex) -> pd.DataFrame:
    """Build aligned 1s frame with mid_<ex>, index_mean, n_present."""
    cols: dict[str, pd.Series] = {}
    for ex, df in raw.items():
        if df.empty:
            cols[f"mid_{ex}"] = pd.Series(np.nan, index=full_index)
            continue
        mid = (df["bid_price"] + df["ask_price"]) / 2.0
        # resample to 1s by floor + last
        floored_index = mid.index.floor("1s")
        s1 = pd.Series(mid.values, index=floored_index)
        s1 = s1.groupby(level=0).last()
        # reindex to full grid + ffill limit 3
        s1 = s1.reindex(full_index).ffill(limit=FFILL_LIMIT_S)
        cols[f"mid_{ex}"] = s1
    aligned = pd.DataFrame(cols, index=full_index)

    # Outlier flag: any exchange > 50 bp from cross-median => excluded from index this slot
    mid_mat = aligned[[f"mid_{ex}" for ex in EXCHANGES]].to_numpy()  # shape (T, 7)
    median = np.nanmedian(mid_mat, axis=1)  # (T,)
    with np.errstate(divide="ignore", invalid="ignore"):
        dev_bp = (mid_mat - median[:, None]) / median[:, None] * 1e4
    outlier_mask = np.abs(dev_bp) > OUTLIER_BP  # True = exclude

    masked = mid_mat.copy()
    masked[outlier_mask] = np.nan
    n_present = (~np.isnan(masked)).sum(axis=1)
    with np.errstate(invalid="ignore"):
        index_mean = np.nanmean(masked, axis=1)
    aligned["index_mean"] = index_mean
    aligned["n_present"] = n_present

    # Drop warmup
    cutoff = start + WARMUP_DROP
    aligned = aligned.loc[aligned.index >= cutoff].copy()
    return aligned


def plot_coverage(base: str, aligned: pd.DataFrame) -> None:
    fig, ax = plt.subplots(figsize=(12, 5))
    bucket = aligned.index.floor("5min")
    for ex in EXCHANGES:
        s = (~aligned[f"mid_{ex}"].isna()).astype(float)
        cov = s.groupby(bucket).mean()
        ax.plot(cov.index, cov.values, label=ex, lw=1)
    ax.set_title(f"Coverage (5min bucket fraction non-NaN) — {base}")
    ax.set_ylabel("fraction non-NaN")
    ax.set_xlabel("UTC")
    ax.set_ylim(-0.02, 1.02)
    ax.grid(alpha=0.3)
    ax.legend(loc="lower left", ncol=4, fontsize=8)
    fig.autofmt_xdate()
    fig.tight_layout()
    fig.savefig(os.path.join(PLOTS_DIR, f"coverage_{base}.png"), dpi=110)
    plt.close(fig)


# ----- Step 4: z-score (right-exclusive rolling) ------------------------------

def compute_zscore(aligned: pd.DataFrame) -> pd.DataFrame:
    """For each exchange compute deviation_bp, sigma (right-exclusive 30min std), zscore."""
    out_cols: dict[str, pd.Series] = {}
    idx_mean = aligned["index_mean"]
    for ex in EXCHANGES:
        mid = aligned[f"mid_{ex}"]
        with np.errstate(divide="ignore", invalid="ignore"):
            dev = (mid - idx_mean) / idx_mean * 1e4
        # right-exclusive 30min std: include up to t-1s; we shift(1) for safety on time-window rolling
        # Use time-based rolling, then shift by 1 row (1s) so that sigma[t] uses [<= t-1s].
        sigma = dev.rolling(ROLLING_WINDOW, min_periods=60).std().shift(1)
        with np.errstate(divide="ignore", invalid="ignore"):
            z = dev / sigma
        out_cols[f"deviation_{ex}"] = dev
        out_cols[f"sigma_{ex}"] = sigma
        out_cols[f"zscore_{ex}"] = z
    z_df = pd.DataFrame(out_cols, index=aligned.index)
    return z_df


def assert_no_leakage(aligned: pd.DataFrame, z_df: pd.DataFrame) -> None:
    """Sanity check: at row t, sigma[t] must be computable from deviation[<t] only."""
    # Pick a representative non-edge row
    ex = EXCHANGES[0]
    dev = (aligned[f"mid_{ex}"] - aligned["index_mean"]) / aligned["index_mean"] * 1e4
    # At row t, manual right-exclusive: dev rolling 30min ending at t (inclusive), then shift(1)
    manual = dev.rolling(ROLLING_WINDOW, min_periods=60).std().shift(1)
    diff = (manual - z_df[f"sigma_{ex}"]).abs().fillna(0).max()
    assert diff < 1e-9, f"sigma leakage check failed for {ex}: max diff {diff}"
    print(f"  no-leakage check passed (max diff {diff:.2e})")


# ----- Step 5: signal grid ----------------------------------------------------

def signal_stats_one(z: pd.Series, mid: pd.Series, threshold: float, holding_s: int) -> dict:
    """For an exchange's zscore series and mid series, compute first-crossing entries and stats."""
    abs_z = z.abs()
    # entries: |z[t-1]| < th AND |z[t]| >= th
    prev = abs_z.shift(1)
    entry_mask = (prev < threshold) & (abs_z >= threshold) & abs_z.notna() & prev.notna()
    entries = entry_mask[entry_mask].index
    if len(entries) == 0:
        return dict(n_signals=0, mean_bp=np.nan, std_bp=np.nan, median_bp=np.nan,
                    hit_rate=np.nan, t_stat=np.nan, total_bp_sum=np.nan, low_n=True)

    # direction = -sign(z[entry])
    z_at = z.loc[entries].values
    direction = -np.sign(z_at)

    # forward returns: mid[t+holding] / mid[t] - 1
    # vectorize: need mid at entries + holding
    target_idx = entries + pd.Timedelta(seconds=holding_s)
    # use reindex to align
    mid_at = mid.reindex(entries).values
    mid_fwd = mid.reindex(target_idx).values
    # drop signals where either is NaN
    valid = (~np.isnan(mid_at)) & (~np.isnan(mid_fwd)) & (mid_at > 0)
    if valid.sum() == 0:
        return dict(n_signals=0, mean_bp=np.nan, std_bp=np.nan, median_bp=np.nan,
                    hit_rate=np.nan, t_stat=np.nan, total_bp_sum=np.nan, low_n=True)
    mid_at = mid_at[valid]
    mid_fwd = mid_fwd[valid]
    direction = direction[valid]
    fwd_ret_bp = direction * (mid_fwd - mid_at) / mid_at * 1e4
    n = len(fwd_ret_bp)
    mean_bp = float(np.mean(fwd_ret_bp))
    std_bp = float(np.std(fwd_ret_bp, ddof=1)) if n > 1 else np.nan
    med_bp = float(np.median(fwd_ret_bp))
    hit = float(np.mean(fwd_ret_bp > 0))
    t_stat = mean_bp / (std_bp / np.sqrt(n)) if (std_bp and std_bp > 0 and n > 1) else np.nan
    total_bp = float(np.sum(fwd_ret_bp))
    return dict(n_signals=n, mean_bp=mean_bp, std_bp=std_bp, median_bp=med_bp,
                hit_rate=hit, t_stat=t_stat, total_bp_sum=total_bp,
                low_n=(n < 100))


def build_signal_grid(aligned: dict[str, pd.DataFrame], z_dfs: dict[str, pd.DataFrame]) -> pd.DataFrame:
    rows: list[dict] = []
    for base in BASES:
        ali = aligned[base]
        zdf = z_dfs[base]
        for ex in EXCHANGES:
            mid = ali[f"mid_{ex}"]
            z = zdf[f"zscore_{ex}"]
            for th in THRESHOLDS:
                for h in HOLDINGS_S:
                    s = signal_stats_one(z, mid, th, h)
                    s["base"] = base
                    s["exchange"] = ex
                    s["threshold"] = th
                    s["holding_s"] = h
                    rows.append(s)
    return pd.DataFrame(rows)


# ----- Step 6: cost adjustment ------------------------------------------------

def add_cost_columns(stats: pd.DataFrame) -> pd.DataFrame:
    for name, c in COST_SCENARIOS.items():
        stats[f"net_bp_{name}"] = stats["mean_bp"] - c
    return stats


# ----- Step 7: output ---------------------------------------------------------

def write_top10(stats: pd.DataFrame, scenario_key: str, label: str) -> None:
    col = f"net_bp_{scenario_key}"
    df = stats[(~stats["low_n"]) & (stats["mean_bp"].notna())].copy()
    df = df.sort_values(col, ascending=False).head(10)
    cols = ["base", "exchange", "threshold", "holding_s", "n_signals",
            "mean_bp", "std_bp", "t_stat", "hit_rate", "median_bp", col]
    out = df[cols].copy()
    out["mean_bp"] = out["mean_bp"].round(3)
    out["std_bp"] = out["std_bp"].round(3)
    out["t_stat"] = out["t_stat"].round(2)
    out["hit_rate"] = out["hit_rate"].round(3)
    out["median_bp"] = out["median_bp"].round(3)
    out[col] = out[col].round(3)
    md_path = os.path.join(RESULTS_DIR, f"top10_{label}.md")
    with open(md_path, "w") as f:
        f.write(f"# Top 10 by {col} (n_signals >= 100)\n\n")
        f.write(f"Cost scenario {label}: {COST_SCENARIOS[scenario_key]} bp round-trip\n\n")
        f.write(out.to_markdown(index=False))
        f.write("\n")
    print(f"  Wrote {md_path}")


def plot_heatmaps(stats: pd.DataFrame) -> None:
    for base in BASES:
        for ex in EXCHANGES:
            sub = stats[(stats["base"] == base) & (stats["exchange"] == ex)].copy()
            if sub.empty:
                continue
            piv_mean = sub.pivot(index="threshold", columns="holding_s", values="mean_bp")
            piv_n = sub.pivot(index="threshold", columns="holding_s", values="n_signals")
            piv_mean = piv_mean.reindex(index=THRESHOLDS, columns=HOLDINGS_S)
            piv_n = piv_n.reindex(index=THRESHOLDS, columns=HOLDINGS_S)
            mask = piv_n < 100
            data = piv_mean.copy()
            data_masked = data.where(~mask)
            fig, ax = plt.subplots(figsize=(8, 5))
            vmax = max(abs(np.nanmin(data_masked.values)), abs(np.nanmax(data_masked.values))) if data_masked.notna().any().any() else 1.0
            if not np.isfinite(vmax) or vmax == 0:
                vmax = 1.0
            im = ax.imshow(data_masked.values, cmap="RdYlGn", aspect="auto",
                           vmin=-vmax, vmax=vmax, origin="lower")
            ax.set_xticks(np.arange(len(HOLDINGS_S)))
            ax.set_xticklabels(HOLDINGS_S)
            ax.set_yticks(np.arange(len(THRESHOLDS)))
            ax.set_yticklabels(THRESHOLDS)
            ax.set_xlabel("holding (s)")
            ax.set_ylabel("threshold (z)")
            ax.set_title(f"mean_bp (gross) — {base} / {ex}\nn<100 cells masked, annotated with n")
            for i in range(len(THRESHOLDS)):
                for j in range(len(HOLDINGS_S)):
                    n = piv_n.values[i, j]
                    m = piv_mean.values[i, j]
                    if np.isnan(n):
                        continue
                    txt = f"n={int(n)}"
                    if not np.isnan(m):
                        txt = f"{m:.1f}\nn={int(n)}"
                    ax.text(j, i, txt, ha="center", va="center", fontsize=7,
                            color="black" if not mask.values[i, j] else "gray")
            fig.colorbar(im, ax=ax, label="mean_bp (gross)")
            fig.tight_layout()
            fig.savefig(os.path.join(PLOTS_DIR, f"heatmap_{base}_{ex}.png"), dpi=110)
            plt.close(fig)


def plot_best_dist(stats: pd.DataFrame, aligned: dict[str, pd.DataFrame],
                   z_dfs: dict[str, pd.DataFrame]) -> dict | None:
    """Pick the single best gross-mean_bp combo with n>=100 and replot its distribution."""
    df = stats[(~stats["low_n"]) & (stats["mean_bp"].notna())]
    if df.empty:
        return None
    best = df.sort_values("mean_bp", ascending=False).iloc[0]
    base, ex = best["base"], best["exchange"]
    th, h = float(best["threshold"]), int(best["holding_s"])

    # Recompute returns array
    z = z_dfs[base][f"zscore_{ex}"]
    mid = aligned[base][f"mid_{ex}"]
    abs_z = z.abs()
    prev = abs_z.shift(1)
    entry_mask = (prev < th) & (abs_z >= th) & abs_z.notna() & prev.notna()
    entries = entry_mask[entry_mask].index
    if len(entries) == 0:
        return None
    z_at = z.loc[entries].values
    direction = -np.sign(z_at)
    target_idx = entries + pd.Timedelta(seconds=h)
    mid_at = mid.reindex(entries).values
    mid_fwd = mid.reindex(target_idx).values
    valid = (~np.isnan(mid_at)) & (~np.isnan(mid_fwd)) & (mid_at > 0)
    fwd = direction[valid] * (mid_fwd[valid] - mid_at[valid]) / mid_at[valid] * 1e4

    fig, ax = plt.subplots(figsize=(10, 5))
    ax.hist(fwd, bins=80, color="steelblue", alpha=0.85, edgecolor="black", lw=0.3)
    ax.axvline(0, color="red", lw=1.2, linestyle="--", label="0 bp")
    ax.axvline(np.mean(fwd), color="black", lw=1.2, linestyle="-",
               label=f"mean={np.mean(fwd):.2f} bp")
    ax.set_title(f"Forward-return distribution — BEST = {base} / {ex} / th={th} / hold={h}s\n"
                 f"n={len(fwd)}, mean={np.mean(fwd):.2f}, t={best['t_stat']:.2f}, hit={best['hit_rate']:.2%}")
    ax.set_xlabel("forward return (bp)")
    ax.set_ylabel("count")
    ax.legend()
    ax.grid(alpha=0.3)
    fig.tight_layout()
    fig.savefig(os.path.join(PLOTS_DIR, "forward_return_dist_BEST.png"), dpi=110)
    plt.close(fig)
    return dict(base=base, exchange=ex, threshold=th, holding_s=h,
                n=len(fwd), mean=float(np.mean(fwd)), t=float(best["t_stat"]),
                hit=float(best["hit_rate"]))


def write_headline(stats: pd.DataFrame, aligned: dict[str, pd.DataFrame],
                   coverage_summary: dict[str, dict], best_combo: dict | None,
                   timings: dict[str, float], window_start: pd.Timestamp,
                   window_end: pd.Timestamp) -> None:
    """1-page concise report."""
    p = os.path.join(RESULTS_DIR, "headline.md")
    df = stats[(~stats["low_n"]) & (stats["mean_bp"].notna())].copy()

    # Per (base,ex) best gross
    best_gross_rows = []
    for base in BASES:
        for ex in EXCHANGES:
            sub = df[(df["base"] == base) & (df["exchange"] == ex)]
            if sub.empty:
                best_gross_rows.append(dict(base=base, exchange=ex, mean_bp=np.nan, t_stat=np.nan,
                                             threshold=np.nan, holding_s=np.nan, n_signals=0))
                continue
            r = sub.sort_values("mean_bp", ascending=False).iloc[0]
            best_gross_rows.append(dict(base=base, exchange=ex,
                                         mean_bp=r["mean_bp"], t_stat=r["t_stat"],
                                         threshold=r["threshold"], holding_s=int(r["holding_s"]),
                                         n_signals=int(r["n_signals"])))
    bg = pd.DataFrame(best_gross_rows)

    # Per (base,ex) best net per scenario
    best_net = {sc: [] for sc in COST_SCENARIOS}
    for base in BASES:
        for ex in EXCHANGES:
            sub = df[(df["base"] == base) & (df["exchange"] == ex)]
            for sc in COST_SCENARIOS:
                col = f"net_bp_{sc}"
                if sub.empty:
                    best_net[sc].append(dict(base=base, exchange=ex, net_bp=np.nan,
                                              threshold=np.nan, holding_s=np.nan,
                                              t_stat=np.nan, n_signals=0))
                    continue
                r = sub.sort_values(col, ascending=False).iloc[0]
                best_net[sc].append(dict(base=base, exchange=ex, net_bp=r[col],
                                          threshold=r["threshold"], holding_s=int(r["holding_s"]),
                                          t_stat=r["t_stat"], n_signals=int(r["n_signals"])))

    # Combos with t>2 AND n>=100 AND net_S2>0
    sig_combos = df[(df["t_stat"] > 2) & (df["n_signals"] >= 100) & (df["net_bp_S2_maker_hedge"] > 0)]

    # Most-often lagger: highest count of positive mean_bp signals (n>=100) per exchange
    lagger_score = (df[df["mean_bp"] > 0].groupby("exchange").size()
                    .reindex(EXCHANGES, fill_value=0))
    top_lagger = lagger_score.idxmax() if lagger_score.max() > 0 else "(none)"

    # Strongest base
    base_score = df.groupby("base")["mean_bp"].max().reindex(BASES)

    lines = []
    lines.append("# Dispersion mean-revert backtest — short window headline\n")
    lines.append(f"Window: **{window_start.isoformat()}** → **{window_end.isoformat()}** "
                 f"(~{(window_end - window_start).total_seconds()/86400:.2f} days, 1s grid)")
    lines.append("Bases: BTC, ETH, SOL  |  Index pool (equal-weight): "
                 + ", ".join(EXCHANGES))
    lines.append("Sigma rolling: 30min, right-exclusive (`shift(1)`).  Outlier flag: |dev| > 50bp from cross-median.")
    lines.append("")
    lines.append("## Wall-clock per step")
    for k, v in timings.items():
        lines.append(f"- {k}: {v:.1f}s")
    lines.append("")
    lines.append("## Coverage / data quality")
    for base in BASES:
        cs = coverage_summary[base]
        lines.append(f"- **{base}**: post-warmup rows = {cs['n_rows']:,}; "
                     f"slot-mean n_present = {cs['mean_n_present']:.2f}/7; "
                     f"slot-min n_present = {cs['min_n_present']}.  "
                     f"Per-ex coverage (fraction non-NaN): "
                     + ", ".join(f"{ex}={cs['per_ex_cov'][ex]:.1%}" for ex in EXCHANGES))
    lines.append("")
    lines.append("## Per (base, exchange) best gross mean_bp")
    lines.append("")
    lines.append("| base | ex | thr | hold(s) | n | mean_bp | t |")
    lines.append("|---|---|---|---|---|---|---|")
    for r in best_gross_rows:
        if np.isnan(r["mean_bp"]):
            lines.append(f"| {r['base']} | {r['exchange']} | — | — | {r['n_signals']} | — | — |")
        else:
            lines.append(f"| {r['base']} | {r['exchange']} | {r['threshold']} | {r['holding_s']} | "
                         f"{r['n_signals']} | {r['mean_bp']:.2f} | {r['t_stat']:.2f} |")
    lines.append("")
    for sc, label in COST_SCENARIOS.items():
        lines.append(f"## Per (base, exchange) best net_bp ({sc} = {label} bp)")
        lines.append("")
        lines.append("| base | ex | thr | hold(s) | n | net_bp | t |")
        lines.append("|---|---|---|---|---|---|---|")
        for r in best_net[sc]:
            if np.isnan(r["net_bp"]):
                lines.append(f"| {r['base']} | {r['exchange']} | — | — | {r['n_signals']} | — | — |")
            else:
                lines.append(f"| {r['base']} | {r['exchange']} | {r['threshold']} | {r['holding_s']} | "
                             f"{r['n_signals']} | {r['net_bp']:.2f} | {r['t_stat']:.2f} |")
        lines.append("")
    lines.append("## Significance counts")
    lines.append(f"- combos with `t_stat>2 AND n>=100 AND net_S2>0`: **{len(sig_combos)}**")
    lines.append(f"- most-often-lagger exchange (most combos with positive mean_bp, n>=100): **{top_lagger}** "
                 f"(score map: " + ", ".join(f"{e}={lagger_score[e]}" for e in EXCHANGES) + ")")
    lines.append(f"- strongest base by max gross mean_bp: " +
                 ", ".join(f"{b}={base_score[b]:.2f}bp" if not np.isnan(base_score[b]) else f"{b}=NA"
                           for b in BASES))
    lines.append("")
    if best_combo is not None:
        lines.append("## Best single combo (gross)")
        lines.append(f"- **{best_combo['base']} / {best_combo['exchange']} / threshold={best_combo['threshold']} / hold={best_combo['holding_s']}s**")
        lines.append(f"- n={best_combo['n']}, mean_bp={best_combo['mean']:.2f}, t={best_combo['t']:.2f}, hit_rate={best_combo['hit']:.1%}")
        lines.append("")

    # Best net under S2 (the most realistic scenario) for go/no-go
    best_s2 = df.sort_values("net_bp_S2_maker_hedge", ascending=False).head(1)
    if not best_s2.empty:
        r2 = best_s2.iloc[0]
        lines.append("## Best net under S2 (FL maker entry / taker exit + hedge + slip = 2.575 bp)")
        lines.append(f"- **{r2['base']} / {r2['exchange']} / thr={r2['threshold']} / hold={int(r2['holding_s'])}s**")
        lines.append(f"- n={int(r2['n_signals'])}, gross_mean={r2['mean_bp']:.2f} bp, "
                     f"net_S2={r2['net_bp_S2_maker_hedge']:.2f} bp, t={r2['t_stat']:.2f}, "
                     f"hit_rate={r2['hit_rate']:.1%}")
        lines.append("")

    # Recommendation
    lines.append("## Go / no-go")
    decision_reason = []
    n_sig_significant = len(sig_combos)
    has_strong = (not best_s2.empty) and best_s2.iloc[0]["t_stat"] > 2 and best_s2.iloc[0]["net_bp_S2_maker_hedge"] > 0
    if has_strong and n_sig_significant >= 5:
        verdict = "**alpha exists**"
        decision_reason.append(f"{n_sig_significant} combos pass t>2 + n>=100 + net_S2>0; "
                                f"best net_S2 is {best_s2.iloc[0]['net_bp_S2_maker_hedge']:.2f} bp at t={best_s2.iloc[0]['t_stat']:.2f}.")
    elif has_strong and n_sig_significant >= 1:
        verdict = "**inconclusive — needs more data / OOS check**"
        decision_reason.append(f"only {n_sig_significant} combo(s) pass strict significance test, "
                                "single-combo edge is fragile on a 4-day window.")
    elif (not best_s2.empty) and best_s2.iloc[0]["mean_bp"] > 0:
        verdict = "**inconclusive — gross positive but does not survive cost**"
        decision_reason.append("gross mean_bp positive but net_S2 not significantly > 0 (t<=2 or net<=0).")
    else:
        verdict = "**alpha does not exist (under S2 cost)**"
        decision_reason.append("no combos with positive gross mean and adequate t-stat pass S2 cost.")
    lines.append(f"Verdict: {verdict}")
    for r in decision_reason:
        lines.append(f"- {r}")
    lines.append("")
    with open(p, "w") as f:
        f.write("\n".join(lines))
    print(f"  Wrote {p}")


# ----- Main -------------------------------------------------------------------

def coverage_summary_one(base: str, aligned: pd.DataFrame) -> dict:
    return dict(
        n_rows=len(aligned),
        mean_n_present=float(aligned["n_present"].mean()),
        min_n_present=int(aligned["n_present"].min()),
        per_ex_cov={ex: float((~aligned[f"mid_{ex}"].isna()).mean()) for ex in EXCHANGES},
    )


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--refetch", action="store_true", help="ignore cached parquet, repull")
    args = ap.parse_args(argv)

    timings: dict[str, float] = {}
    window_end = pd.Timestamp.now(tz="UTC").floor("s")
    print(f"Window: {WINDOW_START.isoformat()} -> {window_end.isoformat()}")
    print(f"Bases: {BASES}")
    print(f"Exchanges: {EXCHANGES}")

    full_index = pd.date_range(WINDOW_START, window_end, freq="1s", inclusive="left")
    print(f"Full 1s grid: {len(full_index):,} rows ({(len(full_index)/86400):.2f} days)")

    # === Step 3: load raw + align (per base, free raw between bases) ===
    t_load = 0.0
    t_align = 0.0
    aligned: dict[str, pd.DataFrame] = {}
    coverage = {}
    for base in BASES:
        print(f"\n[load raw] base={base}")
        t0 = time.time()
        raw = {}
        for ex in EXCHANGES:
            raw[ex] = load_or_fetch_raw(ex, base, WINDOW_START, window_end,
                                        refetch=args.refetch)
        t_load += time.time() - t0

        print(f"[align] base={base}")
        t0 = time.time()
        ali = align_base(base, raw, WINDOW_START, window_end, full_index)
        aligned[base] = ali
        ali.to_parquet(os.path.join(DATA_DIR, f"aligned_{base}.parquet"))
        plot_coverage(base, ali)
        coverage[base] = coverage_summary_one(base, ali)
        print(f"  rows after warmup drop: {len(ali):,}; mean n_present={ali['n_present'].mean():.2f}")
        t_align += time.time() - t0

        del raw  # free
    timings["3a_load_raw"] = t_load
    timings["3b_align"] = t_align

    # === Step 4: z-score ===
    t0 = time.time()
    z_dfs: dict[str, pd.DataFrame] = {}
    for base in BASES:
        print(f"\n[zscore] base={base}")
        z = compute_zscore(aligned[base])
        assert_no_leakage(aligned[base], z)
        z_dfs[base] = z
        z.to_parquet(os.path.join(DATA_DIR, f"zscore_{base}.parquet"))
    timings["4_zscore"] = time.time() - t0

    # === Step 5: signal grid ===
    t0 = time.time()
    print(f"\n[signal grid] {len(THRESHOLDS)} thresholds * {len(HOLDINGS_S)} holdings * {len(BASES)} bases * {len(EXCHANGES)} exchanges = "
          f"{len(THRESHOLDS)*len(HOLDINGS_S)*len(BASES)*len(EXCHANGES)} combos")
    stats = build_signal_grid(aligned, z_dfs)
    timings["5_signal_grid"] = time.time() - t0

    # === Step 6: cost adjust ===
    stats = add_cost_columns(stats)
    stats.to_parquet(os.path.join(RESULTS_DIR, "signal_stats.parquet"))
    print(f"  Wrote {os.path.join(RESULTS_DIR, 'signal_stats.parquet')}")

    # === Step 7: outputs ===
    t0 = time.time()
    write_top10(stats, "S1_taker_hedge", "S1")
    write_top10(stats, "S2_maker_hedge", "S2")
    write_top10(stats, "S3_directional", "S3")
    plot_heatmaps(stats)
    best_combo = plot_best_dist(stats, aligned, z_dfs)
    write_headline(stats, aligned, coverage, best_combo, timings, WINDOW_START, window_end)
    timings["7_output"] = time.time() - t0

    print("\n=== DONE ===")
    for k, v in timings.items():
        print(f"  {k}: {v:.1f}s")
    print(f"  Results: {RESULTS_DIR}")
    print(f"  Plots:   {PLOTS_DIR}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

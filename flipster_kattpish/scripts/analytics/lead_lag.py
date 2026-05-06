"""Lead-lag analysis: does MEXC quote lag other exchanges?

Method: cross-correlation of log returns at lags ±30s.
Peak at positive lag means MEXC follows; negative lag means MEXC leads.
"""
import sys
from pathlib import Path

import numpy as np
import pandas as pd

sys.path.insert(0, str(Path(__file__).parent))
from spread_calc import _series_path

OUTPUT = Path(__file__).parent / "output"

LAG_RANGE = 30  # seconds


def cross_corr(coin: str, ex_a: str, ex_b: str, max_lag: int = LAG_RANGE) -> dict | None:
    """Returns {lags, corrs, peak_lag, peak_corr}.
    Positive peak_lag means ex_a lags ex_b (b's returns predict a's future returns).
    """
    pa = _series_path(ex_a, coin)
    pb = _series_path(ex_b, coin)
    if not pa.exists() or not pb.exists():
        return None
    a = pd.read_parquet(pa)["mid"]
    b = pd.read_parquet(pb)["mid"]
    df = pd.concat([a.rename("a"), b.rename("b")], axis=1).dropna()
    if len(df) < 1000:
        return None
    ra = np.log(df["a"]).diff().dropna()
    rb = np.log(df["b"]).diff().dropna()
    ra, rb = ra.align(rb, join="inner")
    lags = list(range(-max_lag, max_lag + 1))
    corrs = []
    for lag in lags:
        if lag < 0:
            c = ra.iloc[-lag:].corr(rb.iloc[:lag] if lag != 0 else rb)
        elif lag > 0:
            c = ra.iloc[:-lag].corr(rb.iloc[lag:])
        else:
            c = ra.corr(rb)
        corrs.append(float(c))
    corrs = np.array(corrs)
    peak_idx = int(np.argmax(corrs))
    peak_lag = lags[peak_idx]
    return {
        "coin": coin, "ex_a": ex_a, "ex_b": ex_b,
        "peak_lag_sec": peak_lag,
        "peak_corr": float(corrs[peak_idx]),
        "corr_at_0": float(corrs[max_lag]),
        "lags": lags,
        "corrs": corrs.tolist(),
    }


def analyze_top_pairs():
    """For each MEXC pair in qualifying list, compute lead-lag."""
    bt = pd.read_csv(OUTPUT / "05_backtest_realistic_taker_taker.csv")
    qual = bt[(bt.n_trades>=10)&(bt.win_rate>=0.6)&(bt.total_pnl_bp>0)]
    rows = []
    for r in qual.itertuples(index=False):
        # Use convention: ex_a = MEXC, ex_b = other (so positive peak_lag = MEXC lags other)
        if r.ex_a == "mexc":
            mexc, other = r.ex_a, r.ex_b
        elif r.ex_b == "mexc":
            mexc, other = r.ex_b, r.ex_a
        else:
            # No MEXC in this pair, still compute for reference
            res = cross_corr(r.coin, r.ex_a, r.ex_b)
            if res:
                rows.append({"coin": r.coin, "lagger": r.ex_a, "leader": r.ex_b,
                             **{k:v for k,v in res.items() if k not in ("lags","corrs","coin","ex_a","ex_b")}})
            continue
        res = cross_corr(r.coin, mexc, other)
        if res:
            rows.append({"coin": r.coin, "lagger": mexc, "leader": other,
                         "peak_lag_sec": res["peak_lag_sec"],
                         "peak_corr": res["peak_corr"],
                         "corr_at_0": res["corr_at_0"]})
    return pd.DataFrame(rows)


def cross_exchange_lead_lag_summary(coin: str, exchanges: list[str], max_lag: int = 10):
    """For one coin, build a matrix: peak_lag[i,j] for each (i, j) pair."""
    series = {}
    for ex in exchanges:
        p = _series_path(ex, coin)
        if not p.exists(): continue
        series[ex] = pd.read_parquet(p)["mid"]
    if len(series) < 2: return None
    df = pd.DataFrame(series).dropna()
    if len(df) < 2000: return None
    rets = np.log(df).diff().dropna()
    n = len(exchanges)
    mat = pd.DataFrame(np.nan, index=exchanges, columns=exchanges, dtype=float)
    for a in exchanges:
        if a not in rets: continue
        for b in exchanges:
            if a == b or b not in rets: continue
            ra, rb = rets[a], rets[b]
            best_lag, best_c = 0, -1
            for lag in range(-max_lag, max_lag + 1):
                if lag < 0:
                    c = ra.iloc[-lag:].corr(rb.iloc[:lag] if lag != 0 else rb)
                elif lag > 0:
                    c = ra.iloc[:-lag].corr(rb.iloc[lag:])
                else:
                    c = ra.corr(rb)
                if c > best_c:
                    best_c = c
                    best_lag = lag
            # peak_lag = +k means a's returns are best predicted by b shifted forward k seconds,
            # i.e. b leads a by k seconds (a lags b by k seconds).
            mat.loc[a, b] = best_lag
    return mat


if __name__ == "__main__":
    print("=== Lead-lag for MEXC pairs in qualifying list ===")
    df = analyze_top_pairs()
    df = df.sort_values("peak_lag_sec", ascending=False)
    print(df.to_string(index=False))
    print(f"\n  peak_lag_sec mean: {df.peak_lag_sec.mean():+.2f}s")
    print(f"  peak_lag_sec median: {df.peak_lag_sec.median():+.2f}s")
    print(f"  peak_lag > 0 (lagger genuinely lags): {(df.peak_lag_sec > 0).sum()}/{len(df)}")
    print(f"  peak_lag = 0 (synchronous): {(df.peak_lag_sec == 0).sum()}")
    print(f"  peak_lag < 0 (lagger actually LEADS!): {(df.peak_lag_sec < 0).sum()}")

    df.to_csv(OUTPUT / "06_lead_lag_qualifying.csv", index=False)

    print("\n=== Per-coin full lead-lag matrix (rows lag cols by N seconds) ===")
    inv = pd.read_csv(OUTPUT / "01_inventory.csv")
    for coin in ["SKYAI", "TAG", "BSB", "UB", "LAB", "BTC", "ETH"]:
        exes = sorted(inv[(inv.coin==coin) & (inv.ticks_24h>=50000)]["exchange"].unique())
        m = cross_exchange_lead_lag_summary(coin, exes, max_lag=10)
        if m is None:
            continue
        print(f"\n--- {coin} ({', '.join(exes)}) ---")
        print(m.to_string(float_format=lambda x: f"{x:+.0f}" if pd.notna(x) else "  -"))
        # interpret: row's avg lag (how much does this exchange lag on average)
        avg_lag = m.mean(axis=1)
        print("\n avg_lag_sec (rowwise) — positive = exchange tends to lag others:")
        print(avg_lag.sort_values(ascending=False).round(1).to_string())

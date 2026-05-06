"""Sub-second lead-lag analysis using raw ticks (no resampling).

Two complementary methods:

1) Cross-correlation at high resolution (50ms bins): same as before but finer.
2) Event study: identify "shock" ticks on ex_a where |price change| > 3σ_a.
   Look at ex_b's price relative to its level just before the shock, in 50ms bins
   from t=-1s to t=+5s. If ex_b reacts at t>0 with significant move, ex_b lags ex_a.

Method 2 is more interpretable: it gives the actual response time in milliseconds.
"""
import os
import sys
import time
from pathlib import Path

import numpy as np
import pandas as pd
import psycopg2

sys.path.insert(0, str(Path(__file__).parent))
from data_loader import pg_conn

OUTPUT = Path(__file__).parent / "output"
TICK_CACHE = OUTPUT / "tick_cache"
TICK_CACHE.mkdir(exist_ok=True)

WINDOW_HOURS = 24


def fetch_ticks(exchange: str, raw_symbol: str, hours: int = WINDOW_HOURS,
                force: bool = False) -> pd.DataFrame:
    """Pull raw bid/ask ticks (no resampling). Cached as parquet keyed on (ex, sym, hours)."""
    safe_sym = raw_symbol.replace("/", "_").replace("-", "_")
    p = TICK_CACHE / f"{exchange}_{safe_sym}_{hours}h.parquet"
    if p.exists() and not force:
        return pd.read_parquet(p)
    sql = (
        f"SELECT timestamp, bid_price, ask_price "
        f"FROM {exchange}_bookticker "
        f"WHERE symbol = %s AND timestamp > dateadd('h', -{hours}, now()) "
        f"ORDER BY timestamp"
    )
    with pg_conn() as c:
        df = pd.read_sql(sql, c, params=(raw_symbol,))
    if df.empty:
        return df
    df["timestamp"] = pd.to_datetime(df["timestamp"], utc=True)
    df = df.set_index("timestamp")
    df["mid"] = (df["bid_price"] + df["ask_price"]) / 2.0
    # drop pathological (bid >= ask) ticks
    df = df[df["ask_price"] > df["bid_price"]]
    df.to_parquet(p)
    return df


def lookup_raw_symbol(exchange: str, coin: str) -> str | None:
    inv = pd.read_csv(OUTPUT / "01_inventory.csv")
    rows = inv[(inv.exchange == exchange) & (inv.coin == coin)]
    if rows.empty:
        return None
    return rows.sort_values("ticks_24h", ascending=False).iloc[0]["raw_symbol"]


def merge_pair_ticks(coin: str, ex_a: str, ex_b: str, hours: int = WINDOW_HOURS) -> pd.DataFrame | None:
    """Merge two raw tick streams via merge_asof; carry-forward last quote per side."""
    sa = lookup_raw_symbol(ex_a, coin); sb = lookup_raw_symbol(ex_b, coin)
    if not sa or not sb:
        return None
    a = fetch_ticks(ex_a, sa, hours=hours).rename(columns=lambda c: f"{c}_a")
    b = fetch_ticks(ex_b, sb, hours=hours).rename(columns=lambda c: f"{c}_b")
    if a.empty or b.empty:
        return None
    # union of timestamps
    a = a.reset_index().rename(columns={"timestamp":"ts"})
    b = b.reset_index().rename(columns={"timestamp":"ts"})
    # use merge_asof: for each tick in unioned timeline, carry last bid/ask from each side
    union = pd.concat([a[["ts"]].assign(_src="a"), b[["ts"]].assign(_src="b")]).sort_values("ts")
    union = union.drop_duplicates("ts")
    df = pd.merge_asof(union[["ts"]], a, on="ts", direction="backward")
    df = pd.merge_asof(df, b, on="ts", direction="backward")
    df = df.dropna()
    df = df.set_index("ts")
    return df


def event_study(coin: str, ex_a: str, ex_b: str, shock_sigma: float = 3.0,
                bin_ms: int = 50, pre_sec: int = 1, post_sec: int = 5,
                hours: int = WINDOW_HOURS, max_events: int = 2000) -> dict | None:
    """Identify shocks on ex_a; track ex_b response in ms bins. Symmetric run also done.

    Returns dict with `bin_centers_ms` (1D), `resp_a_when_a_shock` (1D normalized),
    `resp_b_when_a_shock` (1D normalized), and inverse case.
    """
    df = merge_pair_ticks(coin, ex_a, ex_b, hours=hours)
    if df is None or len(df) < 5000:
        return None

    # Use mid_a, mid_b
    mid_a = df["mid_a"].to_numpy()
    mid_b = df["mid_b"].to_numpy()
    ts_ns = df.index.values.astype("datetime64[ns]").astype("int64")

    # Compute log returns at tick granularity for shock detection
    ra = np.diff(np.log(mid_a))
    rb = np.diff(np.log(mid_b))
    sigma_a = ra.std()
    sigma_b = rb.std()

    bin_edges_ms = np.arange(-pre_sec * 1000, post_sec * 1000 + bin_ms, bin_ms)
    n_bins = len(bin_edges_ms) - 1
    bin_centers_ms = (bin_edges_ms[:-1] + bin_edges_ms[1:]) / 2

    def _study(side_label: str, shock_ret: np.ndarray, sigma: float,
               leader_mid: np.ndarray, follower_mid: np.ndarray) -> tuple[np.ndarray, np.ndarray, int]:
        # Find shock indices: |shock_ret| > shock_sigma * sigma
        shock_idx = np.where(np.abs(shock_ret) > shock_sigma * sigma)[0] + 1  # +1 because diff loses 1
        if len(shock_idx) > max_events:
            shock_idx = np.random.RandomState(42).choice(shock_idx, max_events, replace=False)
        leader_resp_sum = np.zeros(n_bins); leader_resp_count = np.zeros(n_bins)
        follower_resp_sum = np.zeros(n_bins); follower_resp_count = np.zeros(n_bins)
        for i in shock_idx:
            sign = np.sign(shock_ret[i-1])
            t0 = ts_ns[i]
            base_leader = leader_mid[i-1]   # price just before shock
            base_follower = follower_mid[i-1]
            # window
            lo = t0 + (-pre_sec) * 1_000_000_000
            hi = t0 + post_sec * 1_000_000_000
            j_lo = np.searchsorted(ts_ns, lo, side="left")
            j_hi = np.searchsorted(ts_ns, hi, side="right")
            if j_hi - j_lo < 2: continue
            for j in range(j_lo, j_hi):
                dt_ms = (ts_ns[j] - t0) // 1_000_000  # nanoseconds → ms
                bin_i = (dt_ms - bin_edges_ms[0]) // bin_ms
                if bin_i < 0 or bin_i >= n_bins: continue
                # normalized response: log return × shock sign
                rl = sign * np.log(leader_mid[j] / base_leader)
                rf = sign * np.log(follower_mid[j] / base_follower)
                leader_resp_sum[int(bin_i)] += rl; leader_resp_count[int(bin_i)] += 1
                follower_resp_sum[int(bin_i)] += rf; follower_resp_count[int(bin_i)] += 1
        leader_avg = np.where(leader_resp_count > 0, leader_resp_sum / leader_resp_count, np.nan)
        follower_avg = np.where(follower_resp_count > 0, follower_resp_sum / follower_resp_count, np.nan)
        return leader_avg, follower_avg, len(shock_idx)

    a_lead_a, a_lead_b, n_a_shocks = _study("a_shock", ra, sigma_a, mid_a, mid_b)
    b_lead_b, b_lead_a, n_b_shocks = _study("b_shock", rb, sigma_b, mid_b, mid_a)

    return {
        "coin": coin, "ex_a": ex_a, "ex_b": ex_b,
        "n_ticks": int(len(df)),
        "n_a_shocks": int(n_a_shocks),
        "n_b_shocks": int(n_b_shocks),
        "bin_centers_ms": bin_centers_ms.tolist(),
        "a_when_a_shock_bp": (a_lead_a * 10000).tolist(),  # leader response (a moves due to shock_a)
        "b_when_a_shock_bp": (a_lead_b * 10000).tolist(),  # follower response (b moves after shock_a)
        "b_when_b_shock_bp": (b_lead_b * 10000).tolist(),
        "a_when_b_shock_bp": (b_lead_a * 10000).tolist(),
    }


def lead_lag_summary(result: dict) -> dict:
    """Summarize: at which lag does follower reach half of leader's response?
    Returns half-life-like response lag in ms."""
    bins = np.array(result["bin_centers_ms"])
    a_a = np.array(result["a_when_a_shock_bp"])
    a_b = np.array(result["b_when_a_shock_bp"])
    b_b = np.array(result["b_when_b_shock_bp"])
    b_a = np.array(result["a_when_b_shock_bp"])

    def t_half(leader_resp, follower_resp, bins):
        # Use t≥0 only
        mask = bins >= 0
        L = leader_resp[mask]; F = follower_resp[mask]; B = bins[mask]
        if len(L) == 0 or np.nanmax(L) <= 0: return np.nan
        # leader's "asymptote": value at last bin
        L_inf = np.nanmean(L[-5:])  # last 5 bins
        if L_inf <= 0: return np.nan
        target = 0.5 * L_inf
        ix = np.where(F >= target)[0]
        return float(B[ix[0]]) if len(ix) else np.nan

    return {
        "coin": result["coin"], "ex_a": result["ex_a"], "ex_b": result["ex_b"],
        "n_a_shocks": result["n_a_shocks"], "n_b_shocks": result["n_b_shocks"],
        "b_lag_after_a_shock_ms": t_half(a_a, a_b, bins),
        "a_lag_after_b_shock_ms": t_half(b_b, b_a, bins),
        # Final asymptote (response magnitude bp)
        "b_resp_asymptote_bp": float(np.nanmean(a_b[-5:])),
        "a_resp_asymptote_bp": float(np.nanmean(b_a[-5:])),
        "leader_a_asymptote_bp": float(np.nanmean(a_a[-5:])),
        "leader_b_asymptote_bp": float(np.nanmean(b_b[-5:])),
    }


PAIRS = [
    # MEXC pairs from qualifying list
    ("TAG", "bingx", "mexc"),
    ("TAG", "binance", "mexc"),
    ("TAG", "bitget", "mexc"),
    ("TAG", "gate", "mexc"),
    ("SKYAI", "gate", "mexc"),
    ("SKYAI", "bitget", "mexc"),
    ("SKYAI", "bingx", "mexc"),
    ("BSB", "bingx", "mexc"),
    ("BSB", "bybit", "mexc"),
    ("UB", "binance", "mexc"),
    ("UB", "mexc", "okx"),
    # Non-MEXC qualifying (control)
    ("BSB", "bingx", "bybit"),
    ("LAB", "bitget", "okx"),
    ("LAB", "binance", "okx"),
    # Major coin sanity check
    ("BTC", "binance", "mexc"),
    ("BTC", "binance", "gate"),
    ("BTC", "bingx", "binance"),
    ("ETH", "binance", "mexc"),
]

# Cross-exchange lag survey: test each candidate (col 'b') against binance (col 'a').
# Coin choices: BTC/ETH (universal), AAVE (on all 11 ex per coverage matrix).
LAG_SURVEY = []
for coin in ["BTC", "ETH", "AAVE"]:
    for cand in ["mexc", "kucoin", "kraken", "bitget", "gate", "okx", "bybit", "bingx", "backpack", "hyperliquid"]:
        LAG_SURVEY.append((coin, "binance", cand))
# Also test internal pairs (binance-bitget, gate-bitget, bingx-bitget) to see if those are the real leaders
for coin in ["BTC", "ETH"]:
    LAG_SURVEY += [(coin, "bitget", "gate"), (coin, "bitget", "bingx"), (coin, "bitget", "okx")]


if __name__ == "__main__":
    use_pairs = LAG_SURVEY if os.getenv("MODE","")== "survey" else PAIRS
    rows = []
    full_results = []
    t0 = time.time()
    for i, (coin, a, b) in enumerate(use_pairs):
        print(f"  [{i+1}/{len(use_pairs)}] {coin} {a} × {b}...", end=" ", flush=True)
        try:
            res = event_study(coin, a, b)
        except Exception as e:
            print(f"ERR {e}")
            continue
        if res is None:
            print("None")
            continue
        summary = lead_lag_summary(res)
        rows.append(summary)
        full_results.append(res)
        print(f"n_ticks={res['n_ticks']} a_shocks={res['n_a_shocks']} b_shocks={res['n_b_shocks']} "
              f"b_lag={summary['b_lag_after_a_shock_ms']:.0f}ms a_lag={summary['a_lag_after_b_shock_ms']:.0f}ms")
        print(f"     ↳ a_asymptote={summary['leader_a_asymptote_bp']:+.2f}bp b_response={summary['b_resp_asymptote_bp']:+.2f}bp "
              f"b_asymptote={summary['leader_b_asymptote_bp']:+.2f}bp a_response={summary['a_resp_asymptote_bp']:+.2f}bp")

    print(f"\nDone in {time.time()-t0:.0f}s")
    df = pd.DataFrame(rows)
    suffix = "_survey" if os.getenv("MODE","")== "survey" else ""
    df.to_csv(OUTPUT / f"07_lead_lag_subsec{suffix}.csv", index=False)
    print(f"\nWrote 07_lead_lag_subsec{suffix}.csv")
    print()
    print("=== Summary table ===")
    cols = ["coin","ex_a","ex_b","n_a_shocks","n_b_shocks",
            "b_lag_after_a_shock_ms","a_lag_after_b_shock_ms",
            "leader_a_asymptote_bp","b_resp_asymptote_bp",
            "leader_b_asymptote_bp","a_resp_asymptote_bp"]
    print(df[cols].to_string(index=False))

    # Save full event study curves for plotting
    import json
    with open(OUTPUT / "07_lead_lag_curves.json", "w") as f:
        json.dump(full_results, f, default=lambda x: float(x) if hasattr(x,"item") else x)

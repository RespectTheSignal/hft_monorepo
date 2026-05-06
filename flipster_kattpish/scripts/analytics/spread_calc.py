"""Step 2: per-(exchange, coin) 1s-resampled mid series → cached parquet.

Pulls bid/ask, resamples LAST mid per second over the analysis window.
For each coin, all relevant exchanges are dumped once. Pair spreads computed
later in memory by aligning on timestamp.
"""
import os
import sys
import time
from pathlib import Path

import numpy as np
import pandas as pd
import psycopg2

sys.path.insert(0, str(Path(__file__).parent))
from data_loader import EXCHANGES, pg_conn
from symbols import normalize

OUTPUT = Path(__file__).parent / "output"
CACHE = OUTPUT / "series_cache"
CACHE.mkdir(parents=True, exist_ok=True)

# Window: last 24h — fast multi-exchange sweep
WINDOW_HOURS = 24


def _series_path(exchange: str, coin: str) -> Path:
    return CACHE / f"{exchange}_{coin}.parquet"


def _raw_for_coin(exchange: str, coin: str, inv: pd.DataFrame) -> str | None:
    """Look up the raw_symbol(s) for (exchange, coin) from inventory."""
    rows = inv[(inv.exchange == exchange) & (inv.coin == coin)]
    if rows.empty:
        return None
    # Use the highest-volume raw symbol (e.g. if multiple variants exist)
    return rows.sort_values("ticks_24h", ascending=False).iloc[0]["raw_symbol"]


def fetch_series(exchange: str, coin: str, raw_symbol: str, hours: int = WINDOW_HOURS,
                 force: bool = False) -> pd.DataFrame:
    """Pull bid/ask for (exchange, coin); resample to 1s last bid/ask/mid + tick_count."""
    p = _series_path(exchange, coin)
    if p.exists() and not force:
        df = pd.read_parquet(p)
        # Migrate from old (mid-only) cache: re-fetch if missing bid_price
        if "bid_price" not in df.columns:
            return fetch_series(exchange, coin, raw_symbol, hours=hours, force=True)
        return df

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
    df["mid"] = (df["bid_price"] + df["ask_price"]) / 2.0
    df = df.set_index("timestamp")
    res = df.resample("1s").last()[["bid_price","ask_price","mid"]]
    res["tick_count"] = df["mid"].resample("1s").count().astype("int32")
    # forward-fill quotes within a small window; consumer can mask staler points
    for c in ("bid_price","ask_price","mid"):
        res[c] = res[c].ffill(limit=10)
    res = res.dropna(subset=["mid"])
    res.to_parquet(p)
    return res


def select_pairs(inv: pd.DataFrame, top_coins: int = 100, min_ticks: int = 50000) -> pd.DataFrame:
    """Build target pair list.

    For coins ranked top_coins by total ticks (across all exchanges with ≥min_ticks),
    enumerate all exchange pairs.
    """
    f = inv[inv.ticks_24h >= min_ticks].copy()
    coin_total = f.groupby("coin")["ticks_24h"].sum().sort_values(ascending=False)
    # Need ≥2 exchanges per coin
    coin_excount = f.groupby("coin")["exchange"].nunique()
    valid = coin_total[(coin_excount >= 2).reindex(coin_total.index, fill_value=False)]
    selected = valid.head(top_coins).index.tolist()
    rows = []
    for coin in selected:
        exes = sorted(f[f.coin == coin]["exchange"].unique())
        for i in range(len(exes)):
            for j in range(i + 1, len(exes)):
                rows.append((coin, exes[i], exes[j]))
    return pd.DataFrame(rows, columns=["coin", "ex_a", "ex_b"])


def cache_all_series(inv: pd.DataFrame, pairs: pd.DataFrame):
    """Pull series for every (exchange, coin) appearing in pairs."""
    needed = set()
    for r in pairs.itertuples(index=False):
        needed.add((r.ex_a, r.coin))
        needed.add((r.ex_b, r.coin))
    print(f"Caching {len(needed)} (exchange, coin) series...")
    t0 = time.time()
    failed = []
    for i, (ex, coin) in enumerate(sorted(needed)):
        raw = _raw_for_coin(ex, coin, inv)
        if raw is None:
            failed.append((ex, coin, "no_inv_match"))
            continue
        try:
            df = fetch_series(ex, coin, raw)
            if df.empty:
                failed.append((ex, coin, "empty"))
        except Exception as e:
            failed.append((ex, coin, str(e)[:80]))
        if (i + 1) % 50 == 0:
            elapsed = time.time() - t0
            rate = (i + 1) / elapsed
            eta = (len(needed) - i - 1) / rate
            print(f"  [{i+1}/{len(needed)}] {elapsed:.0f}s elapsed, ~{eta:.0f}s ETA, failed={len(failed)}")
    print(f"Done in {time.time()-t0:.0f}s. Failed: {len(failed)}")
    if failed:
        pd.DataFrame(failed, columns=["exchange","coin","reason"]).to_csv(OUTPUT / "02_series_failed.csv", index=False)


if __name__ == "__main__":
    inv = pd.read_csv(OUTPUT / "01_inventory.csv")
    top_n = int(os.getenv("TOP_COINS", "100"))
    min_ticks = int(os.getenv("MIN_TICKS", "50000"))
    pairs = select_pairs(inv, top_coins=top_n, min_ticks=min_ticks)
    print(f"Selected {pairs['coin'].nunique()} coins, {len(pairs)} pairs (top_coins={top_n}, min_ticks={min_ticks})")
    pairs.to_csv(OUTPUT / "02_pair_list.csv", index=False)
    cache_all_series(inv, pairs)

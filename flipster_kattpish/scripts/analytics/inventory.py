"""Step 1: data inventory.

For each (exchange, raw_symbol) emit normalized base and tick density.
Output: output/01_inventory.csv with columns:
    coin, exchange, raw_symbol, ticks_24h, ticks_total, first_seen, last_seen
Then build per-coin coverage: which exchanges have the coin (with min density).

Drop coins covered by fewer than 2 exchanges (no pair possible).
"""
import os
import sys
from pathlib import Path

import pandas as pd

sys.path.insert(0, str(Path(__file__).parent))
from data_loader import EXCHANGES, http_query
from symbols import normalize

OUTPUT = Path(__file__).parent / "output"
OUTPUT.mkdir(exist_ok=True)

MIN_TICKS_24H = 200  # ~ 1 every 7 minutes; below this is too sparse to analyze

def per_exchange_inventory(exchange: str) -> pd.DataFrame:
    # 24h-window only to keep query fast on multi-million-row tables
    sql = (
        f"SELECT symbol, count() AS ticks_24h, "
        f"  min(timestamp) AS first_seen_24h, max(timestamp) AS last_seen_24h "
        f"FROM {exchange}_bookticker "
        f"WHERE timestamp > dateadd('h', -24, now()) "
        f"GROUP BY symbol"
    )
    rows = http_query(sql, timeout=120)
    df = pd.DataFrame(rows, columns=["raw_symbol","ticks_24h","first_seen_24h","last_seen_24h"])
    df["exchange"] = exchange
    df["coin"] = df["raw_symbol"].apply(lambda s: normalize(exchange, s))
    return df


def build_inventory() -> pd.DataFrame:
    parts = []
    for ex in EXCHANGES:
        print(f"  scanning {ex}...", end="", flush=True)
        df = per_exchange_inventory(ex)
        kept = df.dropna(subset=["coin"])
        print(f" raw={len(df)} normalized={len(kept)}")
        parts.append(kept)
    inv = pd.concat(parts, ignore_index=True)
    inv = inv[["coin","exchange","raw_symbol","ticks_24h","first_seen_24h","last_seen_24h"]]
    return inv


def coverage(inv: pd.DataFrame) -> pd.DataFrame:
    """For each coin, which exchanges cover it (after density filter)."""
    f = inv[inv["ticks_24h"] >= MIN_TICKS_24H].copy()
    pivot = (
        f.groupby(["coin","exchange"])["ticks_24h"].sum()
         .reset_index()
         .pivot(index="coin", columns="exchange", values="ticks_24h")
         .fillna(0).astype(int)
    )
    pivot["n_exchanges"] = (pivot > 0).sum(axis=1)
    pivot["total_ticks_24h"] = pivot[[c for c in pivot.columns if c not in ("n_exchanges","total_ticks_24h")]].sum(axis=1)
    pivot = pivot.sort_values(["n_exchanges","total_ticks_24h"], ascending=[False,False])
    return pivot


if __name__ == "__main__":
    print("Building inventory...")
    inv = build_inventory()
    inv_path = OUTPUT / "01_inventory.csv"
    inv.to_csv(inv_path, index=False)
    print(f"Wrote {inv_path}: {len(inv)} rows")

    cov = coverage(inv)
    cov_path = OUTPUT / "01_coverage.csv"
    cov.to_csv(cov_path)
    print(f"Wrote {cov_path}: {len(cov)} coins")

    # Summary
    multi = cov[cov["n_exchanges"] >= 2]
    print(f"\nCoins on ≥2 exchanges: {len(multi)}")
    print(f"Coins on ≥3 exchanges: {(cov['n_exchanges']>=3).sum()}")
    print(f"Coins on ≥5 exchanges: {(cov['n_exchanges']>=5).sum()}")
    print(f"Coins on ALL {len(EXCHANGES)} exchanges: {(cov['n_exchanges']==len(EXCHANGES)).sum()}")
    print("\nTop 20 most-covered coins:")
    print(cov.head(20)[["n_exchanges","total_ticks_24h"]].to_string())

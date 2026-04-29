"""Latency log analysis:
  1. Per-symbol mean latency ranking
  2. Latency > 500ms windows
  3. Hour-of-day x symbol heatmap
Run: python analyze_latency.py
"""
from __future__ import annotations
import matplotlib.pyplot as plt
import pandas as pd
import seaborn as sns
from qdb import query


def ranking(days: int = 7) -> pd.DataFrame:
    sql = f"""
        SELECT symbol,
               avg(latency_ms) avg_ms,
               count() n
        FROM latency_log
        WHERE binance_ts > dateadd('d', -{days}, now())
        GROUP BY symbol
        ORDER BY avg_ms DESC
    """
    return query(sql)


def hourly_heatmap(days: int = 7) -> pd.DataFrame:
    sql = f"""
        SELECT symbol,
               hour(binance_ts) hour,
               avg(latency_ms) avg_ms
        FROM latency_log
        WHERE binance_ts > dateadd('d', -{days}, now())
        GROUP BY symbol, hour
    """
    df = query(sql)
    if df.empty:
        return df
    return df.pivot(index="symbol", columns="hour", values="avg_ms")


def high_latency_windows(threshold_ms: float = 500, days: int = 7) -> pd.DataFrame:
    sql = f"""
        SELECT binance_ts, symbol, latency_ms, price_diff_bp, direction
        FROM latency_log
        WHERE binance_ts > dateadd('d', -{days}, now())
          AND latency_ms > {threshold_ms}
        ORDER BY binance_ts
    """
    return query(sql)


def main() -> None:
    rank = ranking()
    print("== Avg latency ranking ==")
    print(rank.to_string(index=False))

    heat = hourly_heatmap()
    if not heat.empty:
        plt.figure(figsize=(12, 6))
        sns.heatmap(heat, cmap="magma", annot=False)
        plt.title("Mean latency_ms — hour of day × symbol")
        plt.tight_layout()
        plt.savefig("latency_heatmap.png", dpi=140)
        print("saved latency_heatmap.png")

    hi = high_latency_windows()
    print(f"== {len(hi)} events with latency > 500ms ==")


if __name__ == "__main__":
    main()

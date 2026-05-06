"""Step 7: assemble RECOMMENDATIONS.md from analytics outputs."""
import sys
from pathlib import Path

import pandas as pd

sys.path.insert(0, str(Path(__file__).parent))
from fees import RAW, fees_bp

OUTPUT = Path(__file__).parent / "output"


def fmt(x, fmt_str=".2f"):
    if pd.isna(x): return "—"
    return format(x, fmt_str)


def build():
    inv = pd.read_csv(OUTPUT / "01_inventory.csv")
    pair_stats = pd.read_csv(OUTPUT / "03_pair_stats.csv")
    scores = pd.read_csv(OUTPUT / "04_pair_scores_taker_taker.csv")
    bt_mid = pd.read_csv(OUTPUT / "05_backtest_taker_taker.csv")
    bt_real = pd.read_csv(OUTPUT / "05_backtest_realistic_taker_taker.csv")

    # Top recommended pairs: realistic backtest, ≥10 trades, ≥60% win, ≥0 total_pnl
    rec = bt_real[(bt_real.n_trades >= 10) &
                   (bt_real.win_rate >= 0.6) &
                   (bt_real.total_pnl_bp > 0)].sort_values("total_pnl_bp", ascending=False)

    lines = []
    lines.append("# Cross-Exchange Mean Revert Opportunity Analysis")
    lines.append("")
    lines.append("**Generated**: 2026-05-04 (24h-window analysis)")
    lines.append("")
    lines.append("## Executive Summary")
    lines.append("")
    lines.append(f"- **Universe analyzed**: {pair_stats['coin'].nunique()} coins, "
                 f"{len(pair_stats)} pairs across 11 exchanges (excl. flipster/pyth/hashkey).")
    lines.append(f"- **Window**: last 24 hours, 1s-resampled mid prices.")
    lines.append(f"- **Mean-revert score** (Hurst<0.5, ADF p<0.05, half-life 3-1800s, edge>0): "
                 f"{(scores.score>0).sum()} pairs survive.")
    lines.append(f"- **Mid-mid backtest** (idealistic): {(bt_mid.total_pnl_bp>0).sum()}/{len(bt_mid)} pairs profitable.")
    lines.append(f"- **Bid-ask backtest** (realistic, half-spread cost charged): "
                 f"{(bt_real.total_pnl_bp>0).sum()}/{len(bt_real)} pairs profitable, "
                 f"{len(rec)} pass quality bar (≥10 trades, ≥60% win rate, +PnL).")
    lines.append("")
    lines.append("## Methodology")
    lines.append("")
    lines.append("1. Pull 24h book ticker data for 11 exchanges from central QuestDB.")
    lines.append("2. Normalize symbols → canonical coin (BTC, ETH, ...).")
    lines.append("3. Resample to 1s last bid/ask/mid, forward-fill ≤10s gaps.")
    lines.append("4. For each pair: compute mid-spread = (mid_a − mid_b) / mid_b × 10000 bp.")
    lines.append("5. Stats: mean, stddev, half-life (OU), Hurst, ADF, mean-crossings/h.")
    lines.append("6. Score = max(0, 2σ − round_trip_fee) × P(|z|>2) × crossings/h × 24 × stationarity_gate.")
    lines.append("7. Paper backtest: rolling 10-min mean/std, enter |z|≥2, exit z=0; 5s min hold, 30s cooldown, 600s max hold.")
    lines.append("8. **Realistic** variant uses ask on entry-buy / bid on entry-sell; bid on exit-sell / ask on exit-buy. The half-spread cost is paid TWICE (entry + exit).")
    lines.append("")
    lines.append("**Effective fees (bp, post-Flipster Banner payback where applicable):**")
    lines.append("")
    lines.append("| exchange | taker | maker | banner payback |")
    lines.append("|---|---:|---:|---:|")
    for ex, (rt, rm, p) in RAW.items():
        f = fees_bp(ex)
        lines.append(f"| {ex} | {f['taker_bp']:.4f} | {f['maker_bp']:.4f} | {p*100:.0f}% |")
    lines.append("")
    lines.append("## Top Recommendations (Realistic Bid-Ask Backtest)")
    lines.append("")
    lines.append("Filter: ≥10 trades, ≥60% win rate, positive PnL. Sorted by total bp PnL.")
    lines.append("")
    lines.append("| # | coin | ex_a | ex_b | trades | win% | total bp | avg bp | p50 bp | hold_s | rt_fee | open_cost | trades/day |")
    lines.append("|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for i, r in enumerate(rec.head(20).itertuples(index=False), 1):
        lines.append(f"| {i} | {r.coin} | {r.ex_a} | {r.ex_b} | {r.n_trades} | "
                     f"{r.win_rate*100:.1f}% | {r.total_pnl_bp:+.0f} | {r.avg_pnl_bp:+.2f} | "
                     f"{r.p50_pnl_bp:+.2f} | {r.avg_hold_sec:.1f} | {r.rt_fee_bp:.2f} | "
                     f"{r.avg_open_cost_bp:+.2f} | {r.trades_per_day:.0f} |")
    lines.append("")
    lines.append("**Reading the table**:")
    lines.append("- `total bp` = sum of net PnL in basis points across all 24h trades on a unit notional.")
    lines.append("  +1000 bp/24h ≈ 10% return per unit position-size, at very high turnover.")
    lines.append("- `open_cost` is the executable spread paid to ENTER. When negative, it means the entry leg ")
    lines.append("  receives a credit (favorable bid-ask alignment). This is where the mean-revert alpha lives.")
    lines.append("- `hold_s` ≈ how long position held; `rt_fee` is round-trip taker fee (bp).")
    lines.append("")
    lines.append("## Per-Coin Best Pair (top 15 coins by recommended pair count)")
    lines.append("")
    if not rec.empty:
        per_coin = rec.sort_values("total_pnl_bp", ascending=False).groupby("coin").head(1).head(15)
        lines.append("| coin | best pair | trades | win% | total bp | avg bp |")
        lines.append("|---|---|---:|---:|---:|---:|")
        for r in per_coin.itertuples(index=False):
            lines.append(f"| {r.coin} | {r.ex_a} × {r.ex_b} | {r.n_trades} | {r.win_rate*100:.1f}% | "
                         f"{r.total_pnl_bp:+.0f} | {r.avg_pnl_bp:+.2f} |")
    lines.append("")
    lines.append("## Critical Caveats")
    lines.append("")
    lines.append("1. **24h window only**. Generalization to other regimes is unverified. Funding regimes,")
    lines.append("   exchange outages, or thin-alt listing cycles could invalidate findings.")
    lines.append("2. **Size unmodeled**. All numbers assume unit notional fills at top-of-book. For thin alts")
    lines.append("   (SKYAI, BSB, TAG, UB) executing $1-10k may already exhaust top-of-book.")
    lines.append("3. **No funding cost** subtracted. Holding time is short (avg 25s) so funding is small,")
    lines.append("   but for low-margin pairs (gate-bitget BTC at +0.5 bp avg) funding could flip sign.")
    lines.append("4. **Mid-spread signal vs bid-ask execution**. Signal triggers on z-score of mid spread. ")
    lines.append("   Real entry pays the bid-ask 'tax' = sum of half-spreads. We charge it explicitly.")
    lines.append("5. **Look-ahead-free at the second level** (we use trailing rolling mean/std), but not at")
    lines.append("   the millisecond level — within a 1s bar, signal and execution happen at the same `last`")
    lines.append("   quote. Real implementation will see the bar end ~1s later, possibly missing the bar's tail.")
    lines.append("6. **Correlated pairs**. SKYAI/BSB/TAG/UB/LAB are listed on overlapping exchanges. Don't")
    lines.append("   compound size — same alt-listing-cycle dynamic drives all of them.")
    lines.append("7. **Major coins (BTC/ETH/SOL) are not in the top 100** by score. Their stddev is ~1bp,")
    lines.append("   so 2σ deviation is only ~2bp, which round-trip taker fees on most pair combos exceeds.")
    lines.append("   The cheapest BTC pair (gate × bitget, rt_fee 2.52bp) has range_p99 = 2.1bp → marginal.")
    lines.append("")
    lines.append("## Files")
    lines.append("")
    lines.append("- `output/01_inventory.csv` — per (exchange, raw_symbol) tick density.")
    lines.append("- `output/01_coverage.csv` — coin × exchange coverage matrix.")
    lines.append("- `output/02_pair_list.csv` — pair selection.")
    lines.append("- `output/03_pair_stats.csv` — full pair statistics.")
    lines.append("- `output/04_pair_scores_taker_taker.csv` — scored pairs.")
    lines.append("- `output/05_backtest_taker_taker.csv` — mid-mid backtest.")
    lines.append("- `output/05_backtest_realistic_taker_taker.csv` — bid-ask backtest (use this).")
    lines.append("- `output/plots/{coin}_{ex_a}_{ex_b}_spread.png` — top pair spread plots.")
    lines.append("- `output/plots/{coin}_corr_heatmap.png` — correlation heatmaps.")
    lines.append("- `output/series_cache/{exchange}_{coin}.parquet` — raw 1s-resampled bid/ask/mid cache.")
    lines.append("")
    return "\n".join(lines)


if __name__ == "__main__":
    md = build()
    p = OUTPUT / "RECOMMENDATIONS.md"
    p.write_text(md)
    print(f"Wrote {p}: {len(md):,} chars")
    print()
    print(md[:2000])

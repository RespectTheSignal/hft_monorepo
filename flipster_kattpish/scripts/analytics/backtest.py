"""Option B: paper backtest top candidate pairs.

Strategy:
  1. Compute rolling mean (window=ROLL_SEC) and rolling std of spread_bp.
  2. Enter LONG spread (= long ex_a, short ex_b) when z = (spread - rolling_mean) / rolling_std < -ENTRY_K
     Enter SHORT spread when z > +ENTRY_K
  3. Exit when |z| < EXIT_K (default 0.0 = mean reversion) OR after MAX_HOLD_SEC.
  4. Skip cool-down period after exit (avoid over-trading on autocorrelated z).
  5. Charge round-trip taker fee on each entry+exit (in bp).

Optional slippage model: 50% taker fill assumption ⇒ avg of taker_taker and maker_taker fees.

Outputs: per-pair PnL, win rate, num trades, avg hold, max DD.
"""
import os
import sys
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import pandas as pd

sys.path.insert(0, str(Path(__file__).parent))
from fees import round_trip_bp
from pair_stats import load_aligned_spread

OUTPUT = Path(__file__).parent / "output"

ROLL_SEC = 600       # 10 min rolling mean/std
ENTRY_K = 2.0
EXIT_K = 0.0
MAX_HOLD_SEC = 600   # exit if not converged in 10 min
COOLDOWN_SEC = 30
MIN_HOLD_SEC = 5     # avoid microstructure flips


@dataclass
class Trade:
    entry_ts: pd.Timestamp
    exit_ts: pd.Timestamp
    side: int            # +1 long spread, -1 short spread
    entry_bp: float
    exit_bp: float
    fee_bp: float
    pnl_bp: float
    hold_sec: int
    reason: str


def simulate_pair(coin: str, ex_a: str, ex_b: str,
                  fee_mode: str = "taker_taker",
                  entry_k: float = ENTRY_K, exit_k: float = EXIT_K,
                  roll_sec: int = ROLL_SEC) -> dict | None:
    df = load_aligned_spread(coin, ex_a, ex_b)
    if df is None:
        return None
    s = df["spread_bp_w"]
    rm = s.rolling(f"{roll_sec}s").mean()
    rs = s.rolling(f"{roll_sec}s").std()
    valid = rs > 0
    z = (s - rm) / rs.where(rs > 0)
    rt_fee = round_trip_bp(ex_a, ex_b, mode=fee_mode)

    trades: list[Trade] = []
    pos_side = 0
    entry_idx = -1
    entry_bp = 0.0
    cooldown_until_idx = -1

    # Convert index to integer seconds since epoch for fast arithmetic.
    # Force ns precision because parquet may store datetime64[us] etc.
    times_ns = z.index.values.astype("datetime64[ns]").astype("int64")
    times_sec = (times_ns // 1_000_000_000).astype("int64")
    z_arr = z.to_numpy()
    s_arr = s.to_numpy()
    valid_arr = valid.to_numpy()

    for i in range(len(z_arr)):
        if not valid_arr[i]:
            continue
        if cooldown_until_idx > 0 and times_sec[i] < cooldown_until_idx:
            continue
        zi = z_arr[i]
        if np.isnan(zi):
            continue
        if pos_side == 0:
            if zi <= -entry_k:
                pos_side = +1; entry_idx = i; entry_bp = s_arr[i]
            elif zi >= entry_k:
                pos_side = -1; entry_idx = i; entry_bp = s_arr[i]
        else:
            hold_sec = int(times_sec[i] - times_sec[entry_idx])
            if hold_sec < MIN_HOLD_SEC:
                continue
            exit_now = False; reason = ""
            if pos_side == +1 and zi >= -exit_k:
                exit_now = True; reason = "revert"
            elif pos_side == -1 and zi <= exit_k:
                exit_now = True; reason = "revert"
            elif hold_sec >= MAX_HOLD_SEC:
                exit_now = True; reason = "timeout"
            if exit_now:
                exit_bp = s_arr[i]
                # Long spread (pos=+1): entered when spread cheap, profits if spread rises
                pnl_bp = pos_side * (exit_bp - entry_bp) - rt_fee
                trades.append(Trade(
                    entry_ts=pd.Timestamp(times_ns[entry_idx]),
                    exit_ts=pd.Timestamp(times_ns[i]),
                    side=pos_side, entry_bp=entry_bp, exit_bp=exit_bp,
                    fee_bp=rt_fee, pnl_bp=pnl_bp,
                    hold_sec=hold_sec, reason=reason,
                ))
                pos_side = 0
                cooldown_until_idx = times_sec[i] + COOLDOWN_SEC

    if not trades:
        return {
            "coin": coin, "ex_a": ex_a, "ex_b": ex_b, "fee_mode": fee_mode,
            "n_trades": 0, "win_rate": 0, "total_pnl_bp": 0,
            "avg_pnl_bp": 0, "p25_pnl_bp": 0, "p50_pnl_bp": 0, "p75_pnl_bp": 0,
            "avg_hold_sec": 0, "max_dd_bp": 0, "rt_fee_bp": rt_fee,
            "trades_per_day": 0, "timeout_rate": 0,
        }
    tdf = pd.DataFrame([t.__dict__ for t in trades])
    cum = tdf["pnl_bp"].cumsum()
    max_dd = float((cum.cummax() - cum).max())
    days = (df.index[-1] - df.index[0]).total_seconds() / 86400
    return {
        "coin": coin, "ex_a": ex_a, "ex_b": ex_b, "fee_mode": fee_mode,
        "n_trades": len(tdf),
        "win_rate": float((tdf.pnl_bp > 0).mean()),
        "total_pnl_bp": float(tdf.pnl_bp.sum()),
        "avg_pnl_bp": float(tdf.pnl_bp.mean()),
        "p25_pnl_bp": float(tdf.pnl_bp.quantile(0.25)),
        "p50_pnl_bp": float(tdf.pnl_bp.quantile(0.50)),
        "p75_pnl_bp": float(tdf.pnl_bp.quantile(0.75)),
        "avg_hold_sec": float(tdf.hold_sec.mean()),
        "max_dd_bp": max_dd,
        "rt_fee_bp": rt_fee,
        "trades_per_day": len(tdf) / max(days, 1e-9),
        "timeout_rate": float((tdf.reason == "timeout").mean()),
    }


def run_top_n(n: int = 50, fee_mode: str = "taker_taker") -> pd.DataFrame:
    scores = pd.read_csv(OUTPUT / "04_pair_scores_taker_taker.csv")
    scores = scores[scores.score > 0].sort_values("score", ascending=False).head(n)
    rows = []
    for i, r in enumerate(scores.itertuples(index=False)):
        try:
            res = simulate_pair(r.coin, r.ex_a, r.ex_b, fee_mode=fee_mode)
        except Exception as e:
            print(f"  ERR {r.coin} {r.ex_a}-{r.ex_b}: {e}")
            res = None
        if res:
            rows.append(res)
        if (i+1) % 10 == 0:
            print(f"  [{i+1}/{n}] last: {r.coin} {r.ex_a}-{r.ex_b}")
    return pd.DataFrame(rows)


if __name__ == "__main__":
    n = int(os.getenv("BACKTEST_N", "50"))
    fee_mode = os.getenv("FEE_MODE", "taker_taker")
    res = run_top_n(n=n, fee_mode=fee_mode)
    p = OUTPUT / f"05_backtest_{fee_mode}.csv"
    res.to_csv(p, index=False)
    res = res.sort_values("total_pnl_bp", ascending=False)
    print(f"\nWrote {p}")
    print(f"\n=== Top 25 by total_pnl_bp ({fee_mode}) ===")
    cols = ["coin","ex_a","ex_b","n_trades","win_rate","total_pnl_bp","avg_pnl_bp",
            "p50_pnl_bp","avg_hold_sec","timeout_rate","rt_fee_bp","trades_per_day"]
    print(res.head(25)[cols].to_string(index=False))
    print(f"\nProfitable pairs: {(res.total_pnl_bp > 0).sum()} / {len(res)}")
    print(f"Pairs with ≥10 trades and win_rate ≥ 60%: {((res.n_trades>=10)&(res.win_rate>=0.6)).sum()}")

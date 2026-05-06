"""Realistic paper backtest using bid/ask quotes (not mid).

Differences from backtest.py:
- Entry: pay ask on buy leg, receive bid on sell leg → executable spread incl half-spread cost
- Exit: receive bid on close-buy-leg, pay ask on close-sell-leg
- Signal: still uses mid-spread z-score (no look-ahead at orderbook)

Spread definition (long spread = long ex_a, short ex_b):
    open_cost_bp  = (ask_a - bid_b) / bid_b * 10000      [paid to OPEN long]
    close_rev_bp  = (bid_a - ask_b) / ask_b * 10000      [received on CLOSE long]
    pnl_long      = close_rev_bp - open_cost_bp - fee_bp

For SHORT spread (short ex_a, long ex_b):
    open_cost_bp  = (ask_b - bid_a) / bid_a * 10000      [executable for opening short_spread]
    close_rev_bp  = (bid_b - ask_a) / ask_a * 10000
    pnl_short     = close_rev_bp - open_cost_bp - fee_bp
"""
import os
import sys
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import pandas as pd

sys.path.insert(0, str(Path(__file__).parent))
from fees import round_trip_bp
from spread_calc import _series_path

OUTPUT = Path(__file__).parent / "output"

ROLL_SEC = 600
ENTRY_K = 2.0
EXIT_K = 0.0
MAX_HOLD_SEC = 600
COOLDOWN_SEC = 30
MIN_HOLD_SEC = 5
MIN_OBS = 1800


def load_pair_quotes(coin: str, ex_a: str, ex_b: str) -> pd.DataFrame | None:
    pa = _series_path(ex_a, coin)
    pb = _series_path(ex_b, coin)
    if not pa.exists() or not pb.exists():
        return None
    a = pd.read_parquet(pa)[["bid_price","ask_price","mid"]].rename(columns=lambda c: f"{c}_a")
    b = pd.read_parquet(pb)[["bid_price","ask_price","mid"]].rename(columns=lambda c: f"{c}_b")
    df = pd.concat([a, b], axis=1).dropna()
    if len(df) < MIN_OBS:
        return None
    df["mid_spread_bp"] = (df["mid_a"] - df["mid_b"]) / df["mid_b"] * 10000.0
    # winsorize to filter pathological quotes
    lo, hi = df["mid_spread_bp"].quantile([0.001, 0.999])
    df["mid_spread_bp_w"] = df["mid_spread_bp"].clip(lo, hi)
    return df


def simulate_pair_realistic(coin: str, ex_a: str, ex_b: str,
                            fee_mode: str = "taker_taker",
                            entry_k: float = ENTRY_K,
                            exit_k: float = EXIT_K,
                            roll_sec: int = ROLL_SEC) -> dict | None:
    df = load_pair_quotes(coin, ex_a, ex_b)
    if df is None:
        return None
    s = df["mid_spread_bp_w"]
    rm = s.rolling(f"{roll_sec}s").mean()
    rs = s.rolling(f"{roll_sec}s").std()
    z = (s - rm) / rs.where(rs > 0)
    valid_arr = (rs > 0).to_numpy()

    rt_fee = round_trip_bp(ex_a, ex_b, mode=fee_mode)
    times_ns = z.index.values.astype("datetime64[ns]").astype("int64")
    times_sec = (times_ns // 1_000_000_000).astype("int64")
    z_arr = z.to_numpy()
    bid_a = df["bid_price_a"].to_numpy()
    ask_a = df["ask_price_a"].to_numpy()
    bid_b = df["bid_price_b"].to_numpy()
    ask_b = df["ask_price_b"].to_numpy()

    pos_side = 0
    entry_idx = -1
    open_cost_bp = 0.0
    cooldown_until_idx = -1
    trades = []

    for i in range(len(z_arr)):
        if not valid_arr[i]: continue
        if cooldown_until_idx > 0 and times_sec[i] < cooldown_until_idx: continue
        zi = z_arr[i]
        if np.isnan(zi): continue
        if pos_side == 0:
            if zi <= -entry_k:
                # Long spread: buy ex_a@ask, sell ex_b@bid
                if bid_b[i] <= 0: continue
                open_cost_bp = (ask_a[i] - bid_b[i]) / bid_b[i] * 10000
                pos_side = +1; entry_idx = i
            elif zi >= entry_k:
                # Short spread: buy ex_b@ask, sell ex_a@bid
                if bid_a[i] <= 0: continue
                open_cost_bp = (ask_b[i] - bid_a[i]) / bid_a[i] * 10000
                pos_side = -1; entry_idx = i
        else:
            hold_sec = int(times_sec[i] - times_sec[entry_idx])
            if hold_sec < MIN_HOLD_SEC: continue
            exit_now = False; reason = ""
            if pos_side == +1 and zi >= -exit_k: exit_now = True; reason = "revert"
            elif pos_side == -1 and zi <= exit_k: exit_now = True; reason = "revert"
            elif hold_sec >= MAX_HOLD_SEC: exit_now = True; reason = "timeout"
            if exit_now:
                if pos_side == +1:
                    if ask_b[i] <= 0: pos_side=0; continue
                    close_rev_bp = (bid_a[i] - ask_b[i]) / ask_b[i] * 10000
                else:
                    if ask_a[i] <= 0: pos_side=0; continue
                    close_rev_bp = (bid_b[i] - ask_a[i]) / ask_a[i] * 10000
                pnl_bp = close_rev_bp - open_cost_bp - rt_fee  # NB: open_cost is the spread paid, sign already encoded
                # Wait — for long spread the position profits when (mid_a - mid_b) RISES.
                # open_cost_bp = (ask_a - bid_b)/bid_b      > 0 typically
                # close_rev_bp = (bid_a - ask_b)/ask_b      < open_cost_bp typically
                # We BUY at open_cost_bp, SELL at close_rev_bp → pnl = close_rev - open_cost - fee.
                # But when long spread, we expect (mid_a - mid_b) to RISE, so close_rev_bp > open_cost_bp.
                # That means the sign convention above is correct.
                trades.append({
                    "entry_ts": pd.Timestamp(times_ns[entry_idx]),
                    "exit_ts": pd.Timestamp(times_ns[i]),
                    "side": pos_side,
                    "open_cost_bp": open_cost_bp,
                    "close_rev_bp": close_rev_bp,
                    "fee_bp": rt_fee,
                    "pnl_bp": pnl_bp,
                    "hold_sec": hold_sec,
                    "reason": reason,
                })
                pos_side = 0
                cooldown_until_idx = times_sec[i] + COOLDOWN_SEC

    if not trades:
        return {"coin": coin, "ex_a": ex_a, "ex_b": ex_b, "fee_mode": fee_mode,
                "n_trades": 0, "win_rate": 0, "total_pnl_bp": 0,
                "avg_pnl_bp": 0, "p50_pnl_bp": 0, "avg_hold_sec": 0,
                "rt_fee_bp": rt_fee, "trades_per_day": 0,
                "avg_open_cost_bp": 0, "avg_close_rev_bp": 0}
    tdf = pd.DataFrame(trades)
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
        "max_dd_bp": float((tdf.pnl_bp.cumsum().cummax() - tdf.pnl_bp.cumsum()).max()),
        "rt_fee_bp": rt_fee,
        "trades_per_day": len(tdf) / max(days, 1e-9),
        "timeout_rate": float((tdf.reason == "timeout").mean()),
        "avg_open_cost_bp": float(tdf.open_cost_bp.mean()),
        "avg_close_rev_bp": float(tdf.close_rev_bp.mean()),
    }


def run_top_n(n: int = 50, fee_mode: str = "taker_taker") -> pd.DataFrame:
    scores = pd.read_csv(OUTPUT / "04_pair_scores_taker_taker.csv")
    scores = scores[scores.score > 0].sort_values("score", ascending=False).head(n)
    rows = []
    for i, r in enumerate(scores.itertuples(index=False)):
        try:
            res = simulate_pair_realistic(r.coin, r.ex_a, r.ex_b, fee_mode=fee_mode)
        except Exception as e:
            print(f"  ERR {r.coin} {r.ex_a}-{r.ex_b}: {e}")
            res = None
        if res:
            rows.append(res)
        if (i+1) % 10 == 0:
            print(f"  [{i+1}/{n}] last: {r.coin} {r.ex_a}-{r.ex_b}")
    return pd.DataFrame(rows)


if __name__ == "__main__":
    n = int(os.getenv("BACKTEST_N", "100"))
    fee_mode = os.getenv("FEE_MODE", "taker_taker")
    res = run_top_n(n=n, fee_mode=fee_mode)
    p = OUTPUT / f"05_backtest_realistic_{fee_mode}.csv"
    res.to_csv(p, index=False)
    res = res.sort_values("total_pnl_bp", ascending=False)
    print(f"\nWrote {p}")
    print(f"\n=== Top 25 by total_pnl_bp ({fee_mode}, REALISTIC bid/ask) ===")
    cols = ["coin","ex_a","ex_b","n_trades","win_rate","total_pnl_bp","avg_pnl_bp","p50_pnl_bp",
            "avg_hold_sec","timeout_rate","rt_fee_bp","avg_open_cost_bp","avg_close_rev_bp","trades_per_day"]
    print(res.head(25)[cols].to_string(index=False))
    print(f"\nProfitable pairs: {(res.total_pnl_bp > 0).sum()} / {len(res)}")
    print(f"Pairs with ≥10 trades and win_rate ≥ 60%: {((res.n_trades>=10)&(res.win_rate>=0.6)).sum()}")

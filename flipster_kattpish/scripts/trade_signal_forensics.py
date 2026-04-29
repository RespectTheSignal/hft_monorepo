#!/usr/bin/env python3
"""For each losing trade, reconstruct the Binance price view at entry and
surface WHY the filters let it pass (and why the move went against us).

Queries central QuestDB `binance_bookticker` (ms resolution) for:
  ±60s around entry (high-res price path)
  ±20s around exit (what happened during hold)

Computes at entry_ms:
  - 1s/5s/10s/20s returns (matches binance_momentum_agree)
  - 1s realized range (matches binance_vol_ok)
  - 400ms monotonicity (matches binance_monotonic)
  - Post-entry velocity (next 500ms, 2s, 5s — did it reverse immediately?)

Outputs: /tmp/signal_forensics.html (interactive plotly, one card per losing trade)
"""
from __future__ import annotations
import argparse, json, sys, time, urllib.request, urllib.parse
from datetime import datetime, timezone, timedelta
from pathlib import Path

import plotly.graph_objects as go
from plotly.subplots import make_subplots
import pandas as pd

QDB_HTTP = "http://211.181.122.102:9000/exec"
TF_MS = [1000, 5000, 10000, 20000]
MONO_WINDOW_MS = 400
VOL_WINDOW_MS = 1000


def to_binance_sym(fsym: str) -> str:
    return fsym.replace("USDT.PERP", "_USDT")


def _fetch_qdb(sql: str):
    url = QDB_HTTP + "?" + urllib.parse.urlencode({"query": sql})
    r = json.loads(urllib.request.urlopen(url, timeout=15).read())
    cols = [c["name"] for c in r.get("columns", [])]
    return cols, r.get("dataset", [])


def fetch_binance(bsym: str, t0_ms: int, t1_ms: int) -> pd.DataFrame:
    # QuestDB timestamps in ISO UTC
    t0_iso = datetime.fromtimestamp(t0_ms / 1000, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
    t1_iso = datetime.fromtimestamp(t1_ms / 1000, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")
    sql = (f"SELECT timestamp, bid_price, ask_price FROM binance_bookticker "
           f"WHERE symbol='{bsym}' AND timestamp >= '{t0_iso}' AND timestamp <= '{t1_iso}' "
           f"ORDER BY timestamp")
    cols, rows = _fetch_qdb(sql)
    if not rows:
        return pd.DataFrame(columns=["ts_ms", "bid", "ask", "mid"])
    df = pd.DataFrame(rows, columns=cols)
    df["ts_ms"] = pd.to_datetime(df["timestamp"], utc=True).astype("int64") // 1_000_000
    df["bid"] = df["bid_price"].astype(float)
    df["ask"] = df["ask_price"].astype(float)
    df["mid"] = (df["bid"] + df["ask"]) / 2
    return df[["ts_ms", "bid", "ask", "mid"]].reset_index(drop=True)


def reconstruct_filters(df: pd.DataFrame, entry_ms: int, side: str) -> dict:
    """Replay what binance_momentum_agree/binance_vol_ok/binance_monotonic would see
    at entry_ms, using the Binance history up to that moment."""
    past = df[df.ts_ms <= entry_ms]
    if past.empty:
        return {"note": "no Binance data at entry"}
    cur_mid = past.iloc[-1]["mid"]

    # 1/5/10/20s returns
    returns = {}
    agree = 0
    for w in TF_MS:
        target = entry_ms - w
        # nearest tick at or before target
        candidates = past[past.ts_ms <= target]
        if candidates.empty:
            returns[f"{w}ms"] = None
            continue
        past_mid = candidates.iloc[-1]["mid"]
        if past_mid <= 0:
            returns[f"{w}ms"] = None
            continue
        bp = (cur_mid - past_mid) / past_mid * 1e4
        returns[f"{w}ms"] = round(bp, 1)
        if (side == "buy" and bp > 0) or (side == "sell" and bp < 0):
            agree += 1

    # 1s range
    window = past[past.ts_ms >= entry_ms - VOL_WINDOW_MS]
    if len(window) >= 2:
        lo, hi = window["mid"].min(), window["mid"].max()
        range_bp = (hi - lo) / lo * 1e4 if lo > 0 else 0
    else:
        range_bp = 0

    # 400ms monotonicity
    mono_win = past[past.ts_ms >= entry_ms - MONO_WINDOW_MS]
    if len(mono_win) >= 4:
        start, end = mono_win.iloc[0]["mid"], mono_win.iloc[-1]["mid"]
        move_bp = (end - start) / start * 1e4
        deltas = mono_win["mid"].diff().dropna()
        if side == "buy":
            wrong = (deltas < 0).sum()
        else:
            wrong = (deltas > 0).sum()
        rev_ratio = wrong / len(deltas) if len(deltas) else 0
        mono_str = f"move={move_bp:+.1f}bp rev={rev_ratio:.0%} n={len(mono_win)}"
    else:
        mono_str = f"insufficient samples ({len(mono_win)})"

    # Post-entry behaviour: velocity in next 500ms / 2s / 5s
    post = {}
    for fwd in (500, 2000, 5000):
        future = df[(df.ts_ms > entry_ms) & (df.ts_ms <= entry_ms + fwd)]
        if future.empty:
            post[f"+{fwd}ms"] = None
            continue
        end_mid = future.iloc[-1]["mid"]
        bp = (end_mid - cur_mid) / cur_mid * 1e4
        post[f"+{fwd}ms"] = round(bp, 1)

    return {
        "entry_mid": round(cur_mid, 6),
        "returns_bp": returns,
        "momentum_agree": f"{agree}/4",
        "vol_range_1s_bp": round(range_bp, 1),
        "monotonicity": mono_str,
        "post_entry_bp": post,
    }


def plot_trade(df: pd.DataFrame, trade: dict, forensics: dict) -> go.Figure:
    fig = make_subplots(rows=1, cols=1)
    if not df.empty:
        xs = pd.to_datetime(df.ts_ms, unit="ms").dt.tz_localize("UTC").dt.tz_convert("Asia/Seoul")
        fig.add_trace(go.Scatter(x=xs, y=df["mid"], mode="lines",
                                 line=dict(color="#444", width=1),
                                 name="Binance mid"))
        fig.add_trace(go.Scatter(x=xs, y=df["bid"], mode="lines",
                                 line=dict(color="#2ca02c", width=0.7, dash="dot"),
                                 name="bid", opacity=0.5))
        fig.add_trace(go.Scatter(x=xs, y=df["ask"], mode="lines",
                                 line=dict(color="#d62728", width=0.7, dash="dot"),
                                 name="ask", opacity=0.5))

    entry_dt = datetime.fromtimestamp(trade["entry_ms"] / 1000, tz=timezone.utc).astimezone()
    exit_dt = datetime.fromtimestamp(trade["exit_ms"] / 1000, tz=timezone.utc).astimezone()

    fig.add_trace(go.Scatter(
        x=[entry_dt], y=[trade["entry_price"]],
        mode="markers+text",
        marker=dict(symbol="triangle-up" if trade["side"] == "buy" else "triangle-down",
                    size=18, color="#0066cc",
                    line=dict(width=1.5, color="white")),
        text=[f"ENTRY {trade['side']}"],
        textposition="top center",
        name="entry",
    ))
    fig.add_trace(go.Scatter(
        x=[exit_dt], y=[trade["exit_price"]],
        mode="markers+text",
        marker=dict(symbol="x", size=18, color="#cc2222",
                    line=dict(width=1.5, color="white")),
        text=[f"EXIT {trade['move_bp']:+.0f}bp"],
        textposition="top center",
        name="exit",
    ))
    # annotate forensics
    if "returns_bp" in forensics:
        r = forensics["returns_bp"]
        post = forensics["post_entry_bp"]
        ann = (
            f"<b>At entry</b> ({trade['sig_ts']}): "
            f"1s={r.get('1000ms','?')}bp 5s={r.get('5000ms','?')}bp "
            f"10s={r.get('10000ms','?')}bp 20s={r.get('20000ms','?')}bp "
            f"| agree={forensics['momentum_agree']}<br>"
            f"vol(1s)={forensics['vol_range_1s_bp']}bp  mono: {forensics['monotonicity']}<br>"
            f"<b>Post-entry</b> Binance move: +500ms={post.get('+500ms','?')}bp "
            f"+2s={post.get('+2000ms','?')}bp +5s={post.get('+5000ms','?')}bp"
        )
    else:
        ann = forensics.get("note", "")

    subtitle = (f"{trade['sym']} {trade['side']} ${trade['pnl']:+.2f}  "
                f"move={trade['move_bp']:+.0f}bp hold={trade['hold_ms']/1000:.0f}s  "
                f"reason={trade['close_reason']}  sig_diff={trade.get('sig_diff_bp','?')}bp")
    fig.update_layout(
        title=dict(text=subtitle, font=dict(size=12)),
        height=380, template="plotly_white",
        margin=dict(l=50, r=20, t=50, b=100),
        xaxis=dict(title=""), yaxis=dict(title="Price"),
        legend=dict(orientation="h", yanchor="bottom", y=1.02, x=0.01, font=dict(size=9)),
        annotations=[dict(
            text=ann, xref="paper", yref="paper", x=0.01, y=-0.22,
            showarrow=False, align="left", font=dict(size=10, family="monospace"),
        )],
    )
    return fig


def sig_ts_to_epoch_ms(s: str, ref_epoch_ms: int) -> int | None:
    if not s: return None
    try:
        hh, mm, ss = map(int, s.split(":"))
    except Exception:
        return None
    ref = datetime.fromtimestamp(ref_epoch_ms / 1000)
    dt = ref.replace(hour=hh, minute=mm, second=ss, microsecond=0)
    return int(dt.timestamp() * 1000)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--trades", type=Path, default="/tmp/trades.json")
    ap.add_argument("--out", type=Path, default="/tmp/signal_forensics.html")
    ap.add_argument("--top", type=int, default=12)
    ap.add_argument("--pre-sec", type=float, default=60)
    ap.add_argument("--post-sec", type=float, default=30)
    ap.add_argument("--log-mtime", action="store_true")
    args = ap.parse_args()

    trades = json.loads(args.trades.read_text())
    if args.log_mtime:
        log = Path("/home/gate1/projects/quant/flipster_kattpish/logs/latency_pick.log")
        ref_ms = int(log.stat().st_mtime * 1000)
    else:
        ref_ms = int(time.time() * 1000)

    # enrich
    for t in trades:
        t["entry_ms"] = sig_ts_to_epoch_ms(t.get("sig_ts"), ref_ms) or (ref_ms - t["hold_ms"] - 60_000)
        t["exit_ms"] = t["entry_ms"] + t["hold_ms"]

    losers = sorted([t for t in trades if t["pnl"] < 0 and t.get("sig_ts")],
                    key=lambda t: t["pnl"])[:args.top]

    cards = []
    for t in losers:
        bsym = to_binance_sym(t["sym"])
        t0 = t["entry_ms"] - int(args.pre_sec * 1000)
        t1 = t["exit_ms"] + int(args.post_sec * 1000)
        df = fetch_binance(bsym, t0, t1)
        forensics = reconstruct_filters(df, t["entry_ms"], t["side"])
        fig = plot_trade(df, t, forensics)
        cards.append('<div class="card">' + fig.to_html(include_plotlyjs=False, full_html=False) + "</div>")
        print(f"  {t['sym']:22} entry={datetime.fromtimestamp(t['entry_ms']/1000).strftime('%H:%M:%S')} "
              f"binance_pts={len(df)} post500ms={forensics.get('post_entry_bp',{}).get('+500ms')}")

    html = f"""<!DOCTYPE html><html><head><meta charset="utf-8">
<title>Signal forensics — losing trades</title>
<script src="https://cdn.plot.ly/plotly-2.35.2.min.js"></script>
<style>
  body {{ font-family: -apple-system, sans-serif; margin: 20px; background: #fafafa; }}
  h1 {{ color: #222; }}
  .card {{ background: #fff; border-radius: 8px; padding: 10px;
          margin-bottom: 18px; box-shadow: 0 1px 3px rgba(0,0,0,.1); }}
  .intro {{ color: #444; font-size: 13px; line-height: 1.5; max-width: 860px;
           background: #fff; padding: 14px 18px; border-radius: 6px; }}
</style></head><body>
<h1>Signal Forensics — why did noise get through?</h1>
<div class="intro">
For each losing trade, Binance bookticker (ms resolution) is pulled around the entry time.
<b>At entry</b>: reconstructed 1s/5s/10s/20s returns (= momentum filter view) + 1s range (vol gate) + 400ms monotonicity.
<b>Post-entry</b>: Binance's actual move in the next 500ms / 2s / 5s — if this reverses
sharply against our side, the signal was a false lead / top/bottom tick.
<br><br>
Generated {datetime.now().strftime('%Y-%m-%d %H:%M:%S')} · {len(losers)} losing trades
</div>
{''.join(cards)}
</body></html>"""
    args.out.write_text(html)
    print(f"\n→ wrote {args.out}")


if __name__ == "__main__":
    main()

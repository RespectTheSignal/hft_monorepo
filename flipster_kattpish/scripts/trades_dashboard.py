#!/usr/bin/env python3
"""Interactive trade dashboard — single HTML file with 4 sections:

  1. Summary stats (win rate, net $, avg win/loss)
  2. Cumulative PnL timeline
  3. All-trades table (sortable, filterable — use browser Ctrl+F or sort headers)
  4. Per-losing-trade price charts with Binance 1m klines (entry/exit markers)

Auto-refresh via --watch or re-run anytime. Output: /tmp/trades_dashboard.html
"""
from __future__ import annotations
import argparse, json, sys, time, urllib.request, urllib.parse
from datetime import datetime
from pathlib import Path

import plotly.graph_objects as go
from plotly.subplots import make_subplots
import pandas as pd

BINANCE_KLINES = "https://fapi.binance.com/fapi/v1/klines"


def to_binance_sym(fsym: str) -> str:
    return fsym.replace("USDT.PERP", "USDT")


def sig_ts_to_epoch_ms(s: str, ref_epoch_ms: int) -> int | None:
    if not s: return None
    try:
        hh, mm, ss = map(int, s.split(":"))
    except Exception:
        return None
    ref = datetime.fromtimestamp(ref_epoch_ms / 1000)  # naive local
    dt = ref.replace(hour=hh, minute=mm, second=ss, microsecond=0)
    return int(dt.timestamp() * 1000)


def fetch_klines(bsym: str, start_ms: int, end_ms: int):
    out = []
    cur = start_ms
    while cur < end_ms:
        url = BINANCE_KLINES + "?" + urllib.parse.urlencode({
            "symbol": bsym, "interval": "1m",
            "startTime": cur, "endTime": end_ms, "limit": 1500,
        })
        try:
            data = json.loads(urllib.request.urlopen(url, timeout=10).read())
        except Exception as e:
            return out
        if not data:
            break
        out.extend(data)
        if len(data) < 1500:
            break
        cur = data[-1][0] + 1
        time.sleep(0.05)
    return out


def bucketize_reason(r: str) -> str:
    if r.startswith("tp"): return "tp"
    if r.startswith("stop"): return "stop"
    if r.startswith("max_hold"): return "max_hold"
    return r.split("(")[0]


def build_dashboard(trades: list, ref_ms: int, out_path: Path,
                    top_n: int = 12, window_min: float = 8):
    if not trades:
        out_path.write_text("<html><body>No trades yet</body></html>")
        print(f"→ empty dashboard: {out_path}")
        return

    # --- Enrich trades ---
    for t in trades:
        t["entry_ms"] = sig_ts_to_epoch_ms(t.get("sig_ts"), ref_ms) or (ref_ms - t["hold_ms"] - 60_000)
        t["exit_ms"] = t["entry_ms"] + t["hold_ms"]
        t["reason_bucket"] = bucketize_reason(t["close_reason"])

    df = pd.DataFrame(trades).sort_values("entry_ms")
    df["entry_dt"] = pd.to_datetime(df["entry_ms"], unit="ms", utc=True).dt.tz_convert("Asia/Seoul")
    df["cum"] = df["pnl"].cumsum()

    # --- Summary stats ---
    n = len(df)
    wins = df[df.pnl > 0]
    losses = df[df.pnl < 0]
    gross_win = wins.pnl.sum()
    gross_loss = losses.pnl.sum()
    net = gross_win + gross_loss
    win_rate = len(wins) / n * 100 if n else 0
    avg_win_bp = wins.move_bp.mean() if len(wins) else 0
    avg_loss_bp = losses.move_bp.mean() if len(losses) else 0

    stats_html = f"""
    <div class="stats">
      <div class="stat"><div class="v">{n}</div><div class="l">Trades</div></div>
      <div class="stat"><div class="v">{win_rate:.1f}%</div><div class="l">Win rate</div></div>
      <div class="stat {'pos' if net >= 0 else 'neg'}"><div class="v">${net:+.2f}</div><div class="l">Net</div></div>
      <div class="stat pos"><div class="v">${gross_win:+.2f}</div><div class="l">Gross win</div></div>
      <div class="stat neg"><div class="v">${gross_loss:+.2f}</div><div class="l">Gross loss</div></div>
      <div class="stat"><div class="v">{avg_win_bp:+.0f}bp</div><div class="l">Avg win</div></div>
      <div class="stat"><div class="v">{avg_loss_bp:+.0f}bp</div><div class="l">Avg loss</div></div>
    </div>
    """

    # --- Cumulative PnL line + trade markers ---
    fig_cum = go.Figure()
    fig_cum.add_trace(go.Scatter(
        x=df.entry_dt, y=df.cum, mode="lines+markers",
        name="Cum PnL",
        line=dict(color="#2c7fb8", width=2),
        marker=dict(
            size=8,
            color=df.pnl,
            colorscale=[[0, "#d62728"], [0.5, "#aaaaaa"], [1, "#2ca02c"]],
            cmin=-max(abs(df.pnl.min()), 0.1), cmax=max(abs(df.pnl.max()), 0.1),
            showscale=False,
        ),
        hovertemplate="%{x|%H:%M:%S} %{customdata[0]} %{customdata[1]}<br>"
                      "pnl=$%{customdata[2]:+.3f} move=%{customdata[3]:+.1f}bp<br>"
                      "reason=%{customdata[4]}<extra></extra>",
        customdata=df[["sym", "side", "pnl", "move_bp", "close_reason"]].values,
    ))
    fig_cum.update_layout(
        title=f"Cumulative PnL — {n} trades  (net ${net:+.2f})",
        xaxis_title="", yaxis_title="Cum PnL ($)",
        height=360, margin=dict(l=50, r=20, t=50, b=40),
        template="plotly_white",
    )

    # --- Reason breakdown bar ---
    by_reason = df.groupby("reason_bucket").agg(n=("pnl","count"), total=("pnl","sum"),
                                                 avg_bp=("move_bp","mean")).reset_index()
    fig_reason = go.Figure(go.Bar(
        x=by_reason.reason_bucket, y=by_reason.total,
        marker_color=["#2ca02c" if v >= 0 else "#d62728" for v in by_reason.total],
        text=[f"n={n}<br>${t:+.2f}<br>avg {b:+.0f}bp" for n, t, b
              in zip(by_reason.n, by_reason.total, by_reason.avg_bp)],
        textposition="auto",
    ))
    fig_reason.update_layout(title="PnL by close reason", height=300,
                              template="plotly_white",
                              margin=dict(l=50, r=20, t=50, b=30))

    # --- By symbol (top 15 worst) ---
    by_sym = df.groupby("sym").agg(n=("pnl","count"), total=("pnl","sum")).reset_index()
    worst_syms = by_sym.sort_values("total").head(15)
    fig_syms = go.Figure(go.Bar(
        x=worst_syms.total, y=worst_syms.sym, orientation="h",
        marker_color=["#d62728"] * len(worst_syms),
        text=[f"n={n} ${t:+.2f}" for n, t in zip(worst_syms.n, worst_syms.total)],
        textposition="auto",
    ))
    fig_syms.update_layout(title="Worst 15 symbols (by net $)", height=480,
                            yaxis={"autorange": "reversed"},
                            template="plotly_white",
                            margin=dict(l=140, r=20, t=50, b=30))

    # --- All-trades table ---
    tbl_df = df.sort_values("entry_ms", ascending=False)[
        ["entry_dt", "sym", "side", "pnl", "move_bp", "hold_ms",
         "close_reason", "sig_diff_bp", "entry_price", "exit_price", "adopted"]
    ].copy()
    tbl_df["entry_dt"] = tbl_df.entry_dt.dt.strftime("%H:%M:%S")
    tbl_df["hold_s"] = (tbl_df["hold_ms"] / 1000).round(1)
    tbl_df = tbl_df.drop(columns=["hold_ms"])

    fig_tbl = go.Figure(data=[go.Table(
        header=dict(values=list(tbl_df.columns),
                    fill_color="#333", font=dict(color="white"), align="left"),
        cells=dict(
            values=[tbl_df[c] for c in tbl_df.columns],
            fill_color=[["#fff" if p >= 0 else "#ffeaea" for p in tbl_df.pnl]],
            align="left", height=22, font=dict(size=11),
        ),
    )])
    fig_tbl.update_layout(height=480, margin=dict(l=10, r=10, t=20, b=10),
                          title="All trades (newest first)")

    # --- Per-losing-trade subplots ---
    losers = df[df.pnl < 0].sort_values("pnl").head(top_n).to_dict("records")
    ncols = 2
    nrows = max(1, (len(losers) + ncols - 1) // ncols)
    fig_losers = make_subplots(
        rows=nrows, cols=ncols,
        subplot_titles=[
            f"{t['sym']} {t['side']} ${t['pnl']:+.2f} move={t['move_bp']:+.0f}bp "
            f"hold={t['hold_ms']/1000:.0f}s<br>"
            f"<sub>reason={t['close_reason']} sig_diff={t.get('sig_diff_bp','?')}bp</sub>"
            for t in losers
        ],
        vertical_spacing=0.08, horizontal_spacing=0.07,
    )
    for idx, t in enumerate(losers):
        r, c = idx // ncols + 1, idx % ncols + 1
        bsym = to_binance_sym(t["sym"])
        window_ms = int(window_min * 60_000)
        klines = fetch_klines(bsym, t["entry_ms"] - window_ms, t["exit_ms"] + window_ms)
        if klines:
            xs = [datetime.fromtimestamp(k[0] / 1000) for k in klines]
            closes = [float(k[4]) for k in klines]
            fig_losers.add_trace(
                go.Scatter(x=xs, y=closes, mode="lines",
                           line=dict(color="#555", width=1.3),
                           name="Binance 1m close", showlegend=(idx == 0)),
                row=r, col=c,
            )
        entry_dt = datetime.fromtimestamp(t["entry_ms"] / 1000)
        exit_dt = datetime.fromtimestamp(t["exit_ms"] / 1000)
        fig_losers.add_trace(
            go.Scatter(x=[entry_dt], y=[t["entry_price"]],
                       mode="markers",
                       marker=dict(symbol="triangle-up" if t["side"] == "buy" else "triangle-down",
                                   size=14, color="#0066cc",
                                   line=dict(width=1, color="white")),
                       name=f"entry {t['side']}", showlegend=(idx == 0)),
            row=r, col=c,
        )
        fig_losers.add_trace(
            go.Scatter(x=[exit_dt], y=[t["exit_price"]],
                       mode="markers",
                       marker=dict(symbol="x", size=14, color="#cc2222",
                                   line=dict(width=1, color="white")),
                       name="exit", showlegend=(idx == 0)),
            row=r, col=c,
        )
    fig_losers.update_layout(
        height=max(400, 280 * nrows),
        margin=dict(l=40, r=20, t=60, b=40),
        template="plotly_white",
        title=f"Top {len(losers)} losing trades — Binance 1m price context "
              f"(±{window_min:g} min)",
    )
    for ann in fig_losers["layout"]["annotations"]:
        ann["font"] = dict(size=10)

    # --- Assemble single HTML ---
    parts = []
    parts.append("""
<!DOCTYPE html><html><head><meta charset="utf-8">
<title>Flipster latency_pick — trades dashboard</title>
<script src="https://cdn.plot.ly/plotly-2.35.2.min.js"></script>
<style>
  body { font-family: -apple-system, BlinkMacSystemFont, sans-serif; margin: 20px; background: #fafafa; }
  h1 { color: #222; }
  .stats { display: flex; gap: 12px; margin: 20px 0; flex-wrap: wrap; }
  .stat { background: #fff; padding: 14px 20px; border-radius: 8px;
          box-shadow: 0 1px 3px rgba(0,0,0,.08); min-width: 110px; }
  .stat .v { font-size: 22px; font-weight: 600; }
  .stat .l { font-size: 11px; color: #666; text-transform: uppercase; letter-spacing: .5px; }
  .stat.pos .v { color: #2ca02c; }
  .stat.neg .v { color: #d62728; }
  .row { display: flex; gap: 12px; margin: 14px 0; }
  .card { background: #fff; border-radius: 8px; padding: 10px;
          box-shadow: 0 1px 3px rgba(0,0,0,.08); flex: 1; overflow: hidden; }
  .note { color: #666; font-size: 12px; margin-top: 8px; }
</style></head><body>
<h1>Flipster latency_pick — trades dashboard</h1>
<div class="note">Generated """ + datetime.now().strftime("%Y-%m-%d %H:%M:%S") + """ · """
+ f"""parsed {n} trades · reference epoch = log mtime</div>""")
    parts.append(stats_html)
    parts.append('<div class="card">' + fig_cum.to_html(include_plotlyjs=False, full_html=False) + "</div>")
    parts.append('<div class="row">')
    parts.append('<div class="card">' + fig_reason.to_html(include_plotlyjs=False, full_html=False) + "</div>")
    parts.append('<div class="card">' + fig_syms.to_html(include_plotlyjs=False, full_html=False) + "</div>")
    parts.append("</div>")
    parts.append('<div class="card">' + fig_tbl.to_html(include_plotlyjs=False, full_html=False) + "</div>")
    parts.append('<div class="card">' + fig_losers.to_html(include_plotlyjs=False, full_html=False) + "</div>")
    parts.append("</body></html>")
    out_path.write_text("\n".join(parts))
    print(f"→ wrote {out_path} ({len(losers)} losing trades plotted)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--trades", type=Path, default="/tmp/trades.json")
    ap.add_argument("--out", type=Path, default="/tmp/trades_dashboard.html")
    ap.add_argument("--top", type=int, default=12)
    ap.add_argument("--window-min", type=float, default=8)
    ap.add_argument("--log-mtime", action="store_true")
    args = ap.parse_args()

    trades = json.loads(args.trades.read_text())
    if args.log_mtime:
        log = Path("/home/gate1/projects/quant/flipster_kattpish/logs/latency_pick.log")
        ref_ms = int(log.stat().st_mtime * 1000)
    else:
        ref_ms = int(time.time() * 1000)
    build_dashboard(trades, ref_ms, args.out, top_n=args.top, window_min=args.window_min)


if __name__ == "__main__":
    main()

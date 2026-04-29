#!/usr/bin/env python3
"""Plot price context around top N losing trades.

Uses Binance klines (1s if ≤1h window, else 1m) for price ladder, overlays entry/exit markers.
Generates a single multi-subplot PNG.
"""
from __future__ import annotations
import argparse, json, sys, time, urllib.request, urllib.parse
from datetime import datetime, timedelta, timezone
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import matplotlib.dates as mdates

BINANCE_KLINES = "https://fapi.binance.com/fapi/v1/klines"


def to_binance_sym(fsym: str) -> str:
    return fsym.replace("USDT.PERP", "USDT")


def fetch_klines(bsym: str, start_ms: int, end_ms: int, interval: str = "1s") -> list:
    """Fetch Binance klines. Returns list of [open_t, o, h, l, c, ...] per candle."""
    out = []
    cur = start_ms
    while cur < end_ms:
        params = {"symbol": bsym, "interval": interval, "startTime": cur,
                  "endTime": end_ms, "limit": 1500}
        url = BINANCE_KLINES + "?" + urllib.parse.urlencode(params)
        try:
            data = json.loads(urllib.request.urlopen(url, timeout=10).read())
        except Exception as e:
            print(f"  fetch err {bsym}: {e}", file=sys.stderr)
            return out
        if not data:
            break
        out.extend(data)
        if len(data) < 1500:
            break
        cur = data[-1][0] + 1
        time.sleep(0.05)  # rate limit courtesy
    return out


def sig_ts_to_epoch_ms(sig_ts_str: str, ref_epoch_ms: int) -> int | None:
    """'HH:MM:SS' (local time) + reference date → epoch ms.
    Uses local timezone (log timestamps are written with time.localtime())."""
    if not sig_ts_str:
        return None
    try:
        hh, mm, ss = map(int, sig_ts_str.split(":"))
    except Exception:
        return None
    # Reference as local-time date
    ref_local = datetime.fromtimestamp(ref_epoch_ms / 1000)  # naive local
    dt_local = ref_local.replace(hour=hh, minute=mm, second=ss, microsecond=0)
    return int(dt_local.timestamp() * 1000)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--trades", type=Path, default="/tmp/trades.json")
    ap.add_argument("--top", type=int, default=10, help="plot top N worst PnL trades")
    ap.add_argument("--window-min", type=float, default=5,
                    help="price context window on each side of entry (minutes)")
    ap.add_argument("--out", type=Path, default="/tmp/losing_trades.png")
    ap.add_argument("--log-mtime", action="store_true",
                    help="Use log file mtime as reference epoch instead of now")
    args = ap.parse_args()

    trades = json.loads(args.trades.read_text())
    losers = sorted([t for t in trades if t["pnl"] < 0], key=lambda t: t["pnl"])[:args.top]
    if not losers:
        print("no losing trades to plot"); return

    # Reference epoch for HH:MM:SS conversion
    if args.log_mtime:
        log = Path("/home/gate1/projects/quant/flipster_kattpish/logs/latency_pick.log")
        ref_ms = int(log.stat().st_mtime * 1000)
    else:
        ref_ms = int(time.time() * 1000)

    ncols = 2
    nrows = (len(losers) + ncols - 1) // ncols
    fig, axes = plt.subplots(nrows, ncols, figsize=(14, 3.4 * nrows), squeeze=False)
    axes_flat = [ax for row in axes for ax in row]

    for idx, t in enumerate(losers):
        ax = axes_flat[idx]
        bsym = to_binance_sym(t["sym"])
        entry_ms = sig_ts_to_epoch_ms(t.get("sig_ts"), ref_ms)
        if entry_ms is None:
            # fallback: compute from hold
            entry_ms = ref_ms - t["hold_ms"] - 60_000
        exit_ms = entry_ms + t["hold_ms"]
        window_ms = int(args.window_min * 60_000)
        start = entry_ms - window_ms
        end = exit_ms + window_ms
        # Binance futures klines: minimum interval is 1m (1s only on spot)
        klines = fetch_klines(bsym, start, end, interval="1m")
        if not klines:
            ax.set_title(f"{t['sym']} {t['side']} — no Binance data")
            continue
        # Plot in local time for readability vs log HH:MM:SS
        xs = [datetime.fromtimestamp(k[0] / 1000) for k in klines]
        close_px = [float(k[4]) for k in klines]
        ax.plot(xs, close_px, color="#444", linewidth=0.9, label="Binance close")

        entry_x = datetime.fromtimestamp(entry_ms / 1000)
        exit_x = datetime.fromtimestamp(exit_ms / 1000)
        ax.axvline(entry_x, color="#0066cc", alpha=0.45, linestyle="--", linewidth=1)
        ax.axvline(exit_x, color="#cc2222", alpha=0.45, linestyle="--", linewidth=1)

        ey = t["entry_price"]
        xy = t["exit_price"]
        side_mk = "^" if t["side"] == "buy" else "v"
        entry_color = "#0066cc"
        ax.scatter([entry_x], [ey], marker=side_mk, s=110, color=entry_color,
                   zorder=5, label=f"entry {t['side']} @ {ey:g}")
        ax.scatter([exit_x], [xy], marker="X", s=110, color="#cc2222",
                   zorder=5, label=f"exit @ {xy:g}")

        sig_diff = f"{t.get('sig_diff_bp', '?')}"
        reason = t["close_reason"].replace(",confirm=3", "")
        title = (f"{t['sym']} {t['side']}  ${t['pnl']:+.2f}  "
                 f"move={t['move_bp']:+.0f}bp  hold={t['hold_ms']/1000:.0f}s\n"
                 f"reason={reason}  sig_diff={sig_diff}bp")
        ax.set_title(title, fontsize=9)
        ax.xaxis.set_major_formatter(mdates.DateFormatter("%H:%M:%S"))
        ax.tick_params(axis='x', labelsize=7)
        ax.tick_params(axis='y', labelsize=7)
        ax.legend(fontsize=6, loc="best")
        ax.grid(True, alpha=0.25)

    # Hide unused axes
    for i in range(len(losers), len(axes_flat)):
        axes_flat[i].axis("off")

    fig.suptitle(f"Top {len(losers)} losing trades — Binance price context "
                 f"(±{args.window_min:g}min)", fontsize=11)
    fig.tight_layout(rect=[0, 0, 1, 0.97])
    fig.savefig(args.out, dpi=110)
    print(f"→ wrote {args.out}")


if __name__ == "__main__":
    main()

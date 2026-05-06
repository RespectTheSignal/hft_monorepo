#!/usr/bin/env python3
"""Log Lighter stock perp price vs Yahoo Finance every 10s to CSV.

Goal: see if cross-venue divergences mean-revert (capturable alpha).
"""
import csv
import json
import os
import time
import urllib.parse
import urllib.request
from datetime import datetime, timezone

QDB_HTTP = os.getenv("QDB_HTTP_URL", "http://211.181.122.102:9000")
CSV_PATH = os.getenv("BASIS_CSV", "logs/lighter_stock_basis.csv")
SYMS = [
    "NVDA", "TSLA", "AAPL", "AMZN", "MSFT", "META", "GOOGL", "AMD",
    "MU", "INTC", "SPY", "QQQ", "ORCL", "ASML", "SNDK", "AVGO", "MRVL",
]
INTERVAL_S = 10


def lighter_price(sym):
    sql = (
        f"SELECT (bid_price+ask_price)/2 FROM lighter_bookticker "
        f"WHERE symbol='{sym}' AND timestamp > dateadd('m',-1,now()) "
        f"ORDER BY timestamp DESC LIMIT 1"
    )
    try:
        r = json.loads(
            urllib.request.urlopen(
                QDB_HTTP + "/exec?query=" + urllib.parse.quote(sql), timeout=5
            ).read()
        )
        ds = r.get("dataset")
        return ds[0][0] if ds else None
    except Exception:
        return None


def yahoo_price(sym):
    url = (
        f"https://query1.finance.yahoo.com/v8/finance/chart/{sym}"
        f"?interval=1m&range=1d&includePrePost=true"
    )
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
    try:
        r = json.loads(urllib.request.urlopen(req, timeout=8).read())
        result = r.get("chart", {}).get("result")
        if not result:
            return None, None
        ts = result[0].get("timestamp") or []
        closes = result[0]["indicators"]["quote"][0].get("close") or []
        for i in range(len(closes) - 1, -1, -1):
            if closes[i] is not None:
                return closes[i], ts[i] if i < len(ts) else None
        return None, None
    except Exception:
        return None, None


def main():
    os.makedirs(os.path.dirname(CSV_PATH) or ".", exist_ok=True)
    new_file = not os.path.exists(CSV_PATH)
    f = open(CSV_PATH, "a", buffering=1)
    w = csv.writer(f)
    if new_file:
        w.writerow(["ts_iso", "sym", "lighter", "yahoo", "diff_bp", "yh_age_s"])
    print(
        f"basis logger -> {CSV_PATH} ({len(SYMS)} syms / {INTERVAL_S}s)",
        flush=True,
    )
    while True:
        t0 = time.time()
        rows = []
        ts_iso = datetime.now(timezone.utc).isoformat(timespec="seconds")
        for s in SYMS:
            lp = lighter_price(s)
            yp, yts = yahoo_price(s)
            if lp is None or yp is None or yp <= 0:
                continue
            bp = (lp - yp) / yp * 1e4
            age = (time.time() - yts) if yts else 0.0
            rows.append((s, lp, yp, bp, age))
            w.writerow([ts_iso, s, f"{lp:.6f}", f"{yp:.6f}", f"{bp:.4f}", f"{age:.1f}"])
        f.flush()
        if rows:
            top = sorted(rows, key=lambda r: -abs(r[3]))[:5]
            print(
                f"{ts_iso[11:19]}  top |Δ|: "
                + "  ".join(f"{r[0]}={r[3]:+.1f}bp" for r in top),
                flush=True,
            )
        time.sleep(max(0.0, INTERVAL_S - (time.time() - t0)))


if __name__ == "__main__":
    main()

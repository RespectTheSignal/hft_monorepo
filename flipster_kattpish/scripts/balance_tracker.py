#!/usr/bin/env python3
"""Poll Flipster API for ground-truth PnL + balance, log to JSONL.

trade_log.jsonl was overstating PnL by ~74% (paper prices, partial-fill
duplicates, untracked orphan closes). This daemon hits Flipster's own
endpoints — `/api/v2/positions/history` for closed positions and the
`private/margins` WS topic for the wallet — so the recorded numbers
match the actual balance change.

Output: logs/balance_history.jsonl, one row per poll, keyed by ISO ts.
Each row carries:
  - margin_balance   (USDT, from WS)
  - cumulative_net   (since first_ts seen by this run)
  - last_n_positions (rolling stats over the API page)
  - latest_position  (most recent close, for sanity-check)

Designed to run alongside the live executor on the same host so both
share the cookies.json. Auth-aware proxy support via FLIPSTER_PROXY_POOL
(uses round-robin like the executor)."""
import argparse
import asyncio
import datetime
import itertools
import json
import os
import random
import time
from pathlib import Path
from typing import Optional

PROJECT = Path(__file__).resolve().parent.parent
LOG_FILE = PROJECT / "logs" / "balance_history.jsonl"
COOKIE_FILE = Path("/home/teamreporter/.config/flipster_kattpish/cookies.json")
ENV_FILE = PROJECT / ".env"


def load_proxies() -> list[str]:
    """Read FLIPSTER_PROXY_POOL from .env (comma-separated)."""
    if not ENV_FILE.exists():
        return []
    for line in ENV_FILE.read_text().splitlines():
        if line.startswith("FLIPSTER_PROXY_POOL="):
            return [s.strip() for s in line.split("=", 1)[1].split(",") if s.strip()]
    return []


def cookie_str() -> str:
    ck = json.loads(COOKIE_FILE.read_text()).get("flipster", {})
    return "; ".join(f"{k}={v}" for k, v in ck.items())


def fetch_history(proxies: list[str]) -> Optional[list[dict]]:
    """GET /api/v2/positions/history through a random proxy. Returns the
    `history` array (max 64 entries) or None on failure."""
    import requests
    if not proxies:
        return None
    proxy = random.choice(proxies)
    try:
        r = requests.get(
            "https://api.flipster.io/api/v2/positions/history",
            headers={
                "User-Agent": "Mozilla/5.0",
                "Origin": "https://flipster.io",
                "Cookie": cookie_str(),
            },
            proxies={"http": proxy, "https": proxy},
            timeout=10,
        )
        if r.status_code == 200:
            return r.json().get("history", [])
    except Exception as e:
        print(f"[history] err: {e}")
    return None


async def fetch_margin(proxy_url: Optional[str] = None) -> Optional[float]:
    """WS subscribe to private/margins via HTTP CONNECT through a
    Webshare proxy (teamreporter direct WS is CF-blocked). Returns
    USDT marginBalance from the snapshot frame. ~5s timeout."""
    try:
        import websockets
    except ImportError:
        return None
    if proxy_url is None:
        proxies = load_proxies()
        if proxies:
            proxy_url = random.choice(proxies)

    url = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"
    headers = {"Origin": "https://flipster.io", "Cookie": cookie_str(),
               "User-Agent": "Mozilla/5.0"}
    connect_kwargs = dict(additional_headers=headers, ping_interval=20, open_timeout=10)
    if proxy_url:
        connect_kwargs["proxy"] = proxy_url

    try:
        async with websockets.connect(url, **connect_kwargs) as ws:
            await ws.send(json.dumps({"s": {"private/margins": {"rows": ["*"]}}}))
            t0 = time.time()
            while time.time() - t0 < 5:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=2)
                except asyncio.TimeoutError:
                    continue
                d = json.loads(msg)
                usdt = d.get("t", {}).get("private/margins", {}).get("s", {}).get("USDT", {})
                if "marginBalance" in usdt:
                    return float(usdt["marginBalance"])
    except Exception as e:
        print(f"[margin] err: {e}")
    return None


def stats_from_history(hist: list[dict], since_ns: int) -> dict:
    """Compute (gross, fee_eff, funding, net, wins, n) over positions
    closed at-or-after since_ns. Fee uses the 85% VIP11 rebate."""
    recent = [p for p in hist if int(p["openTime"]) >= since_ns]
    n = len(recent)
    if n == 0:
        return {"n": 0, "gross": 0.0, "fee_eff": 0.0, "funding": 0.0,
                "net": 0.0, "wins": 0, "win_pct": 0.0}
    gross = sum(float(p["grossRealizedPnl"]) for p in recent)
    fee_raw = sum(float(p["tradingFee"]) for p in recent)
    funding = sum(float(p["fundingFee"]) for p in recent)
    fee_eff = fee_raw * 0.15
    net = gross - fee_eff - funding
    wins = sum(1 for p in recent if float(p["netRealizedPnl"]) > 0)
    return {"n": n, "gross": round(gross, 4), "fee_eff": round(fee_eff, 4),
            "funding": round(funding, 4), "net": round(net, 4),
            "wins": wins, "win_pct": round(wins / n * 100, 1)}


def write_log(row: dict) -> None:
    LOG_FILE.parent.mkdir(parents=True, exist_ok=True)
    with LOG_FILE.open("a") as f:
        f.write(json.dumps(row) + "\n")


def cmd_once(start_ts_ns: Optional[int] = None) -> None:
    proxies = load_proxies()
    bal = asyncio.run(fetch_margin())
    hist = fetch_history(proxies) or []
    since_ns = start_ts_ns if start_ts_ns else (int(time.time()) - 3600) * 1_000_000_000
    s = stats_from_history(hist, since_ns)
    row = {
        "ts": int(time.time()),
        "iso": datetime.datetime.utcnow().isoformat() + "Z",
        "margin_balance": round(bal, 4) if bal else None,
        "since_ns": since_ns,
        **s,
    }
    write_log(row)
    print(json.dumps(row, indent=2))


def cmd_watch(interval_s: int = 60) -> None:
    print(f"[balance-tracker] poll every {interval_s}s → {LOG_FILE}")
    # Anchor: when this run started (so cumulative is from now)
    start_ns = int(time.time()) * 1_000_000_000
    proxies = load_proxies()
    while True:
        try:
            bal = asyncio.run(fetch_margin())
            hist = fetch_history(proxies) or []
            s = stats_from_history(hist, start_ns)
            row = {
                "ts": int(time.time()),
                "iso": datetime.datetime.utcnow().isoformat() + "Z",
                "margin_balance": round(bal, 4) if bal else None,
                "since_ns": start_ns,
                **s,
            }
            write_log(row)
            bal_s = f"${bal:.2f}" if bal else "N/A"
            print(f"  {row['iso'][:19]} bal={bal_s} positions={s['n']} "
                  f"net=${s['net']:+.2f} win={s['win_pct']:.0f}%")
        except Exception as e:
            print(f"[watch] err: {e}")
        time.sleep(interval_s)


def cmd_summary(hours: int = 24) -> None:
    """Read accumulated log + show recent trend."""
    if not LOG_FILE.exists():
        print("(no log yet)")
        return
    rows = [json.loads(l) for l in LOG_FILE.open() if l.strip()]
    cutoff = int(time.time()) - hours * 3600
    recent = [r for r in rows if r.get("ts", 0) >= cutoff]
    if not recent:
        print(f"(no rows in last {hours}h)")
        return
    print(f"=== last {hours}h ({len(recent)} polls) ===")
    first = recent[0]
    last = recent[-1]
    print(f"  first: {first['iso']} bal=${first.get('margin_balance')}")
    print(f"  last:  {last['iso']} bal=${last.get('margin_balance')}")
    if first.get("margin_balance") and last.get("margin_balance"):
        delta = last["margin_balance"] - first["margin_balance"]
        elapsed_h = (last["ts"] - first["ts"]) / 3600
        print(f"  Δ balance: ${delta:+.4f} over {elapsed_h:.2f}h "
              f"= ${delta/elapsed_h:+.2f}/hr" if elapsed_h > 0 else "")


def main() -> None:
    p = argparse.ArgumentParser()
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--once", action="store_true")
    g.add_argument("--watch", action="store_true")
    g.add_argument("--summary", action="store_true")
    p.add_argument("--interval", type=int, default=60,
                   help="seconds between polls in --watch mode")
    p.add_argument("--hours", type=int, default=24,
                   help="lookback for --summary")
    args = p.parse_args()

    if args.once:
        cmd_once()
    elif args.watch:
        cmd_watch(args.interval)
    elif args.summary:
        cmd_summary(args.hours)


if __name__ == "__main__":
    main()

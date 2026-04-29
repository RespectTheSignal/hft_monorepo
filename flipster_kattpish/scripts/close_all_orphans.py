#!/usr/bin/env python3
"""Close all open Flipster perpetual positions via PUT size=0.

Pulls live state from the private WS tracker, then issues close orders
for every position at the current mid (or bid for safety on long, ask
for short — but we don't know side, so use bid which is conservative).
"""

from __future__ import annotations

import asyncio
import json
import sys
import time
import urllib.request
from pathlib import Path

import requests

sys.path.insert(0, str(Path(__file__).resolve().parent))
from positions_ws import WSPositionTracker, _fetch_cookies

MONOREPO = Path(__file__).resolve().parent.parent.parent
sys.path.insert(0, str(MONOREPO / "flipster_web"))
from python.client import FlipsterClient
from python.browser import BrowserManager as FBM
from python.order import OrderParams


async def main():
    # Cookies + proxies
    resp = urllib.request.urlopen("http://localhost:9230/json/version")
    cdp_url = json.loads(resp.read())["webSocketDebuggerUrl"]
    cookies = await _fetch_cookies(cdp_url)

    proxies_path = Path(__file__).parent / "proxies.txt"
    proxies = [p for p in proxies_path.read_text().splitlines() if p.strip()] if proxies_path.exists() else []

    # FlipsterClient just for close_position helper (uses cookies + proxies)
    fc = FlipsterClient(FBM(), proxies=proxies)
    fc._cookies = cookies
    fc._session = requests.Session()
    fc._session.cookies.update(cookies)
    fc._session.headers.update({
        "User-Agent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
                      "(KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36",
        "Accept": "application/json, text/plain, */*",
        "Referer": "https://flipster.io/", "Content-Type": "application/json",
        "X-Prex-Client-Platform": "web",
        "X-Prex-Client-Version": "release-web-3.15.110",
        "Origin": "https://flipster.io",
    })

    # Spin up tracker, wait for snapshot
    tracker = WSPositionTracker(cookies=cookies)
    log = lambda m: print(f"[{time.strftime('%H:%M:%S')}] {m}", flush=True)
    task = asyncio.create_task(tracker.run(log))
    await asyncio.sleep(6)

    if not tracker.positions:
        log("No open positions to close.")
        task.cancel()
        return

    log(f"\nFound {len(tracker.positions)} positions, margin_avail=${tracker.margin_avail:.2f}")
    for k, v in sorted(tracker.positions.items()):
        log(f"  {k}: marg={v.get('initMarginReserved','?')} unr={v.get('unrealizedPnl','?')} "
            f"mid={v.get('midPrice','?')}")

    log("\nClosing all... (PUT size=0 at mid price)")
    results = []
    for sym_slot, fields in list(tracker.positions.items()):
        if "/" in sym_slot:
            sym, slot_s = sym_slot.split("/", 1)
            slot = int(slot_s)
        else:
            sym, slot = sym_slot, 0
        mid = fields.get("midPrice") or fields.get("bidPrice") or fields.get("askPrice")
        if not mid:
            # Fallback: fetch from Binance perpetual ticker
            base = sym.replace("USDT.PERP", "")
            try:
                r = requests.get(f"https://fapi.binance.com/fapi/v1/ticker/price?symbol={base}USDT", timeout=5)
                mid = float(r.json()["price"])
                log(f"  {sym_slot}: WS price missing, using Binance {mid}")
            except Exception:
                log(f"  SKIP {sym_slot}: no price (WS or Binance)")
                results.append((sym_slot, "no-price"))
                continue
        try:
            r = await asyncio.to_thread(fc.close_position, sym, slot, float(mid))
            log(f"  ✓ closed {sym_slot} at {mid}")
            results.append((sym_slot, "ok"))
        except Exception as e:
            log(f"  ✗ failed {sym_slot}: {str(e)[:200]}")
            results.append((sym_slot, str(e)[:120]))
        await asyncio.sleep(0.3)  # gentle pacing

    # Wait for WS to confirm
    await asyncio.sleep(5)
    log(f"\nAfter close: {len(tracker.positions)} still open, margin_avail=${tracker.margin_avail:.2f}")
    for k, v in sorted(tracker.positions.items()):
        log(f"  STILL OPEN {k}: marg={v.get('initMarginReserved','?')} unr={v.get('unrealizedPnl','?')}")

    ok = sum(1 for _, r in results if r == "ok")
    log(f"\nDone: {ok}/{len(results)} close requests OK")
    task.cancel()


if __name__ == "__main__":
    asyncio.run(main())

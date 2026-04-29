#!/usr/bin/env python3
"""Flipster orderbook depth collector — both v1 (HMAC API) and v2 (browser).

Two parallel WS clients write to QuestDB tables:
  flipster_depth_v1  ← wss://trading-api.flipster.io/api/v1/stream  (HMAC)
  flipster_depth_v2  ← wss://api.flipster.io/api/v2/stream/r230522  (cookies)

v1 advantage: stable auth (no cookie expiry), no browser dependency.
v2 advantage: same source as web UI, may have different update cadence.

Usage:
    python3 scripts/depth_collector.py --symbols BTC,ETH,SOL,XRP,DOGE
"""
from __future__ import annotations

import argparse
import asyncio
import hashlib
import hmac
import json
import os
import sys
import time
import urllib.request
from pathlib import Path

import websockets

MONOREPO = Path(__file__).resolve().parent.parent.parent

QDB_HOST = "211.181.122.102"
QDB_ILP_PORT = 9009


def make_ilp_line(table: str, tags: dict, fields: dict, ts_ns: int) -> str:
    tag_str = ",".join(f"{k}={v}" for k, v in tags.items())
    field_str = ",".join(
        f'{k}="{v}"' if isinstance(v, str) else f"{k}={v}"
        for k, v in fields.items()
    )
    return f"{table},{tag_str} {field_str} {ts_ns}\n"


class IlpClient:
    """Async TCP ILP client with auto-reconnect."""

    def __init__(self, host: str, port: int):
        self.host = host
        self.port = port
        self._writer = None
        self._lock = asyncio.Lock()

    async def _connect(self):
        _, writer = await asyncio.open_connection(self.host, self.port)
        self._writer = writer

    async def write(self, line: str):
        async with self._lock:
            for attempt in range(2):
                try:
                    if self._writer is None:
                        await self._connect()
                    self._writer.write(line.encode())
                    await self._writer.drain()
                    return
                except (ConnectionError, BrokenPipeError, OSError) as e:
                    self._writer = None
                    if attempt == 1:
                        raise


def book_to_fields(bids: list, asks: list, levels: int) -> dict:
    """Build flat field dict from top-N bids/asks. Handles malformed entries."""
    fields = {}
    valid_bids = [b for b in bids if len(b) >= 2][:levels]
    valid_asks = [a for a in asks if len(a) >= 2][:levels]
    if not valid_bids or not valid_asks:
        return {}
    for i, (px, sz) in enumerate(valid_bids):
        fields[f"bp{i}"] = float(px)
        fields[f"bs{i}"] = float(sz)
    for i, (px, sz) in enumerate(valid_asks):
        fields[f"ap{i}"] = float(px)
        fields[f"as{i}"] = float(sz)
    return fields


# ----------------------------------------------------------------------------
# V1 collector (HMAC API)
# ----------------------------------------------------------------------------

async def v1_collector(symbols: list[str], levels: int, ilp: IlpClient,
                       book_state: dict):
    """Subscribe to v1 orderbook topic. Maintains delta-merged book per symbol.

    book_state["v1"][symbol] = {"bids": {price: size, ...}, "asks": {...}}
    Sorted top-N pulled per snapshot for ILP write.
    """
    api_key = os.getenv("FLIPSTER_API_KEY", "3|8Q9vdANlA1pvIbimev5NgB__5ULBH3_a")
    api_secret = os.getenv("FLIPSTER_API_SECRET",
                           "3c823cd1b32582d697b359898952020d0018cdb93b40d6ec4e49bad020d361e2")

    while True:
        try:
            expires = int(time.time()) + 60
            msg = f"GET/api/v1/stream{expires}"
            sig = hmac.new(api_secret.encode(), msg.encode(), hashlib.sha256).hexdigest()
            headers = {"api-key": api_key, "api-expires": str(expires), "api-signature": sig}
            url = "wss://trading-api.flipster.io/api/v1/stream"

            async with websockets.connect(url, additional_headers=headers, ping_interval=20) as ws:
                topics = [f"orderbook.{s}" for s in symbols]
                # batch subscribe
                for chunk_i in range(0, len(topics), 50):
                    chunk = topics[chunk_i:chunk_i + 50]
                    await ws.send(json.dumps({"op": "subscribe", "args": chunk}))
                print(f"[v1] subscribed to {len(topics)} orderbook topics")

                book_state.setdefault("v1", {})
                msg_count = 0
                last_log = time.time()

                while True:
                    raw = await ws.recv()
                    try:
                        d = json.loads(raw)
                    except json.JSONDecodeError:
                        continue

                    topic = d.get("topic", "")
                    if not topic.startswith("orderbook."):
                        continue
                    symbol = topic[len("orderbook."):]
                    data = d.get("data", [])
                    ts_ns = int(d.get("ts", time.time_ns()))

                    book = book_state["v1"].setdefault(symbol, {"bids": {}, "asks": {}})

                    for entry in data:
                        action = entry.get("actionType", "")
                        rows = entry.get("rows", [])
                        for row in rows:
                            sym = row.get("symbol")
                            if sym != symbol:
                                continue
                            new_bids = row.get("bids", [])
                            new_asks = row.get("asks", [])

                            if action == "SNAPSHOT":
                                book["bids"] = {float(p): float(s) for p, s in new_bids if len(new_bids[0]) >= 2}
                                book["asks"] = {float(p): float(s) for p, s in new_asks if len(new_asks[0]) >= 2}
                            elif action == "UPDATE":
                                for p, s in new_bids:
                                    sz = float(s)
                                    if sz == 0:
                                        book["bids"].pop(float(p), None)
                                    else:
                                        book["bids"][float(p)] = sz
                                for p, s in new_asks:
                                    sz = float(s)
                                    if sz == 0:
                                        book["asks"].pop(float(p), None)
                                    else:
                                        book["asks"][float(p)] = sz

                    # Get top-N sorted
                    top_bids = sorted(book["bids"].items(), reverse=True)[:levels]
                    top_asks = sorted(book["asks"].items())[:levels]
                    bids_lst = [[str(p), str(s)] for p, s in top_bids]
                    asks_lst = [[str(p), str(s)] for p, s in top_asks]
                    fields = book_to_fields(bids_lst, asks_lst, levels)
                    if not fields:
                        continue

                    line = make_ilp_line("flipster_depth_v1", {"symbol": symbol}, fields, ts_ns)
                    try:
                        await ilp.write(line)
                        msg_count += 1
                    except Exception as e:
                        print(f"[v1 ilp err] {e}")

                    now = time.time()
                    if now - last_log > 60:
                        rate = msg_count / (now - last_log)
                        print(f"[v1 stat] {msg_count} writes ({rate:.1f}/s) syms_with_book={len(book_state['v1'])}")
                        msg_count = 0
                        last_log = now

        except Exception as e:
            print(f"[v1] err: {e}, reconnect in 5s")
            await asyncio.sleep(5)


# ----------------------------------------------------------------------------
# V2 collector (browser cookies)
# ----------------------------------------------------------------------------

async def _async_extract_cookies(cdp_ws_url: str) -> dict:
    """Async version of cookie extraction (avoids event-loop conflicts)."""
    async with websockets.connect(cdp_ws_url) as ws:
        await ws.send(json.dumps({"id": 1, "method": "Storage.getCookies"}))
        resp = json.loads(await ws.recv())
        all_cookies = resp.get("result", {}).get("cookies", [])
        return {
            c["name"]: c["value"]
            for c in all_cookies
            if "flipster" in c.get("domain", "")
        }


async def v2_collector(symbols: list[str], levels: int, ilp: IlpClient, cdp_port: int):
    while True:
        try:
            ver = json.loads(urllib.request.urlopen(
                f"http://localhost:{cdp_port}/json/version", timeout=5
            ).read())
            cookies = await _async_extract_cookies(ver["webSocketDebuggerUrl"])
            cookie_str = "; ".join(f"{k}={v}" for k, v in cookies.items())
            headers = {
                "Cookie": cookie_str,
                "Origin": "https://flipster.io",
                "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0",
            }
            url = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"

            async with websockets.connect(url, additional_headers=headers, ping_interval=20) as ws:
                sub = {"s": {"market/orderbooks-v2": {"rows": symbols}}}
                await ws.send(json.dumps(sub))
                print(f"[v2] subscribed to {len(symbols)} symbols")

                msg_count = 0
                last_log = time.time()

                while True:
                    raw = await ws.recv()
                    try:
                        d = json.loads(raw)
                    except json.JSONDecodeError:
                        continue

                    if "t" not in d or "market/orderbooks-v2" not in d.get("t", {}):
                        continue
                    ts_ns = int(d.get("p", time.time_ns()))
                    s_data = d["t"]["market/orderbooks-v2"].get("s", {})
                    for sym, book in s_data.items():
                        bids = book.get("bids", [])
                        asks = book.get("asks", [])
                        fields = book_to_fields(bids, asks, levels)
                        if not fields:
                            continue
                        line = make_ilp_line("flipster_depth_v2", {"symbol": sym}, fields, ts_ns)
                        try:
                            await ilp.write(line)
                            msg_count += 1
                        except Exception as e:
                            print(f"[v2 ilp err] {e}")

                    now = time.time()
                    if now - last_log > 60:
                        rate = msg_count / (now - last_log)
                        print(f"[v2 stat] {msg_count} writes ({rate:.1f}/s)")
                        msg_count = 0
                        last_log = now

        except Exception as e:
            print(f"[v2] err: {e}, reconnect in 5s")
            await asyncio.sleep(5)


# ----------------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------------

async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--symbols", default="BTC,ETH,SOL,XRP,DOGE,BNB,ADA")
    p.add_argument("--levels", type=int, default=10)
    p.add_argument("--cdp-port", type=int, default=9230)
    p.add_argument("--no-v1", action="store_true", help="Disable v1 (HMAC) collector")
    p.add_argument("--no-v2", action="store_true", help="Disable v2 (cookie) collector")
    args = p.parse_args()

    symbols = [f"{s.strip()}USDT.PERP" for s in args.symbols.split(",") if s.strip()]
    print(f"[init] {len(symbols)} symbols, top-{args.levels} levels")

    ilp = IlpClient(QDB_HOST, QDB_ILP_PORT)
    book_state: dict = {}

    tasks = []
    if not args.no_v1:
        tasks.append(asyncio.create_task(v1_collector(symbols, args.levels, ilp, book_state)))
    if not args.no_v2:
        tasks.append(asyncio.create_task(v2_collector(symbols, args.levels, ilp, args.cdp_port)))

    await asyncio.gather(*tasks)


if __name__ == "__main__":
    asyncio.run(main())

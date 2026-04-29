#!/usr/bin/env python3
"""Dual-feed Flipster price logger.

Writes top-of-book from both WS feeds into ONE QuestDB table, tagged by source,
so Grafana can overlay web vs api per symbol.

Table: flipster_price_compare
Tags:  symbol, source (web|api)
Fields: bid, ask, mid, other_mid, other_age_ms, diff_bp
  diff_bp = (this_source.mid - other_source.mid) / other_source.mid * 1e4
  Only written when other feed has data within 5s.

Web  feed: wss://api.flipster.io/api/v2/stream/r230522   (browser cookies)
API  feed: wss://trading-api.flipster.io/api/v1/stream   (HMAC)

Usage:
    python3 scripts/price_compare_logger.py \
        --symbols RAVE,M,GENIUS,GWEI,ACE,METIS,BTC,ETH,SOL
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

try:
    from dotenv import load_dotenv
    load_dotenv(Path(__file__).resolve().parent.parent / ".env")
except ImportError:
    pass

QDB_HOST = os.getenv("QDB_HOST", "211.181.122.102")
QDB_ILP_PORT = int(os.getenv("QDB_ILP_PORT", "9009"))


STALE_MS = 5000  # other-feed freshness window for diff_bp


def ilp_line(tags: dict, fields: dict, ts_ns: int) -> str:
    tag_str = ",".join(f"{k}={v}" for k, v in tags.items())
    field_str = ",".join(f"{k}={v}" for k, v in fields.items())
    return f"flipster_price_compare,{tag_str} {field_str} {ts_ns}\n"


def build_fields(this_mid: float, bid: float, ask: float,
                 sym: str, this_source: str, other_source: str,
                 ts_ns: int, latest: dict) -> dict:
    """Assemble field dict + pull the other feed's latest mid to compute diff_bp."""
    fields = {"bid": bid, "ask": ask, "mid": this_mid}
    other = latest.get((sym, other_source))
    if other is not None:
        other_mid, other_ts_ns = other
        age_ms = (ts_ns - other_ts_ns) / 1e6
        if abs(age_ms) < STALE_MS and other_mid > 0:
            fields["other_mid"] = other_mid
            fields["other_age_ms"] = age_ms
            fields["diff_bp"] = (this_mid - other_mid) / other_mid * 1e4
    return fields


class IlpClient:
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
                except (ConnectionError, BrokenPipeError, OSError):
                    self._writer = None
                    if attempt == 1:
                        raise


async def _extract_cookies(cdp_ws_url: str) -> dict:
    async with websockets.connect(cdp_ws_url) as ws:
        await ws.send(json.dumps({"id": 1, "method": "Storage.getCookies"}))
        resp = json.loads(await ws.recv())
        return {
            c["name"]: c["value"]
            for c in resp.get("result", {}).get("cookies", [])
            if "flipster" in c.get("domain", "")
        }


async def web_feed(symbols: list[str], ilp: IlpClient, cdp_port: int,
                   stats: dict, latest: dict):
    while True:
        try:
            ver = json.loads(urllib.request.urlopen(
                f"http://localhost:{cdp_port}/json/version", timeout=5
            ).read())
            cookies = await _extract_cookies(ver["webSocketDebuggerUrl"])
            cookie_str = "; ".join(f"{k}={v}" for k, v in cookies.items())
            headers = {
                "Cookie": cookie_str,
                "Origin": "https://flipster.io",
                "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0",
            }
            url = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"

            async with websockets.connect(url, additional_headers=headers,
                                          ping_interval=20, max_size=10_000_000) as ws:
                await ws.send(json.dumps({"s": {"market/orderbooks-v2": {"rows": symbols}}}))
                print(f"[web] subscribed {len(symbols)} syms", flush=True)
                async for raw in ws:
                    try:
                        d = json.loads(raw)
                    except json.JSONDecodeError:
                        continue
                    s_data = d.get("t", {}).get("market/orderbooks-v2", {}).get("s", {})
                    if not s_data:
                        continue
                    ts_ns = int(d.get("p", time.time_ns()))
                    for sym, book in s_data.items():
                        bids = book.get("bids", [])
                        asks = book.get("asks", [])
                        if not bids or not asks or len(bids[0]) < 2 or len(asks[0]) < 2:
                            continue
                        bid = float(bids[0][0])
                        ask = float(asks[0][0])
                        mid = (bid + ask) / 2.0
                        fields = build_fields(mid, bid, ask, sym, "web", "api",
                                              ts_ns, latest)
                        latest[(sym, "web")] = (mid, ts_ns)
                        line = ilp_line(
                            {"symbol": sym, "source": "web"}, fields, ts_ns,
                        )
                        try:
                            await ilp.write(line)
                            stats["web"] = stats.get("web", 0) + 1
                        except Exception as e:
                            print(f"[web ilp err] {e}", flush=True)
        except Exception as e:
            print(f"[web] err: {e} — reconnect in 5s", flush=True)
            await asyncio.sleep(5)


async def api_feed(symbols: list[str], ilp: IlpClient, key: str, secret: str,
                   stats: dict, latest: dict):
    while True:
        try:
            expires = int(time.time()) + 3600
            sig = hmac.new(
                secret.encode(),
                f"GET/api/v1/stream{expires}".encode(),
                hashlib.sha256,
            ).hexdigest()
            headers = {"api-key": key, "api-expires": str(expires), "api-signature": sig}
            url = "wss://trading-api.flipster.io/api/v1/stream"

            async with websockets.connect(url, additional_headers=headers,
                                          ping_interval=20, max_size=10_000_000) as ws:
                topics = [f"ticker.{s}" for s in symbols]
                for i in range(0, len(topics), 50):
                    await ws.send(json.dumps({"op": "subscribe", "args": topics[i:i + 50]}))
                print(f"[api] subscribed {len(topics)} ticker topics", flush=True)
                async for raw in ws:
                    try:
                        d = json.loads(raw)
                    except json.JSONDecodeError:
                        continue
                    topic = d.get("topic", "")
                    if not topic.startswith("ticker."):
                        continue
                    sym = topic[len("ticker."):]
                    ts_ns = int(d.get("ts", time.time_ns()))
                    for row in d.get("data", []):
                        for r in row.get("rows", []):
                            bid = r.get("bidPrice")
                            ask = r.get("askPrice")
                            if bid is None or ask is None:
                                continue
                            bid_f = float(bid)
                            ask_f = float(ask)
                            mid = (bid_f + ask_f) / 2.0
                            fields = build_fields(mid, bid_f, ask_f, sym, "api",
                                                  "web", ts_ns, latest)
                            latest[(sym, "api")] = (mid, ts_ns)
                            line = ilp_line(
                                {"symbol": sym, "source": "api"}, fields, ts_ns,
                            )
                            try:
                                await ilp.write(line)
                                stats["api"] = stats.get("api", 0) + 1
                            except Exception as e:
                                print(f"[api ilp err] {e}", flush=True)
        except Exception as e:
            print(f"[api] err: {e} — reconnect in 5s", flush=True)
            await asyncio.sleep(5)


async def stat_loop(stats: dict):
    while True:
        await asyncio.sleep(60)
        w = stats.get("web", 0)
        a = stats.get("api", 0)
        print(f"[stat] web={w} api={a} (per min: web={w/1.0:.0f} api={a/1.0:.0f})", flush=True)
        stats.clear()


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--symbols",
        default="RAVE,PROM,M,GENIUS,API3,ORDI,GWEI,ENJ,ACE,RENDER,METIS,DYDX,BTC,ETH,SOL",
        help="Comma-separated bases (USDT.PERP suffix added)",
    )
    ap.add_argument("--cdp-port", type=int, default=9230)
    args = ap.parse_args()

    bases = [s.strip().upper() for s in args.symbols.split(",") if s.strip()]
    symbols = [f"{b}USDT.PERP" for b in bases]

    key = os.environ["FLIPSTER_API_KEY"]
    secret = os.environ["FLIPSTER_API_SECRET"]

    ilp = IlpClient(QDB_HOST, QDB_ILP_PORT)
    stats: dict = {}
    latest: dict = {}

    await asyncio.gather(
        web_feed(symbols, ilp, args.cdp_port, stats, latest),
        api_feed(symbols, ilp, key, secret, stats, latest),
        stat_loop(stats),
        return_exceptions=False,
    )


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass

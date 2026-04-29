#!/usr/bin/env python3
"""OBI Maker Fill Rate Test on Flipster.

Strategy:
  1. Subscribe to depth via v2 stream (faster updates)
  2. Compute imbalance = (sum_bs5 - sum_as5) / total
  3. When |imb| > THRESHOLD, place LIMIT order at TOB
     - imb > +0.7: place LIMIT BID (buy if filled)
     - imb < -0.7: place LIMIT ASK (sell if filled)
  4. Wait FILL_TIMEOUT_SEC for fill
  5. If filled, place opposite LIMIT at TOB to exit (also maker)
     - Wait EXIT_TIMEOUT_SEC, then market close if needed
  6. Log: signal, fill?, time_to_fill, entry_px, exit_px, hold_sec, pnl_bp

Uses Flipster v1 HMAC API (separate account from browser).
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
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

import websockets

API_KEY = os.getenv("FLIPSTER_API_KEY", "3|8Q9vdANlA1pvIbimev5NgB__5ULBH3_a")
API_SECRET = os.getenv("FLIPSTER_API_SECRET",
                       "3c823cd1b32582d697b359898952020d0018cdb93b40d6ec4e49bad020d361e2")

V1_HOST = "https://trading-api.flipster.io"
V2_WS = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"


def hmac_sign(method: str, path: str, expires: int, body: str = "") -> str:
    msg = f"{method}{path}{expires}{body}"
    return hmac.new(API_SECRET.encode(), msg.encode(), hashlib.sha256).hexdigest()


async def http_request(method: str, path: str, body: dict | None = None) -> dict:
    """Async HTTP via aiohttp (or sync via urllib for simplicity)."""
    expires = int(time.time()) + 60
    body_str = json.dumps(body) if body else ""
    sig = hmac_sign(method, path, expires, body_str)
    headers = {
        "api-key": API_KEY,
        "api-expires": str(expires),
        "api-signature": sig,
        "Content-Type": "application/json",
        "User-Agent": "flipster-research/0.1",
        "Accept": "application/json",
    }
    url = V1_HOST + path

    def _do():
        req = urllib.request.Request(url, method=method, headers=headers,
                                     data=body_str.encode() if body_str else None)
        try:
            with urllib.request.urlopen(req, timeout=10) as r:
                return json.loads(r.read())
        except urllib.error.HTTPError as e:
            err = e.read().decode()
            return {"_error": True, "_status": e.code, "_body": err}

    return await asyncio.to_thread(_do)


async def place_limit_order(symbol: str, side: str, qty: str, price: str) -> dict:
    body = {
        "symbol": symbol,
        "side": side.upper(),  # BUY or SELL
        "type": "LIMIT",
        "quantity": qty,
        "price": price,
        "timeInForce": "GTC",
    }
    return await http_request("POST", "/api/v1/trade/order", body)


async def cancel_order(symbol: str, order_id: str) -> dict:
    body = {"symbol": symbol, "orderId": order_id}
    return await http_request("DELETE", "/api/v1/trade/order", body)


async def get_pending_orders(symbol: str) -> list:
    r = await http_request("GET", f"/api/v1/trade/order?symbol={symbol}")
    if isinstance(r, list):
        return r
    return r.get("orders", []) if isinstance(r, dict) else []


async def get_position(symbol: str) -> dict | None:
    r = await http_request("GET", "/api/v1/account/position")
    if isinstance(r, list):
        for p in r:
            if p.get("symbol") == symbol and p.get("positionQty") is not None:
                return p
    return None


@dataclass
class Tester:
    symbol: str
    qty: str  # in BASE (e.g. ETH)
    fill_timeout: float = 5.0
    exit_timeout: float = 30.0
    imb_threshold: float = 0.7
    cooldown_sec: float = 10.0
    max_attempts: int = 50
    log_path: Path = field(default_factory=lambda: Path("logs/maker_obi_test.jsonl"))

    last_attempt_ts: float = 0
    attempts: int = 0
    fills: int = 0

    async def run(self):
        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        print(f"[init] symbol={self.symbol} qty={self.qty} threshold=±{self.imb_threshold}")
        print(f"[init] fill_timeout={self.fill_timeout}s exit_timeout={self.exit_timeout}s")
        print(f"[init] max_attempts={self.max_attempts}")

        # Get cookies for v2 stream
        sys.path.insert(0, "/home/gate1/projects/quant/hft_monorepo/flipster_web")
        from python.cookies import _fetch_cookies
        ver = json.loads(urllib.request.urlopen("http://localhost:9230/json/version").read())
        cookies = await _fetch_cookies(ver["webSocketDebuggerUrl"])
        cookie_str = "; ".join(f"{k}={v}" for k, v in cookies.items())

        headers = {
            "Cookie": cookie_str,
            "Origin": "https://flipster.io",
            "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) Gecko/20100101 Firefox/149.0",
        }

        async with websockets.connect(V2_WS, additional_headers=headers) as ws:
            sub = {"s": {"market/orderbooks-v2": {"rows": [self.symbol]}}}
            await ws.send(json.dumps(sub))
            print(f"[ws] subscribed to {self.symbol} depth")

            while self.attempts < self.max_attempts:
                msg = await ws.recv()
                try:
                    d = json.loads(msg)
                except json.JSONDecodeError:
                    continue
                if "t" not in d or "market/orderbooks-v2" not in d.get("t", {}):
                    continue

                book_data = d["t"]["market/orderbooks-v2"].get("s", {}).get(self.symbol)
                if not book_data:
                    continue

                bids = book_data.get("bids", [])[:5]
                asks = book_data.get("asks", [])[:5]
                if len(bids) < 5 or len(asks) < 5:
                    continue

                bs5 = sum(float(b[1]) for b in bids)
                as5 = sum(float(a[1]) for a in asks)
                if bs5 + as5 == 0:
                    continue
                imb = (bs5 - as5) / (bs5 + as5)

                # Check cooldown
                if time.time() - self.last_attempt_ts < self.cooldown_sec:
                    continue

                # Trigger
                side = None
                if imb > self.imb_threshold:
                    side = "BUY"  # bullish signal — buy with maker BID
                    px = bids[0][0]  # TOB bid
                elif imb < -self.imb_threshold:
                    side = "SELL"  # bearish signal — sell with maker ASK
                    px = asks[0][0]  # TOB ask

                if side is None:
                    continue

                self.last_attempt_ts = time.time()
                self.attempts += 1
                print(f"\n[#{self.attempts}] imb={imb:+.3f}  side={side}  px={px}  qty={self.qty}")

                attempt = await self.do_attempt(side, px, imb)

                # Log
                with open(self.log_path, "a") as f:
                    f.write(json.dumps(attempt) + "\n")

                print(f"   → {attempt}")

            print(f"\n=== DONE: {self.attempts} attempts, {self.fills} fills ({self.fills/self.attempts*100:.1f}%)")

    async def do_attempt(self, side: str, price: str, imb: float) -> dict:
        ts_start = time.time()
        record = {
            "ts": datetime.now(timezone.utc).isoformat(),
            "imb": imb, "side": side, "intent_price": price,
            "filled": False, "exit_filled": False,
            "entry_price": None, "exit_price": None,
            "time_to_fill_s": None, "hold_s": None,
            "pnl_bp": None, "errors": [],
        }

        # 1. Place entry LIMIT
        r = await place_limit_order(self.symbol, side, self.qty, price)
        if r.get("_error"):
            record["errors"].append(f"place_entry: {r.get('_status')} {r.get('_body', '')[:100]}")
            return record

        order_data = r.get("order", r)
        order_id = order_data.get("orderId")
        record["entry_order_id"] = order_id
        record["entry_initial_status"] = order_data.get("status")

        # 2. Wait for fill
        filled = False
        fill_price = None
        for _ in range(int(self.fill_timeout * 2)):  # poll every 0.5s
            await asyncio.sleep(0.5)
            pos = await get_position(self.symbol)
            if pos and pos.get("positionQty") is not None and float(pos.get("positionQty", 0)) != 0:
                filled = True
                fill_price = float(pos.get("entryPrice", price))
                break

        record["time_to_fill_s"] = round(time.time() - ts_start, 2)

        # 3. If not filled, cancel
        if not filled:
            cancel_r = await cancel_order(self.symbol, order_id)
            record["entry_cancelled"] = True
            return record

        record["filled"] = True
        record["entry_price"] = fill_price
        self.fills += 1

        # 4. Wait, then place exit LIMIT
        await asyncio.sleep(min(self.exit_timeout * 0.5, 15))  # hold half the timeout
        # Re-read TOB
        # (Simplest: use entry price ± expected move ~1bp)
        # Better: sample a fresh tick. We don't have easy access here, so just use entry +/- a safe offset.
        if side == "BUY":
            exit_side = "SELL"
            exit_px = str(round(fill_price * 1.0001, 2))  # slightly above entry
        else:
            exit_side = "BUY"
            exit_px = str(round(fill_price * 0.9999, 2))

        exit_r = await place_limit_order(self.symbol, exit_side, self.qty, exit_px)
        if exit_r.get("_error"):
            record["errors"].append(f"place_exit: {exit_r.get('_status')}")
            return record

        exit_order = exit_r.get("order", exit_r)
        exit_order_id = exit_order.get("orderId")
        record["exit_order_id"] = exit_order_id
        record["exit_intent_price"] = exit_px

        # Wait for exit
        exit_ts = time.time()
        exit_filled = False
        for _ in range(int(self.exit_timeout * 2)):
            await asyncio.sleep(0.5)
            pos = await get_position(self.symbol)
            if not pos or pos.get("positionQty") is None or float(pos.get("positionQty", 0)) == 0:
                exit_filled = True
                break

        record["hold_s"] = round(time.time() - exit_ts, 2)

        if not exit_filled:
            await cancel_order(self.symbol, exit_order_id)
            # Force close with market
            mkt_r = await place_limit_order(
                self.symbol, exit_side, self.qty,
                str(round(fill_price * (0.99 if side == "BUY" else 1.01), 2))
            )
            record["force_market_close"] = True
            await asyncio.sleep(2)

        # Compute PnL
        # If exit filled at exit_px, use that; if force-closed, would need history
        used_exit = float(exit_px) if exit_filled else float(fill_price)  # rough
        record["exit_filled"] = exit_filled
        record["exit_price"] = used_exit
        sign = 1 if side == "BUY" else -1
        record["pnl_bp"] = round((used_exit - fill_price) / fill_price * 1e4 * sign, 3)

        return record


async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--symbol", default="ETHUSDT.PERP")
    p.add_argument("--qty", default="0.01", help="Quantity in base coin (e.g. 0.01 ETH)")
    p.add_argument("--threshold", type=float, default=0.7)
    p.add_argument("--max-attempts", type=int, default=30)
    p.add_argument("--cooldown", type=float, default=8.0)
    args = p.parse_args()

    t = Tester(
        symbol=args.symbol,
        qty=args.qty,
        imb_threshold=args.threshold,
        max_attempts=args.max_attempts,
        cooldown_sec=args.cooldown,
    )
    await t.run()


if __name__ == "__main__":
    asyncio.run(main())

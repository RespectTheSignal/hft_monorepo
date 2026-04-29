"""Flipster private WebSocket position/margin tracker.

Connects to the private stream that the web UI uses. Maintains in-memory
state of open positions and available margin. Used by slow_statarb_v9 to
track late fills and avoid NotEnoughBalance errors.
"""

from __future__ import annotations

import asyncio
import json
from dataclasses import dataclass, field
from typing import Callable

import websockets


WS_URL = "wss://api.flipster.io/api/v2/stream/r230522?mode=subscription"
SUBSCRIBE = {
    "s": {
        "private/positions": {"rows": ["*"]},
        "private/orders":    {"rows": ["*"]},
        "private/margins":   {"rows": ["*"]},
    }
}


@dataclass
class WSPositionTracker:
    """Maintains real-time state of all open positions via Flipster's
    private WebSocket. Fields per position (per WS protocol):
        effectiveLeverage, initPositionMargin, initMarginReserved,
        unrealizedPnl, netPnl, midPrice, bidPrice, askPrice
    """
    cookies: dict
    positions: dict = field(default_factory=dict)   # "SYMBOL/SLOT" -> {fields}
    orders: dict = field(default_factory=dict)      # "ORDERID" -> {fields}
    margin_avail: float = 0.0
    margin_balance: float = 0.0
    nonbonus_collateral: float = 0.0
    connected: bool = False

    def open_symbols(self) -> set[str]:
        """Return base symbols (no .PERP) currently with open position."""
        out = set()
        for key in self.positions:
            sym = key.split("/")[0]  # "BTCUSDT.PERP"
            out.add(sym)
        return out

    def has_position(self, flip_sym: str, slot: int = 0) -> bool:
        key = f"{flip_sym}/{slot}"
        if key not in self.positions: return False
        margin = self.positions[key].get("initMarginReserved", "0")
        return float(margin) > 0

    def position_fields(self, flip_sym: str, slot: int = 0) -> dict | None:
        return self.positions.get(f"{flip_sym}/{slot}")

    def position_side_qty_entry(self, flip_sym: str, slot: int = 0):
        """Return (side, qty, entry_price) where side is 'Long'/'Short'.
        Returns None if snapshot data not yet received.
        """
        f = self.positions.get(f"{flip_sym}/{slot}")
        if not f: return None
        pos_str = f.get("position")
        ep = f.get("avgEntryPrice")
        if pos_str is None or ep is None: return None
        try:
            qty = float(pos_str)
            entry = float(ep)
        except (TypeError, ValueError):
            return None
        if qty == 0: return None
        return ("Long" if qty > 0 else "Short", abs(qty), entry)

    async def run(self, log: Callable[[str], None]):
        cookie_hdr = "; ".join(f"{k}={v}" for k, v in self.cookies.items())
        headers = [
            ("Origin", "https://flipster.io"),
            ("Cookie", cookie_hdr),
            ("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 "
                           "(KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36"),
        ]
        while True:
            try:
                async with websockets.connect(
                    WS_URL, additional_headers=headers, max_size=10_000_000,
                    ping_interval=20, ping_timeout=10,
                ) as ws:
                    await ws.send(json.dumps(SUBSCRIBE))
                    self.connected = True
                    log(f"[priv-ws] connected & subscribed")
                    async for raw in ws:
                        if raw == "ping":
                            await ws.send("pong"); continue
                        try:
                            msg = json.loads(raw)
                        except Exception:
                            continue
                        t = msg.get("t", {})
                        # Positions update
                        if "private/positions" in t:
                            for sym_slot, fields in t["private/positions"].get("u", {}).items():
                                if sym_slot not in self.positions:
                                    self.positions[sym_slot] = {}
                                self.positions[sym_slot].update(fields)
                                # Closed?
                                margin = self.positions[sym_slot].get("initMarginReserved")
                                if margin is not None and float(margin) == 0:
                                    self.positions.pop(sym_slot, None)
                            for sym_slot in t["private/positions"].get("d", []):
                                self.positions.pop(sym_slot, None)
                        # Orders update
                        if "private/orders" in t:
                            for oid, fields in t["private/orders"].get("u", {}).items():
                                if oid not in self.orders:
                                    self.orders[oid] = {}
                                self.orders[oid].update(fields)
                            for oid in t["private/orders"].get("d", []):
                                self.orders.pop(oid, None)
                        # Margin / wallet
                        if "private/margins" in t:
                            usdt = t["private/margins"].get("u", {}).get("USDT", {})
                            if "crossMarginAvailable" in usdt:
                                self.margin_avail = float(usdt["crossMarginAvailable"])
                            if "marginBalance" in usdt:
                                self.margin_balance = float(usdt["marginBalance"])
                            if "nonBonusCollateral" in usdt:
                                self.nonbonus_collateral = float(usdt["nonBonusCollateral"])
            except Exception as e:
                self.connected = False
                log(f"[priv-ws] err: {type(e).__name__}: {str(e)[:120]}; reconnect 3s")
                await asyncio.sleep(3)


def cookies_from_chrome(cdp_port: int = 9230) -> dict:
    """Extract cookies from running Chrome via CDP (sync)."""
    import urllib.request
    resp = urllib.request.urlopen(f"http://localhost:{cdp_port}/json/version")
    cdp_url = json.loads(resp.read())["webSocketDebuggerUrl"]
    return asyncio.get_event_loop().run_until_complete(_fetch_cookies(cdp_url))


async def _fetch_cookies(cdp_ws_url: str) -> dict:
    async with websockets.connect(cdp_ws_url, max_size=10_000_000) as ws:
        await ws.send(json.dumps({"id": 1, "method": "Storage.getCookies"}))
        resp = json.loads(await ws.recv())
        return {c["name"]: c["value"] for c in resp["result"]["cookies"]
                if "flipster" in c.get("domain", "")}


# Quick standalone test: python3 positions_ws.py
if __name__ == "__main__":
    async def _main():
        cookies = await _fetch_cookies(json.loads(__import__("urllib.request").request.urlopen(
            "http://localhost:9230/json/version").read())["webSocketDebuggerUrl"])
        t = WSPositionTracker(cookies=cookies)
        loop_task = asyncio.create_task(t.run(print))
        for i in range(20):
            await asyncio.sleep(2)
            print(f"[{i*2}s] open={len(t.positions)} avail=${t.margin_avail:.2f} "
                  f"unrealized=positions: {sorted(t.positions.keys())}")
        loop_task.cancel()
    asyncio.run(_main())

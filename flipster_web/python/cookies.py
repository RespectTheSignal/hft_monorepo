"""Extract session cookies from a running Chrome instance via CDP."""

from __future__ import annotations

import asyncio
import json
from typing import Optional

import websockets


async def _fetch_cookies(ws_url: str) -> dict[str, str]:
    async with websockets.connect(ws_url) as ws:
        await ws.send(json.dumps({"id": 1, "method": "Storage.getCookies"}))
        resp = json.loads(await ws.recv())
        all_cookies = resp.get("result", {}).get("cookies", [])
        return {
            c["name"]: c["value"]
            for c in all_cookies
            if "flipster" in c.get("domain", "")
        }


def extract_cookies(cdp_ws_url: str) -> dict[str, str]:
    """Synchronous wrapper — returns {cookie_name: cookie_value} for flipster domain.

    Raises RuntimeError if no flipster cookies found (user probably not logged in).
    """
    cookies = asyncio.get_event_loop().run_until_complete(
        _fetch_cookies(cdp_ws_url)
    )
    if not cookies or "session_id_bolts" not in cookies:
        raise RuntimeError(
            "No Flipster session cookies found. "
            "Log in via the VNC browser first."
        )
    return cookies

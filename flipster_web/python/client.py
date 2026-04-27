"""FlipsterClient — browser-backed order execution for Flipster exchange."""

from __future__ import annotations

import json
from typing import Optional

import requests

from .browser import BrowserManager
from .cookies import extract_cookies
from .order import OrderParams, OrderType

API_BASE = "https://api.flipster.io"

_HEADERS = {
    "User-Agent": (
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:149.0) "
        "Gecko/20100101 Firefox/149.0"
    ),
    "Accept": "application/json, text/plain, */*",
    "Accept-Language": "en-US",
    "Referer": "https://flipster.io/",
    "Content-Type": "application/json",
    "X-Prex-Client-Platform": "web",
    "X-Prex-Client-Version": "release-web-3.15.110",
    "Origin": "https://flipster.io",
    "Sec-GPC": "1",
    "Sec-Fetch-Dest": "empty",
    "Sec-Fetch-Mode": "cors",
    "Sec-Fetch-Site": "same-site",
}


class FlipsterClient:
    """High-level client: manages browser, cookies, and order placement."""

    def __init__(self, browser: Optional[BrowserManager] = None,
                 proxies: Optional[list[str]] = None):
        """proxies: optional list of "ip:port:user:pass" strings; rotated per request."""
        self._browser = browser or BrowserManager()
        self._session: Optional[requests.Session] = None
        self._cookies: dict[str, str] = {}
        self._proxies = proxies or []
        self._proxy_idx = 0

    def _next_proxy(self) -> Optional[dict]:
        if not self._proxies:
            return None
        spec = self._proxies[self._proxy_idx % len(self._proxies)]
        self._proxy_idx += 1
        ip, port, user, pwd = spec.split(":")
        url = f"http://{user}:{pwd}@{ip}:{port}"
        return {"http": url, "https": url}

    # -- lifecycle ------------------------------------------------------------

    def start_browser(self) -> str:
        """Start VNC + Chrome. Returns noVNC URL for login."""
        return self._browser.start()

    def login_done(self):
        """Call after the user has logged in via VNC. Extracts cookies and
        prepares the HTTP session for order placement."""
        ws_url = self._browser.cdp_ws_url
        self._cookies = extract_cookies(ws_url)
        self._session = requests.Session()
        self._session.cookies.update(self._cookies)
        self._session.headers.update(_HEADERS)

    def refresh_cookies(self):
        """Re-extract cookies (e.g. after __cf_bm expires ~30min)."""
        ws_url = self._browser.cdp_ws_url
        self._cookies = extract_cookies(ws_url)
        self._session.cookies.update(self._cookies)

    def stop(self):
        """Stop browser and all services."""
        self._browser.stop()
        if self._session:
            self._session.close()
            self._session = None

    # -- orders ---------------------------------------------------------------

    def place_order(
        self,
        symbol: str,
        params: OrderParams,
        ref_price: Optional[float] = None,
    ) -> dict:
        """Place an order. Returns the parsed API response.

        ref_price: reference price for the order. Required for market orders
        if params.price is None. For limit orders, params.price is used.
        """
        if self._session is None:
            raise RuntimeError("Call login_done() first")

        price = params.price if params.price is not None else ref_price
        if price is None:
            raise ValueError(
                "ref_price is required for market orders (or set params.price)"
            )

        body = params.to_body(ref_price=price)
        url = f"{API_BASE}/api/v2/trade/positions/{symbol}"

        resp = self._session.post(url, json=body, proxies=self._next_proxy())
        status = resp.status_code

        if status in (401, 403):
            raise PermissionError(
                f"Auth failed ({status}). Session may have expired. "
                "Try refresh_cookies() or re-login."
            )

        data = resp.json()

        if status >= 400:
            raise RuntimeError(f"API error {status}: {json.dumps(data)}")

        return data

    def close_position(
        self,
        symbol: str,
        slot: int,
        price: float,
    ) -> dict:
        """Close a position by setting size=0.

        PUT /api/v2/trade/positions/{symbol}/{slot}/size
        """
        if self._session is None:
            raise RuntimeError("Call login_done() first")

        body = OrderParams.close_body(price)
        url = f"{API_BASE}/api/v2/trade/positions/{symbol}/{slot}/size"

        resp = self._session.put(url, json=body, proxies=self._next_proxy())
        status = resp.status_code

        if status in (401, 403):
            raise PermissionError(
                f"Auth failed ({status}). Session may have expired."
            )

        data = resp.json()

        if status >= 400:
            raise RuntimeError(f"API error {status}: {json.dumps(data)}")

        return data

    def cancel_order(self, symbol: str, order_id: str) -> dict | None:
        """Cancel a pending limit order.

        DELETE /api/v2/trade/orders/{symbol}/{orderId}  with JSON body
        {requestId, timestamp}. Body is required — without it the API
        silently rejects.
        Returns parsed JSON on success, raises on auth/server errors.
        """
        if self._session is None:
            raise RuntimeError("Call login_done() first")
        import time as _time, uuid as _uuid
        url = f"{API_BASE}/api/v2/trade/orders/{symbol}/{order_id}"
        body = {
            "requestId": str(_uuid.uuid4()),
            "timestamp": str(_time.time_ns()),
        }
        resp = self._session.delete(url, json=body, proxies=self._next_proxy())
        if resp.status_code in (401, 403):
            raise PermissionError(f"Auth failed ({resp.status_code})")
        try:
            data = resp.json()
        except Exception:
            data = {"raw": resp.text[:200]}
        if resp.status_code >= 400:
            raise RuntimeError(f"cancel API error {resp.status_code}: {json.dumps(data)[:200]}")
        return data

    def raw_request(self, symbol: str, body: dict) -> dict:
        """Send a raw JSON body to the order endpoint (for debugging)."""
        if self._session is None:
            raise RuntimeError("Call login_done() first")

        url = f"{API_BASE}/api/v2/trade/positions/{symbol}"
        resp = self._session.post(url, json=body)

        if resp.status_code in (401, 403):
            raise PermissionError(f"Auth failed ({resp.status_code})")

        return resp.json()

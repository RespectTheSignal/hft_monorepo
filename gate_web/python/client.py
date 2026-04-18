"""GateClient — browser-backed order execution for Gate.io futures."""

from __future__ import annotations

import json
import time
from typing import Optional

import requests

from .browser import BrowserManager
from .cookies import extract_cookies
from .order import OrderParams

API_BASE = "https://www.gate.com"
ORDER_URL = f"{API_BASE}/apiw/v2/futures/usdt/orders"
USER_INFO_URL = f"{API_BASE}/api/web/v1/usercenter/get_info"

_HEADERS = {
    "User-Agent": (
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
        "AppleWebKit/537.36 (KHTML, like Gecko) "
        "Chrome/120.0.0.0 Safari/537.36"
    ),
    "Accept": "application/json",
    "Content-Type": "application/json",
    "X-Gate-Applang": "en",
    "X-Gate-Device-Type": "0",
}


class GateClient:
    """High-level client: manages browser, cookies, and order placement."""

    def __init__(self, browser: Optional[BrowserManager] = None):
        self._browser = browser or BrowserManager()
        self._session: Optional[requests.Session] = None
        self._cookies: dict[str, str] = {}

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
        """Re-extract cookies from the browser."""
        ws_url = self._browser.cdp_ws_url
        self._cookies = extract_cookies(ws_url)
        self._session.cookies.update(self._cookies)

    def stop(self):
        """Stop browser and all services."""
        self._browser.stop()
        if self._session:
            self._session.close()
            self._session = None

    # -- account info ---------------------------------------------------------

    def check_login(self) -> dict:
        """Check login status via Gate.io user info API.

        Returns the parsed JSON response. ``data.code == 200`` means logged in.
        """
        if self._session is None:
            raise RuntimeError("Call login_done() first")

        resp = self._session.get(USER_INFO_URL)
        return resp.json()

    def set_leverage(self, contract: str, leverage: int = 0,
                     cross_leverage_limit: int = 10) -> dict:
        """Set leverage for a contract.

        leverage=0 → CROSS mode with cross_leverage_limit as cap.
        leverage>0 → ISOLATED mode with that leverage.

        e.g. set_leverage("BTC_USDT", leverage=0, cross_leverage_limit=10)
             → cross 10x for BTC_USDT
        """
        if self._session is None:
            raise RuntimeError("Call login_done() first")
        url = (f"{API_BASE}/apiw/v2/futures/usdt/positions/{contract}/leverage"
               "?sub_website_id=0")
        body = {
            "leverage": str(leverage),
            "cross_leverage_limit": str(cross_leverage_limit),
        }
        resp = self._session.post(url, json=body)
        try:
            return resp.json()
        except Exception:
            return {"status": resp.status_code, "text": resp.text[:200]}

    def is_logged_in(self) -> bool:
        """Convenience: True if session cookies are valid."""
        try:
            data = self.check_login()
            # Gate returns code=0, message="Success" when logged in
            return data.get("code") == 0 and data.get("message", "").lower() == "success"
        except Exception:
            return False

    # -- orders ---------------------------------------------------------------

    def place_order(self, contract: str, params: OrderParams) -> dict:
        """Place a futures order.

        contract: e.g. "BTC_USDT"
        Returns the parsed API response.
        """
        if self._session is None:
            raise RuntimeError("Call login_done() first")

        body = params.to_body(contract)
        t0 = time.time()
        resp = self._session.post(ORDER_URL, json=body)
        elapsed_ms = (time.time() - t0) * 1000
        status = resp.status_code

        try:
            data = resp.json()
        except Exception:
            raise RuntimeError(
                f"Non-JSON response ({status}): {resp.text[:500]}"
            )

        # Check for rate limiting
        label = data.get("label", "")
        if label == "TOO_MANY_REQUEST" or status == 429:
            raise RuntimeError(
                f"Rate limited (TOO_MANY_REQUEST). Wait before retrying. "
                f"Response: {json.dumps(data)}"
            )

        if status in (401, 403):
            raise PermissionError(
                f"Auth failed ({status}). Session may have expired. "
                "Try refresh_cookies() or re-login."
            )

        # Gate success: status 200 AND message == "success"
        is_success = (
            status == 200
            and data.get("message") == "success"
        ) or label == "ErrorOrderFok"

        return {
            "ok": is_success,
            "status": status,
            "data": data,
            "elapsed_ms": round(elapsed_ms, 1),
        }

    def close_position(
        self,
        contract: str,
        size: int,
        price: Optional[float] = None,
    ) -> dict:
        """Close a position with a reduce-only order.

        contract: e.g. "BTC_USDT"
        size: positive = close short (buy back), negative = close long (sell).
        price: limit price, or None for market.
        """
        if self._session is None:
            raise RuntimeError("Call login_done() first")

        body = OrderParams.close_body(contract, size, price)
        t0 = time.time()
        resp = self._session.post(ORDER_URL, json=body)
        elapsed_ms = (time.time() - t0) * 1000

        try:
            data = resp.json()
        except Exception:
            raise RuntimeError(
                f"Non-JSON response ({resp.status_code}): {resp.text[:500]}"
            )

        is_success = (
            resp.status_code == 200
            and data.get("message") == "success"
        )

        return {
            "ok": is_success,
            "status": resp.status_code,
            "data": data,
            "elapsed_ms": round(elapsed_ms, 1),
        }

    def raw_request(self, body: dict) -> dict:
        """Send a raw JSON body to the order endpoint (for debugging)."""
        if self._session is None:
            raise RuntimeError("Call login_done() first")

        resp = self._session.post(ORDER_URL, json=body)

        if resp.status_code in (401, 403):
            raise PermissionError(f"Auth failed ({resp.status_code})")

        return resp.json()

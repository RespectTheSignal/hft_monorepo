"""Extract Flipster + Gate session cookies from running Chrome instances
via CDP and write them to a JSON file the Rust executor can read.

Run on teamreporter (where Chrome CDP is live on :9230 / :9231).
Intended for cron / systemd timer every ~20 min so the Rust binary
always sees fresh cookies.

Usage:
    python3 scripts/dump_cookies.py [--out PATH]

Default output: ~/.config/flipster_kattpish/cookies.json
Atomic write (tmp + rename) so a partial file is never observed.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

import websockets


FLIPSTER_CDP_URL = "http://localhost:9230/json/version"
GATE_CDP_URL = "http://localhost:9231/json/version"
BINGX_CDP_URL = "http://localhost:9232/json/version"
DEFAULT_OUT = Path.home() / ".config" / "flipster_kattpish" / "cookies.json"


async def _fetch_cookies(ws_url: str, domain_substr: str) -> dict[str, str]:
    async with websockets.connect(ws_url) as ws:
        await ws.send(json.dumps({"id": 1, "method": "Storage.getCookies"}))
        resp = json.loads(await ws.recv())
        all_cookies = resp.get("result", {}).get("cookies", [])
        return {
            c["name"]: c["value"]
            for c in all_cookies
            if domain_substr in c.get("domain", "")
        }


def _cdp_ws_url(http_endpoint: str) -> str:
    resp = urllib.request.urlopen(http_endpoint, timeout=3)
    return json.loads(resp.read())["webSocketDebuggerUrl"]


async def _dump() -> dict:
    # BingX is optional — only dump if its CDP endpoint is up. Avoids breaking
    # existing Flipster/Gate flows on hosts that haven't started bingx Chrome.
    flipster_ws = _cdp_ws_url(FLIPSTER_CDP_URL)
    gate_ws = _cdp_ws_url(GATE_CDP_URL)
    bingx_ws = None
    try:
        bingx_ws = _cdp_ws_url(BINGX_CDP_URL)
    except Exception as e:
        print(f"[dump_cookies] bingx CDP not reachable, skipping: {e}", file=sys.stderr)

    tasks = [
        _fetch_cookies(flipster_ws, "flipster"),
        _fetch_cookies(gate_ws, "gate.com"),
    ]
    if bingx_ws:
        tasks.append(_fetch_cookies(bingx_ws, "bingx.com"))
    results = await asyncio.gather(*tasks)
    flipster = results[0]
    gate = results[1]
    bingx = results[2] if bingx_ws else {}

    if "session_id_bolts" not in flipster:
        raise RuntimeError(
            "No Flipster session cookies (session_id_bolts missing). "
            "Log in via the VNC browser at :6090."
        )
    if not gate:
        raise RuntimeError(
            "No Gate cookies. Log in via the VNC browser."
        )
    if bingx_ws and not bingx:
        # Don't fail hard — bingx may legitimately not be logged in yet on
        # some hosts. The Rust executor will log a warning if cookies are
        # absent when a BingX trade signal arrives.
        print(
            "[dump_cookies] warning: bingx Chrome up but no bingx.com cookies "
            "found. Log in to https://bingx.com via VNC.",
            file=sys.stderr,
        )

    return {
        "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "flipster": flipster,
        "gate": gate,
        "bingx": bingx,
    }


def _atomic_write(out_path: Path, data: dict) -> None:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(
        prefix=".cookies.", suffix=".tmp", dir=str(out_path.parent)
    )
    try:
        with os.fdopen(fd, "w") as f:
            json.dump(data, f, indent=2)
            f.flush()
            os.fsync(f.fileno())
        os.chmod(tmp, 0o600)
        os.replace(tmp, out_path)
    except Exception:
        try:
            os.unlink(tmp)
        except FileNotFoundError:
            pass
        raise


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out", type=Path, default=DEFAULT_OUT)
    p.add_argument("--quiet", action="store_true", help="suppress success line")
    args = p.parse_args()

    try:
        data = asyncio.run(_dump())
    except Exception as e:
        print(f"[dump_cookies] ERROR: {e}", file=sys.stderr)
        return 1

    _atomic_write(args.out, data)
    if not args.quiet:
        print(
            f"[dump_cookies] {args.out} flipster={len(data['flipster'])} "
            f"gate={len(data['gate'])} bingx={len(data.get('bingx', {}))} "
            f"ts={data['ts']}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())

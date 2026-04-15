"""Flipster /api/v1/market/contract 전용 테스트.

환경변수 필요:
    FLIPSTER_API_KEY, FLIPSTER_API_SECRET

실행:
    set -a && . ./.env && set +a && PYTHONPATH=src .venv/bin/python scripts/test_contract_endpoint.py

반복 샘플링:
    CONTRACT_TEST_REPEAT=5 CONTRACT_TEST_INTERVAL_SEC=60 \
      set -a && . ./.env && set +a && PYTHONPATH=src .venv/bin/python scripts/test_contract_endpoint.py

추가 심볼:
    CONTRACT_TEST_SYMBOLS=ETHUSDT.PERP,BTCUSDT.PERP,SOLUSDT.PERP \
      set -a && . ./.env && set +a && PYTHONPATH=src .venv/bin/python scripts/test_contract_endpoint.py
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import time
from pathlib import Path

import httpx

sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

from strategy_flipster.execution.auth import make_auth_headers


BASE_URL = "https://trading-api.flipster.io"


def _symbols() -> list[str]:
    raw = os.environ.get("CONTRACT_TEST_SYMBOLS", "ETHUSDT.PERP,BTCUSDT.PERP")
    return [s.strip().upper() for s in raw.split(",") if s.strip()]


async def _fetch(client: httpx.AsyncClient, path: str) -> None:
    headers = make_auth_headers(
        os.environ["FLIPSTER_API_KEY"],
        os.environ["FLIPSTER_API_SECRET"],
        "GET",
        path,
    )
    response = await client.get(path, headers=headers)
    try:
        payload = response.json()
    except Exception:
        payload = {"raw": response.text[:500]}

    body_symbol = payload.get("symbol") if isinstance(payload, dict) else None
    interesting_headers = {
        key: response.headers.get(key)
        for key in ("date", "age", "cf-cache-status", "cache-control", "etag", "vary")
        if response.headers.get(key) is not None
    }

    print(f"  PATH   : {path}")
    print(f"  STATUS : {response.status_code}")
    print(f"  HEADERS: {json.dumps(interesting_headers, ensure_ascii=False)}")
    print(f"  SYMBOL : {body_symbol}")
    print(f"  BODY   : {json.dumps(payload, ensure_ascii=False)}")


async def main() -> None:
    api_key = os.environ.get("FLIPSTER_API_KEY", "")
    api_secret = os.environ.get("FLIPSTER_API_SECRET", "")
    if not api_key or not api_secret:
        print("ERROR: FLIPSTER_API_KEY/FLIPSTER_API_SECRET 환경변수 필요")
        sys.exit(1)

    repeat = int(os.environ.get("CONTRACT_TEST_REPEAT", "1"))
    interval_sec = float(os.environ.get("CONTRACT_TEST_INTERVAL_SEC", "60"))
    paths = [f"/api/v1/market/contract?symbol={symbol}" for symbol in _symbols()]
    paths.append("/api/v1/market/contract")

    async with httpx.AsyncClient(base_url=BASE_URL, timeout=10.0) as client:
        for idx in range(repeat):
            print(f"\n=== SAMPLE {idx + 1}/{repeat} {time.strftime('%Y-%m-%d %H:%M:%S', time.gmtime())} UTC ===")
            for path in paths:
                await _fetch(client, path)
                print("-" * 80)
            if idx < repeat - 1:
                await asyncio.sleep(interval_sec)


if __name__ == "__main__":
    asyncio.run(main())

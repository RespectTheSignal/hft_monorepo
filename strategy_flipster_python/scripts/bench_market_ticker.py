"""/api/v1/market/ticker 레이턴시 분해 측정.

20회 반복:
  - 총 소요 시간
  - HTTP wall time (resp 수신까지)
  - JSON 파싱 시간
  - 응답 바이트 크기
  - 반환 티커 수

실행: set -a && . ./.env && set +a && uv run python scripts/bench_market_ticker.py
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

import httpx

from strategy_flipster.execution.auth import make_auth_headers

BASE = "https://trading-api.flipster.io"
PATH = "/api/v1/market/ticker"


async def main() -> None:
    api_key = os.environ.get("FLIPSTER_API_KEY", "")
    api_secret = os.environ.get("FLIPSTER_API_SECRET", "")
    if not api_key or not api_secret:
        print("FLIPSTER_API_KEY/SECRET 필요"); sys.exit(1)

    async with httpx.AsyncClient(base_url=BASE, timeout=10.0) as client:
        print(f"{'#':>3}  {'total(ms)':>10}  {'http(ms)':>10}  {'json(ms)':>10}  {'resp(KB)':>10}  {'n':>5}")
        print("-" * 70)
        totals: list[float] = []
        https: list[float] = []
        jsons: list[float] = []
        for i in range(20):
            t0 = time.perf_counter_ns()
            headers = make_auth_headers(api_key, api_secret, "GET", PATH)
            t_auth = time.perf_counter_ns()
            resp = await client.get(PATH, headers=headers)
            t_http = time.perf_counter_ns()
            body = resp.content
            t_read = time.perf_counter_ns()
            data = json.loads(body)
            t_json = time.perf_counter_ns()

            total_ms = (t_json - t0) / 1e6
            http_ms = (t_http - t_auth) / 1e6
            json_ms = (t_json - t_read) / 1e6
            size_kb = len(body) / 1024
            n = len(data) if isinstance(data, list) else 1
            print(f"{i:>3}  {total_ms:>10.1f}  {http_ms:>10.1f}  {json_ms:>10.1f}  {size_kb:>10.1f}  {n:>5}")
            totals.append(total_ms)
            https.append(http_ms)
            jsons.append(json_ms)
            await asyncio.sleep(0.3)

        def stats(xs: list[float]) -> tuple[float, float, float, float]:
            xs_sorted = sorted(xs)
            return (
                min(xs), max(xs),
                sum(xs) / len(xs),
                xs_sorted[len(xs) // 2],
            )

        print("-" * 70)
        print(f"{'total':>15}  min={stats(totals)[0]:.1f}  max={stats(totals)[1]:.1f}  avg={stats(totals)[2]:.1f}  med={stats(totals)[3]:.1f}")
        print(f"{'http':>15}  min={stats(https)[0]:.1f}  max={stats(https)[1]:.1f}  avg={stats(https)[2]:.1f}  med={stats(https)[3]:.1f}")
        print(f"{'json':>15}  min={stats(jsons)[0]:.1f}  max={stats(jsons)[1]:.1f}  avg={stats(jsons)[2]:.1f}  med={stats(jsons)[3]:.1f}")


if __name__ == "__main__":
    asyncio.run(main())

"""실 ZMQ 스트림에서 거래소별 고유 심볼 수집 (10초).

실행:
    uv run python scripts/probe_symbols.py
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

from strategy_flipster.market_data.flipster_zmq import FlipsterZmqFeed
from strategy_flipster.market_data.zmq_feed import ExchangeZmqFeed


async def collect(feed: object, label: str, duration: float, out: set[str]) -> int:
    await feed.connect()  # type: ignore[attr-defined]
    count = 0
    try:
        loop = asyncio.get_running_loop()
        deadline = loop.time() + duration
        while loop.time() < deadline:
            try:
                ticker = await asyncio.wait_for(feed.recv(), timeout=deadline - loop.time())  # type: ignore[attr-defined]
            except asyncio.TimeoutError:
                break
            if ticker is None:
                break
            out.add(ticker.symbol)
            count += 1
    finally:
        await feed.disconnect()  # type: ignore[attr-defined]
    print(f"[{label}] {count} msgs, {len(out)} unique symbols")
    return count


async def main() -> None:
    binance_syms: set[str] = set()
    flipster_syms: set[str] = set()

    bn = ExchangeZmqFeed(zmq_address="tcp://211.181.122.3:6000", exchange_name="binance")
    fl = FlipsterZmqFeed(zmq_address="tcp://211.181.122.104:7000")

    await asyncio.gather(
        collect(bn, "binance", 10.0, binance_syms),
        collect(fl, "flipster", 10.0, flipster_syms),
    )

    print(f"\n=== Binance 상위 10개 심볼 ===")
    for s in sorted(binance_syms)[:10]:
        print(f"  {s}")

    print(f"\n=== Flipster 상위 10개 심볼 ===")
    for s in sorted(flipster_syms)[:10]:
        print(f"  {s}")

    # BTC 관련만
    bn_btc = sorted(s for s in binance_syms if "BTC" in s)
    fl_btc = sorted(s for s in flipster_syms if "BTC" in s)
    print(f"\n=== BTC 포함 ===")
    print(f"Binance: {bn_btc[:10]}")
    print(f"Flipster: {fl_btc[:10]}")

    # 교집합 후보 — 간단한 정규화
    def normalize(s: str) -> str:
        return s.replace(".PERP", "").replace("-USDT-PERP", "USDT").replace("_", "").upper()

    bn_norm = {normalize(s): s for s in binance_syms}
    fl_norm = {normalize(s): s for s in flipster_syms}
    common = set(bn_norm.keys()) & set(fl_norm.keys())
    print(f"\n=== 정규화 후 공통 심볼: {len(common)}개 ===")
    for n in sorted(common)[:10]:
        print(f"  {n:12s} bn={bn_norm[n]:20s} fl={fl_norm[n]}")


if __name__ == "__main__":
    asyncio.run(main())

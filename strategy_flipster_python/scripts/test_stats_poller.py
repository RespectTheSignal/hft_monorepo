"""MarketStatsPoller 스모크 테스트.

환경변수 필요: FLIPSTER_API_KEY, FLIPSTER_API_SECRET

실행:
    set -a && . ./.env && set +a && uv run python scripts/test_stats_poller.py
"""

from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

from strategy_flipster.config import FlipsterApiConfig
from strategy_flipster.execution.rest_client import FlipsterExecutionClient
from strategy_flipster.market_data.stats_cache import MarketStatsCache
from strategy_flipster.market_data.stats_poller import FlipsterMarketStatsPoller


async def main() -> None:
    api_key = os.environ.get("FLIPSTER_API_KEY", "")
    api_secret = os.environ.get("FLIPSTER_API_SECRET", "")
    if not api_key or not api_secret:
        print("ERROR: FLIPSTER_API_KEY/SECRET 환경변수 필요")
        sys.exit(1)

    config = FlipsterApiConfig(api_key=api_key, api_secret=api_secret)
    client = FlipsterExecutionClient(config)
    cache = MarketStatsCache()
    poller = FlipsterMarketStatsPoller(client=client, cache=cache, interval_sec=2.0)

    await client.start()
    try:
        print("=== 1회 풀링 ===")
        count = await poller.poll_once()
        print(f"  수집: {count}개 심볼")
        print(f"  캐시 크기: {len(cache)}")

        print("\n=== 상위 5개 심볼 ===")
        for sym in list(cache.symbols())[:5]:
            s = cache.get(sym)
            if s is None:
                continue
            print(
                f"  {s.symbol:20s} mid={(s.bid_price + s.ask_price)/2:>10.4f} "
                f"mark={s.mark_price:>10.4f} fr={s.funding_rate:>+.6f} "
                f"OI={s.open_interest:>15.2f} vol24h={s.volume_24h:>15.2f}"
            )

        # BTCUSDT.PERP 상세
        btc = cache.get("BTCUSDT.PERP")
        if btc is not None:
            print("\n=== BTCUSDT.PERP 상세 ===")
            print(f"  bid/ask:       {btc.bid_price} / {btc.ask_price}")
            print(f"  last:          {btc.last_price}")
            print(f"  mark/index:    {btc.mark_price} / {btc.index_price}")
            print(f"  funding_rate:  {btc.funding_rate}")
            print(f"  next_funding:  {btc.next_funding_time_ns} ns")
            print(f"  open_interest: {btc.open_interest}")
            print(f"  volume_24h:    {btc.volume_24h}")
            print(f"  turnover_24h:  {btc.turnover_24h}")
            print(f"  change_24h:    {btc.price_change_24h}")

        print("\n=== 2회째 풀링 (2초 후) ===")
        await asyncio.sleep(2.0)
        count2 = await poller.poll_once()
        print(f"  수집: {count2}개 심볼")

    finally:
        await client.stop()


if __name__ == "__main__":
    asyncio.run(main())

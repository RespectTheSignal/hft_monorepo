"""통합 테스트 — Flipster ZMQ + Strategy + 전체 파이프라인.

API 키 없이도 market data 파이프라인은 테스트 가능.
API 키 있으면 user data + execution까지 테스트.

사용법:
    # market data만 (API 키 불필요)
    python scripts/test_integration.py market

    # 전체 (API 키 필요)
    python scripts/test_integration.py full

    # 기본: market
    python scripts/test_integration.py
"""

from __future__ import annotations

import asyncio
import os
import sys
import time

sys.path.insert(0, "src")

from strategy_flipster.market_data.aggregator import MarketDataAggregator
from strategy_flipster.market_data.flipster_zmq import FlipsterZmqFeed
from strategy_flipster.strategy.sample import SampleStrategy
from strategy_flipster.types import BookTicker
from strategy_flipster.user_data.state import UserState

FLIPSTER_ZMQ = "tcp://211.181.122.104:7000"


async def test_market_pipeline() -> None:
    """Market Data → Aggregator → Strategy 파이프라인 테스트"""
    print("=" * 60)
    print("Market Data Pipeline 테스트")
    print("=" * 60)

    feed = FlipsterZmqFeed(zmq_address=FLIPSTER_ZMQ)
    agg = MarketDataAggregator([feed], queue_size=5000)
    strategy = SampleStrategy()
    state = UserState()

    await agg.start()
    await strategy.on_start(state)

    symbols: set[str] = set()
    count = 0
    order_count = 0
    t0 = time.monotonic()

    try:
        for _ in range(1000):
            try:
                ticker = await asyncio.wait_for(agg.recv(), timeout=0.01)
                count += 1
                symbols.add(ticker.symbol)
                orders = await strategy.on_book_ticker(ticker, state)
                order_count += len(orders)
            except asyncio.TimeoutError:
                # 타이머 트리거
                orders = await strategy.on_timer(state)
                order_count += len(orders)

            if count > 0 and count % 200 == 0:
                elapsed = time.monotonic() - t0
                rate = count / elapsed
                print(f"  [{count}] {rate:.0f} msg/s, {len(symbols)} symbols, q={agg.pending_count}")

    except KeyboardInterrupt:
        pass

    elapsed = time.monotonic() - t0
    rate = count / elapsed if elapsed > 0 else 0

    print(f"\n결과:")
    print(f"  수신: {count}개 / {elapsed:.1f}s / {rate:.0f} msg/s")
    print(f"  심볼: {len(symbols)}개")
    print(f"  주문 시그널: {order_count}개")
    print(f"  상태: OK")

    await strategy.on_stop()
    await agg.stop()


async def test_full_pipeline() -> None:
    """전체 파이프라인 테스트 (API 키 필요)"""
    from strategy_flipster.config import FlipsterApiConfig
    from strategy_flipster.execution.rest_client import FlipsterExecutionClient
    from strategy_flipster.user_data.rest_client import FlipsterUserRestClient

    api_key = os.environ.get("FLIPSTER_API_KEY", "")
    api_secret = os.environ.get("FLIPSTER_API_SECRET", "")

    if not api_key or not api_secret:
        print("FLIPSTER_API_KEY/FLIPSTER_API_SECRET 미설정 — user data 테스트 건너뜀")
        await test_market_pipeline()
        return

    config = FlipsterApiConfig(api_key=api_key, api_secret=api_secret)

    print("=" * 60)
    print("Full Pipeline 테스트 (Market + User + Execution)")
    print("=" * 60)

    # 1. User Data REST
    print("\n[1/3] User Data REST...")
    user_rest = FlipsterUserRestClient(config)
    await user_rest.start()

    state = UserState()
    try:
        account = await user_rest.get_account()
        state.update_account(account)
        print(f"  계정: balance={account.total_wallet_balance} available={account.available_balance}")

        positions = await user_rest.get_positions()
        active_pos = [p for p in positions if p.position_amount != 0]
        for p in positions:
            state.update_position(p)
        print(f"  포지션: {len(positions)}개 (활성 {len(active_pos)}개)")

        balances = await user_rest.get_balances()
        for b in balances:
            state.update_balance(b)
        print(f"  잔고: {len(balances)}개 자산")
    except Exception as e:
        print(f"  REST 에러: {e}")

    await user_rest.stop()

    # 2. Execution — 거래 가능 심볼 조회
    print("\n[2/3] Execution 클라이언트...")
    exec_client = FlipsterExecutionClient(config)
    await exec_client.start()

    try:
        symbols = await exec_client.get_tradable_symbols()
        perp_count = len(symbols.get("perpetual", []))
        spot_count = len(symbols.get("spot", []))
        print(f"  거래 가능: perpetual={perp_count}, spot={spot_count}")
    except Exception as e:
        print(f"  심볼 조회 에러: {e}")

    await exec_client.stop()

    # 3. Market Data + Strategy
    print("\n[3/3] Market Data Pipeline...")
    await test_market_pipeline()

    print("\n전체 통합 테스트 완료")


async def main() -> None:
    mode = sys.argv[1] if len(sys.argv) > 1 else "market"

    if mode == "full":
        await test_full_pipeline()
    else:
        await test_market_pipeline()


if __name__ == "__main__":
    asyncio.run(main())

"""Binance ZMQ 피드 실데이터 테스트 스크립트.

사용법:
    python scripts/test_binance_feed.py [zmq_address]

기본 주소: tcp://211.181.122.24:6000
"""

from __future__ import annotations

import asyncio
import sys
import time

# src 경로 추가
sys.path.insert(0, "src")

from strategy_flipster.market_data.binance_zmq import BinanceZmqFeed


async def main() -> None:
    address = sys.argv[1] if len(sys.argv) > 1 else "tcp://211.181.122.24:6000"
    print(f"Binance ZMQ 연결 중... {address}")

    feed = BinanceZmqFeed(zmq_address=address)
    await feed.connect()

    count = 0
    t0 = time.monotonic()
    symbols_seen: set[str] = set()

    try:
        while True:
            ticker = await feed.recv()
            if ticker is None:
                print("연결 종료됨")
                break

            count += 1
            symbols_seen.add(ticker.symbol)
            elapsed = time.monotonic() - t0

            if count <= 5 or count % 1000 == 0:
                latency_ms = (ticker.recv_ts_ns / 1_000_000) - ticker.event_time_ms
                print(
                    f"[{count:>6}] {ticker.exchange:>8} {ticker.symbol:<20} "
                    f"bid={ticker.bid_price:<12.4f} ask={ticker.ask_price:<12.4f} "
                    f"bid_sz={ticker.bid_size:<10.4f} ask_sz={ticker.ask_size:<10.4f} "
                    f"latency={latency_ms:.1f}ms  "
                    f"({elapsed:.1f}s, {len(symbols_seen)} symbols)"
                )

    except KeyboardInterrupt:
        elapsed = time.monotonic() - t0
        rate = count / elapsed if elapsed > 0 else 0
        print(f"\n총 {count}개 수신 / {elapsed:.1f}s / {rate:.0f} msg/s")
        print(f"심볼 {len(symbols_seen)}개: {sorted(symbols_seen)[:20]}...")

    finally:
        await feed.disconnect()


if __name__ == "__main__":
    asyncio.run(main())

"""Flipster IPC 피드 실데이터 테스트 스크립트.

사용법:
    python scripts/test_flipster_feed.py [socket_path]

기본 경로: /tmp/flipster_data_subscriber.sock

참고: data_subscriber가 로컬에서 실행 중이어야 함.
      원격 테스트 시 Flipster ZMQ PUB (211.181.122.104:7000)에 직접 연결하는
      test_flipster_zmq_feed.py 사용.
"""

from __future__ import annotations

import asyncio
import sys
import time

sys.path.insert(0, "src")

from strategy_flipster.market_data.flipster_ipc import FlipsterIpcFeed


async def main() -> None:
    socket_path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/flipster_data_subscriber.sock"
    print(f"Flipster IPC 연결 중... {socket_path}")

    feed = FlipsterIpcFeed(
        socket_path=socket_path,
        process_id="test_script",
        symbols=[],  # 모든 심볼
    )
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
                    f"[{count:>6}] {ticker.symbol:<20} "
                    f"bid={ticker.bid_price:<12.6f} ask={ticker.ask_price:<12.6f} "
                    f"last={ticker.last_price:<12.6f} mark={ticker.mark_price:<12.6f} "
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

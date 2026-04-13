"""Flipster ZMQ PUB 직접 연결 테스트.

data_subscriber 없이 Flipster publisher에 직접 ZMQ SUB으로 연결.
IPC와 달리 ZMQ multipart: [topic, 104B payload]

사용법:
    python scripts/test_flipster_zmq_feed.py [zmq_address]

기본 주소: tcp://211.181.122.104:7000
"""

from __future__ import annotations

import asyncio
import sys
import time

import zmq
import zmq.asyncio

sys.path.insert(0, "src")

from strategy_flipster.types import FLIPSTER_BT_SIZE, parse_flipster_bookticker


async def main() -> None:
    address = sys.argv[1] if len(sys.argv) > 1 else "tcp://211.181.122.104:7000"
    print(f"Flipster ZMQ 직접 연결 중... {address}")

    ctx = zmq.asyncio.Context()
    sock = ctx.socket(zmq.SUB)
    sock.setsockopt(zmq.RCVHWM, 100_000)
    sock.setsockopt(zmq.RCVBUF, 4 * 1024 * 1024)
    sock.subscribe(b"")
    sock.connect(address)

    count = 0
    t0 = time.monotonic()
    symbols_seen: set[str] = set()

    try:
        while True:
            parts = await sock.recv_multipart()
            if len(parts) != 2:
                print(f"예상 외 파트 수: {len(parts)}")
                continue

            topic_bytes, payload = parts[0], parts[1]

            if len(payload) != FLIPSTER_BT_SIZE:
                print(f"예상 외 payload 크기: {len(payload)}")
                continue

            ticker = parse_flipster_bookticker(payload)
            count += 1
            symbols_seen.add(ticker.symbol)
            elapsed = time.monotonic() - t0

            if count <= 5 or count % 1000 == 0:
                topic = topic_bytes.decode("utf-8", errors="replace")
                latency_ms = (ticker.recv_ts_ns / 1_000_000) - ticker.event_time_ms
                print(
                    f"[{count:>6}] topic={topic:<40} "
                    f"bid={ticker.bid_price:<12.6f} ask={ticker.ask_price:<12.6f} "
                    f"latency={latency_ms:.1f}ms  "
                    f"({elapsed:.1f}s, {len(symbols_seen)} symbols)"
                )

    except KeyboardInterrupt:
        elapsed = time.monotonic() - t0
        rate = count / elapsed if elapsed > 0 else 0
        print(f"\n총 {count}개 수신 / {elapsed:.1f}s / {rate:.0f} msg/s")
        print(f"심볼 {len(symbols_seen)}개: {sorted(symbols_seen)[:20]}...")

    finally:
        sock.close()
        ctx.term()


if __name__ == "__main__":
    asyncio.run(main())

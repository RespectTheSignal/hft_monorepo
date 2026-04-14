#!/usr/bin/env python3
"""
Gate.io Futures WS First-Message Latency Test

Measures the time from WebSocket connection initiation to receiving
the first data update. Critical for reconnection performance in HFT.

Breakdown:
  - Connect:    TCP + TLS + WS handshake
  - Subscribe:  send subscribe → receive ack
  - First msg:  ack → first update message
  - Total:      connect start → first update

Usage:
  python first_message_test.py
  python first_message_test.py --contract BTC_USDT --rounds 10
"""

import argparse
import asyncio
import json
import statistics
import time

import websockets

WS_URL = "wss://fx-ws.gateio.ws/v4/ws/usdt"
CONTRACT = "SOL_USDT"


async def measure_once(contract: str) -> dict | None:
    """
    One full cycle: connect → subscribe → first update → close.
    Returns timing breakdown in ms.
    """
    # Phase 1: Connect
    t_start_us = time.time_ns() / 1_000

    ws = await websockets.connect(WS_URL)
    t_connected_us = time.time_ns() / 1_000

    try:
        # Phase 2: Subscribe
        sub = json.dumps({
            "time": int(time.time()),
            "channel": "futures.book_ticker",
            "event": "subscribe",
            "payload": [contract],
        })
        await ws.send(sub)
        t_sub_sent_us = time.time_ns() / 1_000

        ack = json.loads(await ws.recv())
        t_ack_us = time.time_ns() / 1_000

        if ack.get("error"):
            return None

        # Phase 3: Wait for first update
        while True:
            raw = await ws.recv()
            t_first_us = time.time_ns() / 1_000
            msg = json.loads(raw)
            if msg.get("event") == "update":
                break

    finally:
        await ws.close()

    connect_ms = (t_connected_us - t_start_us) / 1_000
    subscribe_ms = (t_ack_us - t_sub_sent_us) / 1_000
    first_msg_ms = (t_first_us - t_ack_us) / 1_000
    total_ms = (t_first_us - t_start_us) / 1_000

    return {
        "connect_ms": connect_ms,
        "subscribe_ms": subscribe_ms,
        "first_msg_ms": first_msg_ms,
        "total_ms": total_ms,
    }


def percentile(sorted_vals: list[float], p: float) -> float:
    idx = int(len(sorted_vals) * p)
    return sorted_vals[min(idx, len(sorted_vals) - 1)]


def print_stats(label: str, values: list[float]):
    if not values:
        return
    s = sorted(values)
    print(
        f"  {label:>14s}:  "
        f"min={min(s):8.2f}  "
        f"p50={percentile(s, 0.5):8.2f}  "
        f"p90={percentile(s, 0.9):8.2f}  "
        f"p95={percentile(s, 0.95):8.2f}  "
        f"p99={percentile(s, 0.99):8.2f}  "
        f"max={max(s):8.2f}  "
        f"avg={statistics.mean(s):8.2f}  "
        f"std={statistics.stdev(s):7.2f} ms"
    )


async def main():
    parser = argparse.ArgumentParser(description="Gate.io WS first-message latency test")
    parser.add_argument("--contract", type=str, default=CONTRACT,
                        help=f"Futures contract (default: {CONTRACT})")
    parser.add_argument("--rounds", type=int, default=10,
                        help="Number of connect/disconnect cycles (default: 10)")
    args = parser.parse_args()

    print(f"{'=' * 90}")
    print(f"  Gate.io Futures WS First-Message Latency Test")
    print(f"  Contract : {args.contract}")
    print(f"  Rounds   : {args.rounds}")
    print(f"  WS URL   : {WS_URL}")
    print(f"{'=' * 90}")

    samples: list[dict] = []

    for i in range(args.rounds):
        result = await measure_once(args.contract)
        if result is None:
            print(f"  [{i + 1:3d}/{args.rounds}]  FAILED")
            continue

        samples.append(result)
        print(
            f"  [{len(samples):3d}/{args.rounds}]  "
            f"total={result['total_ms']:8.2f}ms  "
            f"connect={result['connect_ms']:8.2f}ms  "
            f"subscribe={result['subscribe_ms']:7.2f}ms  "
            f"first_msg={result['first_msg_ms']:8.2f}ms"
        )

        await asyncio.sleep(0.5)  # avoid rapid reconnects

    if not samples:
        print("\n  No valid samples collected.")
        return

    print(f"\n{'=' * 90}")
    print(f"  RESULTS  ({len(samples)} samples, contract={args.contract})")
    print(f"{'=' * 90}")
    print_stats("Total", [s["total_ms"] for s in samples])
    print_stats("Connect", [s["connect_ms"] for s in samples])
    print_stats("Subscribe", [s["subscribe_ms"] for s in samples])
    print_stats("First update", [s["first_msg_ms"] for s in samples])
    print(f"{'=' * 90}")
    print()
    print("  Total      = connect start → first update received")
    print("  Connect    = TCP + TLS + WebSocket handshake")
    print("  Subscribe  = send subscribe → receive ack")
    print("  First update = ack → first book_ticker update")


if __name__ == "__main__":
    asyncio.run(main())

#!/usr/bin/env python3
"""
Gate.io Futures WS Ping-Pong Latency Test

Measures WebSocket ping/pong round-trip time, providing a pure network
latency baseline without any application-level overhead.

Gate.io supports both:
  1. WebSocket protocol-level ping/pong (RFC 6455)
  2. Application-level ping via channel message

This test uses protocol-level ping for the most accurate RTT measurement.

Usage:
  python ping_pong_test.py
  python ping_pong_test.py --rounds 50 --interval 0.5
"""

import argparse
import asyncio
import statistics
import time

import websockets

WS_URL = "wss://fx-ws.gateio.ws/v4/ws/usdt"


async def measure_ping_pong(rounds: int, interval: float) -> list[float]:
    """
    Connect to WS and measure ping/pong RTT for N rounds.
    Returns list of RTT values in ms.
    """
    rtts_ms: list[float] = []

    async with websockets.connect(WS_URL) as ws:
        print(f"  Connected to {WS_URL}")

        for i in range(rounds):
            t_send_us = time.time_ns() / 1_000
            pong_waiter = await ws.ping()
            await pong_waiter
            t_recv_us = time.time_ns() / 1_000

            rtt_ms = (t_recv_us - t_send_us) / 1_000
            rtts_ms.append(rtt_ms)

            n = len(rtts_ms)
            if n <= 3 or n % 20 == 0 or n == rounds:
                print(f"  [{n:4d}/{rounds}]  rtt={rtt_ms:7.2f}ms")

            await asyncio.sleep(interval)

    return rtts_ms


def percentile(sorted_vals: list[float], p: float) -> float:
    idx = int(len(sorted_vals) * p)
    return sorted_vals[min(idx, len(sorted_vals) - 1)]


def print_stats(label: str, values: list[float]):
    if not values:
        return
    s = sorted(values)
    print(
        f"  {label:>12s}:  "
        f"min={min(s):7.2f}  "
        f"p50={percentile(s, 0.5):7.2f}  "
        f"p90={percentile(s, 0.9):7.2f}  "
        f"p95={percentile(s, 0.95):7.2f}  "
        f"p99={percentile(s, 0.99):7.2f}  "
        f"max={max(s):7.2f}  "
        f"avg={statistics.mean(s):7.2f}  "
        f"std={statistics.stdev(s):6.2f} ms"
    )


async def main():
    parser = argparse.ArgumentParser(description="Gate.io WS ping/pong latency test")
    parser.add_argument("--rounds", type=int, default=30,
                        help="Number of ping/pong cycles (default: 30)")
    parser.add_argument("--interval", type=float, default=0.5,
                        help="Seconds between pings (default: 0.5)")
    args = parser.parse_args()

    print(f"{'=' * 90}")
    print(f"  Gate.io Futures WS Ping-Pong Latency Test")
    print(f"  Rounds   : {args.rounds}")
    print(f"  Interval : {args.interval}s")
    print(f"  WS URL   : {WS_URL}")
    print(f"{'=' * 90}\n")

    rtts = await measure_ping_pong(args.rounds, args.interval)

    if not rtts:
        print("\n  No valid samples collected.")
        return

    print(f"\n{'=' * 90}")
    print(f"  RESULTS  ({len(rtts)} samples)")
    print(f"{'=' * 90}")
    print_stats("Ping RTT", rtts)
    print(f"{'=' * 90}")
    print()
    print("  Ping RTT = WebSocket protocol-level ping → pong round-trip")
    print("  This represents pure network latency (no application overhead)")
    print(f"  One-way estimate: ~{percentile(sorted(rtts), 0.5) / 2:.2f}ms (p50 / 2)")


if __name__ == "__main__":
    asyncio.run(main())

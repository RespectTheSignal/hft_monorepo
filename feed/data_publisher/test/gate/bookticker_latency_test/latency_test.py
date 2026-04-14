#!/usr/bin/env python3
"""
Gate.io Futures BookTicker Latency Test — SOL_USDT

Measures end-to-end WebSocket delivery latency with NTP-style clock synchronization
using Gate.io REST API response headers (X-In-Time / X-Out-Time, microsecond precision).

Latency breakdown:
  - Total:    local_recv - t (matching engine timestamp) - clock_offset
  - Internal: time_ms (gateway send) - t (matching engine)
  - Network:  local_recv - time_ms (gateway send) - clock_offset

Usage:
  pip install -r requirements.txt
  python latency_test.py
  python latency_test.py --samples 200 --sync-rounds 20
"""

import argparse
import asyncio
import json
import statistics
import time

import aiohttp
import websockets

WS_URL = "wss://fx-ws.gateio.ws/v4/ws/usdt"
REST_URL = "https://api.gateio.ws/api/v4"
CONTRACT = "SOL_USDT"


# ---------------------------------------------------------------------------
# Phase 1: Clock synchronization
# ---------------------------------------------------------------------------

async def sync_clock(session: aiohttp.ClientSession, rounds: int, contract: str) -> dict:
    """
    NTP-style clock offset estimation.

    For each round:
      1. Record local_send (μs)
      2. GET a lightweight REST endpoint
      3. Record local_recv (μs)
      4. Read X-In-Time (server recv, μs) and X-Out-Time (server send, μs)

    offset = server_midpoint - local_midpoint
    network_rtt = total_rtt - server_processing_time
    """
    offsets_us: list[float] = []
    rtts_us: list[float] = []

    url = f"{REST_URL}/futures/usdt/tickers"
    params = {"contract": contract}

    for _ in range(rounds):
        local_send = time.time_ns() / 1_000  # ns -> μs

        async with session.get(url, params=params) as resp:
            local_recv = time.time_ns() / 1_000
            await resp.read()

            x_in = resp.headers.get("X-In-Time")
            x_out = resp.headers.get("X-Out-Time")
            if not (x_in and x_out):
                continue

            server_recv = float(x_in)   # μs
            server_send = float(x_out)  # μs

            server_proc = server_send - server_recv
            total_rtt = local_recv - local_send
            net_rtt = total_rtt - server_proc

            offset = (server_recv + server_send) / 2 - (local_send + local_recv) / 2

            offsets_us.append(offset)
            rtts_us.append(net_rtt)

        await asyncio.sleep(0.05)

    if not offsets_us:
        raise RuntimeError("Clock sync failed: no valid samples (missing X-In-Time/X-Out-Time headers)")

    return {
        "offset_us": statistics.median(offsets_us),
        "rtt_us": statistics.median(rtts_us),
        "offset_stdev_us": statistics.stdev(offsets_us) if len(offsets_us) > 1 else 0.0,
        "n": len(offsets_us),
    }


# ---------------------------------------------------------------------------
# Phase 2: WebSocket book_ticker measurement
# ---------------------------------------------------------------------------

async def measure_latency(clock: dict, num_samples: int, contract: str) -> list[dict]:
    """
    Connect to Gate.io futures WebSocket, subscribe to book_ticker,
    and collect latency samples.
    """
    offset_us = clock["offset_us"]
    samples: list[dict] = []

    async with websockets.connect(WS_URL) as ws:
        sub = json.dumps({
            "time": int(time.time()),
            "channel": "futures.book_ticker",
            "event": "subscribe",
            "payload": [contract],
        })
        await ws.send(sub)

        # consume subscription ack
        ack = json.loads(await ws.recv())
        if ack.get("error"):
            raise RuntimeError(f"Subscribe failed: {ack['error']}")
        print(f"  Subscribed: {ack.get('event')}")

        while len(samples) < num_samples:
            raw = await ws.recv()
            local_recv_us = time.time_ns() / 1_000

            msg = json.loads(raw)
            if msg.get("event") != "update":
                continue

            result = msg.get("result", {})
            t_ms = result.get("t")       # matching engine timestamp (ms)
            time_ms = msg.get("time_ms") # gateway send timestamp (ms)
            if t_ms is None:
                continue

            t_us = t_ms * 1_000
            gw_us = time_ms * 1_000 if time_ms else None

            total_ms = (local_recv_us - t_us - offset_us) / 1_000
            internal_ms = (gw_us - t_us) / 1_000 if gw_us else None
            network_ms = (local_recv_us - gw_us - offset_us) / 1_000 if gw_us else None

            samples.append({
                "total_ms": total_ms,
                "internal_ms": internal_ms,
                "network_ms": network_ms,
                "bid": result.get("b"),
                "ask": result.get("a"),
            })

            n = len(samples)
            if n <= 3 or n % 50 == 0 or n == num_samples:
                print(
                    f"  [{n:4d}/{num_samples}]  "
                    f"total={total_ms:7.2f}ms  "
                    f"internal={internal_ms:6.2f}ms  "
                    f"network={network_ms:7.2f}ms  "
                    f"bid={result.get('b')}  ask={result.get('a')}"
                )

    return samples


# ---------------------------------------------------------------------------
# Phase 3: Statistics
# ---------------------------------------------------------------------------

def percentile(sorted_vals: list[float], p: float) -> float:
    idx = int(len(sorted_vals) * p)
    return sorted_vals[min(idx, len(sorted_vals) - 1)]


def print_stats(label: str, values: list[float]):
    if not values:
        return
    s = sorted(values)
    print(f"  {label:>12s}:  "
          f"min={min(s):7.2f}  "
          f"p50={percentile(s, 0.5):7.2f}  "
          f"p90={percentile(s, 0.9):7.2f}  "
          f"p95={percentile(s, 0.95):7.2f}  "
          f"p99={percentile(s, 0.99):7.2f}  "
          f"max={max(s):7.2f}  "
          f"avg={statistics.mean(s):7.2f}  "
          f"std={statistics.stdev(s):6.2f} ms")


def report(clock: dict, samples: list[dict], contract: str):
    totals = [s["total_ms"] for s in samples]
    internals = [s["internal_ms"] for s in samples if s["internal_ms"] is not None]
    networks = [s["network_ms"] for s in samples if s["network_ms"] is not None]

    print(f"\n{'=' * 90}")
    print(f"  RESULTS  ({len(samples)} samples, contract={contract})")
    print(f"{'=' * 90}")
    print_stats("Total", totals)
    print_stats("Internal", internals)
    print_stats("Network", networks)
    print(f"{'=' * 90}")
    print(f"  Clock offset : {clock['offset_us'] / 1000:+.3f} ms  "
          f"(stdev={clock['offset_stdev_us'] / 1000:.3f} ms, n={clock['n']})")
    print(f"  Network RTT  : {clock['rtt_us'] / 1000:.3f} ms (median)")
    print(f"{'=' * 90}")
    print()
    print("  Total    = local_recv - exchange_timestamp(t) - clock_offset")
    print("  Internal = gateway_send(time_ms) - exchange_timestamp(t)")
    print("  Network  = local_recv - gateway_send(time_ms) - clock_offset")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

async def main():
    parser = argparse.ArgumentParser(description="Gate.io BookTicker latency test")
    parser.add_argument("--samples", type=int, default=100, help="Number of book_ticker messages to collect (default: 100)")
    parser.add_argument("--sync-rounds", type=int, default=10, help="Clock sync sample count (default: 10)")
    parser.add_argument("--contract", type=str, default=CONTRACT, help=f"Futures contract (default: {CONTRACT})")
    args = parser.parse_args()

    contract = args.contract

    print(f"{'=' * 90}")
    print(f"  Gate.io Futures BookTicker Latency Test")
    print(f"  Contract : {contract}")
    print(f"  Samples  : {args.samples}")
    print(f"  WS URL   : {WS_URL}")
    print(f"{'=' * 90}")

    # Phase 1
    print(f"\n[Phase 1] Clock Synchronization ({args.sync_rounds} rounds)...")
    async with aiohttp.ClientSession() as session:
        clock = await sync_clock(session, args.sync_rounds, contract)
    print(f"  Offset: {clock['offset_us'] / 1000:+.3f} ms  "
          f"RTT: {clock['rtt_us'] / 1000:.3f} ms  "
          f"Stdev: {clock['offset_stdev_us'] / 1000:.3f} ms")

    # Phase 2
    print(f"\n[Phase 2] Collecting {args.samples} book_ticker updates...")
    samples = await measure_latency(clock, args.samples, contract)

    # Phase 3
    report(clock, samples, contract)


if __name__ == "__main__":
    asyncio.run(main())

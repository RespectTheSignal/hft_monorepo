#!/usr/bin/env python3
"""
Gate.io Futures WS Channel Comparison Latency Test

Compares WebSocket delivery latency across different channels for the
same contract, to determine which channel provides data fastest.

Channels tested:
  1. futures.book_ticker  — BBO (best bid/offer) updates
  2. futures.trades       — individual trade executions
  3. futures.order_book   — orderbook diff updates

Usage:
  python channel_comparison_test.py
  python channel_comparison_test.py --contract BTC_USDT --samples 30
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
# Clock sync
# ---------------------------------------------------------------------------

async def sync_clock(session: aiohttp.ClientSession, rounds: int) -> dict:
    offsets_us: list[float] = []
    url = f"{REST_URL}/futures/usdt/tickers"
    params = {"contract": "BTC_USDT"}

    for _ in range(rounds):
        local_send = time.time_ns() / 1_000
        async with session.get(url, params=params) as resp:
            local_recv = time.time_ns() / 1_000
            await resp.read()
            x_in = resp.headers.get("X-In-Time")
            x_out = resp.headers.get("X-Out-Time")
            if not (x_in and x_out):
                continue
            server_recv = float(x_in)
            server_send = float(x_out)
            offset = (server_recv + server_send) / 2 - (local_send + local_recv) / 2
            offsets_us.append(offset)
        await asyncio.sleep(0.05)

    if not offsets_us:
        raise RuntimeError("Clock sync failed")
    return {
        "offset_us": statistics.median(offsets_us),
        "offset_stdev_us": statistics.stdev(offsets_us) if len(offsets_us) > 1 else 0.0,
        "n": len(offsets_us),
    }


# ---------------------------------------------------------------------------
# Channel definitions & timestamp extraction
# ---------------------------------------------------------------------------

CHANNELS = [
    {
        "name": "book_ticker",
        "channel": "futures.book_ticker",
        "payload_fn": lambda contract: [contract],
    },
    {
        "name": "trades",
        "channel": "futures.trades",
        "payload_fn": lambda contract: [contract],
    },
    {
        "name": "order_book",
        "channel": "futures.order_book",
        "payload_fn": lambda contract: [contract, "5", "0"],  # 5 levels, 100ms interval
    },
]


def extract_timestamp_ms(channel_name: str, msg: dict) -> float | None:
    """Extract the earliest server-side timestamp (ms) from a message."""
    if channel_name == "book_ticker":
        # result.t = matching engine timestamp (ms)
        return msg.get("result", {}).get("t")
    elif channel_name == "trades":
        # result[0].create_time_ms (ms with decimal)
        results = msg.get("result", [])
        if results and isinstance(results, list):
            ct = results[0].get("create_time_ms")
            return ct * 1_000 if ct else None  # create_time_ms is in seconds with ms decimal
        return None
    elif channel_name == "order_book":
        # result.t = orderbook event timestamp (ms)
        return msg.get("result", {}).get("t")
    return None


# ---------------------------------------------------------------------------
# Measure one channel
# ---------------------------------------------------------------------------

async def measure_channel(
    ch: dict, contract: str, offset_us: float, num_samples: int, timeout: float
) -> list[dict]:
    samples: list[dict] = []

    async with websockets.connect(WS_URL) as ws:
        sub = json.dumps({
            "time": int(time.time()),
            "channel": ch["channel"],
            "event": "subscribe",
            "payload": ch["payload_fn"](contract),
        })
        await ws.send(sub)

        ack = json.loads(await ws.recv())
        if ack.get("error"):
            print(f"    Subscribe failed for {ch['name']}: {ack['error']}")
            return samples

        while len(samples) < num_samples:
            try:
                raw = await asyncio.wait_for(ws.recv(), timeout=timeout)
            except asyncio.TimeoutError:
                print(f"    Timeout waiting for {ch['name']} update (collected {len(samples)} samples)")
                break

            local_recv_us = time.time_ns() / 1_000
            msg = json.loads(raw)

            if msg.get("event") != "update":
                continue

            t_ms = extract_timestamp_ms(ch["name"], msg)
            time_ms = msg.get("time_ms")  # gateway timestamp

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
            })

            n = len(samples)
            if n <= 2 or n == num_samples:
                int_str = f"{internal_ms:6.2f}" if internal_ms is not None else "   N/A"
                net_str = f"{network_ms:7.2f}" if network_ms is not None else "    N/A"
                print(
                    f"    [{n:4d}/{num_samples}]  "
                    f"total={total_ms:7.2f}ms  "
                    f"internal={int_str}ms  "
                    f"network={net_str}ms"
                )

    return samples


# ---------------------------------------------------------------------------
# Statistics
# ---------------------------------------------------------------------------

def percentile(sorted_vals: list[float], p: float) -> float:
    idx = int(len(sorted_vals) * p)
    return sorted_vals[min(idx, len(sorted_vals) - 1)]


def print_stats(label: str, values: list[float]):
    if not values:
        return
    s = sorted(values)
    print(
        f"    {label:>12s}:  "
        f"min={min(s):7.2f}  "
        f"p50={percentile(s, 0.5):7.2f}  "
        f"p90={percentile(s, 0.9):7.2f}  "
        f"p95={percentile(s, 0.95):7.2f}  "
        f"p99={percentile(s, 0.99):7.2f}  "
        f"max={max(s):7.2f}  "
        f"avg={statistics.mean(s):7.2f}  "
        f"std={statistics.stdev(s):6.2f} ms"
    )


def report(all_results: dict[str, list[dict]], clock: dict, contract: str):
    for ch_name, samples in all_results.items():
        if not samples:
            print(f"\n  [{ch_name}] no valid samples")
            continue
        print(f"\n  [{ch_name}] ({len(samples)} samples)")
        print_stats("Total", [s["total_ms"] for s in samples])
        print_stats("Internal", [s["internal_ms"] for s in samples if s["internal_ms"] is not None])
        print_stats("Network", [s["network_ms"] for s in samples if s["network_ms"] is not None])

    print(f"\n{'=' * 90}")
    print(f"  COMPARISON (p50 values in ms, contract={contract})")
    print(f"{'=' * 90}")
    print(f"  {'Channel':>14s}  {'Total':>8s}  {'Internal':>8s}  {'Network':>8s}  {'Samples':>8s}")
    print(f"  {'-' * 14}  {'-' * 8}  {'-' * 8}  {'-' * 8}  {'-' * 8}")

    for ch_name, samples in all_results.items():
        if not samples:
            print(f"  {ch_name:>14s}  {'--':>8s}  {'--':>8s}  {'--':>8s}  {'0':>8s}")
            continue

        total_s = sorted(s["total_ms"] for s in samples)
        internal_vals = [s["internal_ms"] for s in samples if s["internal_ms"] is not None]
        network_vals = [s["network_ms"] for s in samples if s["network_ms"] is not None]

        int_str = f"{percentile(sorted(internal_vals), 0.5):8.2f}" if internal_vals else "      --"
        net_str = f"{percentile(sorted(network_vals), 0.5):8.2f}" if network_vals else "      --"

        print(
            f"  {ch_name:>14s}  "
            f"{percentile(total_s, 0.5):8.2f}  "
            f"{int_str}  "
            f"{net_str}  "
            f"{len(samples):8d}"
        )

    print(f"{'=' * 90}")
    print(f"  Clock offset : {clock['offset_us'] / 1000:+.3f} ms  "
          f"(stdev={clock['offset_stdev_us'] / 1000:.3f} ms, n={clock['n']})")
    print(f"{'=' * 90}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

async def main():
    parser = argparse.ArgumentParser(description="Gate.io WS channel comparison latency test")
    parser.add_argument("--contract", type=str, default=CONTRACT,
                        help=f"Futures contract (default: {CONTRACT})")
    parser.add_argument("--samples", type=int, default=30,
                        help="Samples per channel (default: 30)")
    parser.add_argument("--sync-rounds", type=int, default=10,
                        help="Clock sync rounds (default: 10)")
    parser.add_argument("--timeout", type=float, default=30.0,
                        help="Timeout per message in seconds (default: 30)")
    args = parser.parse_args()

    print(f"{'=' * 90}")
    print(f"  Gate.io Futures WS Channel Comparison Latency Test")
    print(f"  Contract : {args.contract}")
    print(f"  Channels : {', '.join(c['name'] for c in CHANNELS)}")
    print(f"  Samples  : {args.samples} per channel")
    print(f"{'=' * 90}")

    # Phase 1: Clock sync
    print(f"\n[Phase 1] Clock Synchronization ({args.sync_rounds} rounds)...")
    async with aiohttp.ClientSession() as session:
        clock = await sync_clock(session, args.sync_rounds)
    print(f"  Offset: {clock['offset_us'] / 1000:+.3f} ms")

    # Phase 2: Measure each channel
    all_results: dict[str, list[dict]] = {}
    for ch in CHANNELS:
        print(f"\n[Phase 2] Measuring channel: {ch['name']}  ({ch['channel']})")
        samples = await measure_channel(ch, args.contract, clock["offset_us"], args.samples, args.timeout)
        all_results[ch["name"]] = samples

    # Phase 3: Report
    print(f"\n{'=' * 90}")
    print(f"  RESULTS")
    print(f"{'=' * 90}")
    report(all_results, clock, args.contract)


if __name__ == "__main__":
    asyncio.run(main())

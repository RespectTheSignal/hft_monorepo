#!/usr/bin/env python3
"""
Gate.io Futures Cross-Contract BookTicker Latency Comparison

Compares book_ticker WebSocket delivery latency across multiple contracts
to identify whether high-volume contracts (BTC, ETH) have different
latency characteristics than lower-volume ones.

Uses NTP-style clock synchronization (same as bookticker_latency_test).

Usage:
  python cross_contract_test.py
  python cross_contract_test.py --contracts BTC_USDT ETH_USDT SOL_USDT --samples 50
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
DEFAULT_CONTRACTS = ["BTC_USDT", "ETH_USDT", "SOL_USDT", "DOGE_USDT", "XRP_USDT"]


# ---------------------------------------------------------------------------
# Clock sync (reused from bookticker_latency_test)
# ---------------------------------------------------------------------------

async def sync_clock(session: aiohttp.ClientSession, rounds: int) -> dict:
    offsets_us: list[float] = []
    rtts_us: list[float] = []
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
            net_rtt = (local_recv - local_send) - (server_send - server_recv)
            offsets_us.append(offset)
            rtts_us.append(net_rtt)
        await asyncio.sleep(0.05)

    if not offsets_us:
        raise RuntimeError("Clock sync failed")

    return {
        "offset_us": statistics.median(offsets_us),
        "rtt_us": statistics.median(rtts_us),
        "offset_stdev_us": statistics.stdev(offsets_us) if len(offsets_us) > 1 else 0.0,
        "n": len(offsets_us),
    }


# ---------------------------------------------------------------------------
# Measure one contract
# ---------------------------------------------------------------------------

async def measure_contract(
    contract: str, offset_us: float, num_samples: int
) -> list[dict]:
    samples: list[dict] = []

    async with websockets.connect(WS_URL) as ws:
        sub = json.dumps({
            "time": int(time.time()),
            "channel": "futures.book_ticker",
            "event": "subscribe",
            "payload": [contract],
        })
        await ws.send(sub)

        ack = json.loads(await ws.recv())
        if ack.get("error"):
            print(f"    Subscribe failed for {contract}: {ack['error']}")
            return samples

        while len(samples) < num_samples:
            raw = await ws.recv()
            local_recv_us = time.time_ns() / 1_000
            msg = json.loads(raw)

            if msg.get("event") != "update":
                continue

            result = msg.get("result", {})
            t_ms = result.get("t")
            time_ms = msg.get("time_ms")
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
                print(
                    f"    [{n:4d}/{num_samples}]  "
                    f"total={total_ms:7.2f}ms  "
                    f"internal={internal_ms:6.2f}ms  "
                    f"network={network_ms:7.2f}ms"
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


def report(all_results: dict[str, list[dict]], clock: dict):
    # Per-contract detail
    for contract, samples in all_results.items():
        if not samples:
            print(f"\n  [{contract}] no valid samples")
            continue
        print(f"\n  [{contract}] ({len(samples)} samples)")
        print_stats("Total", [s["total_ms"] for s in samples])
        print_stats("Internal", [s["internal_ms"] for s in samples if s["internal_ms"] is not None])
        print_stats("Network", [s["network_ms"] for s in samples if s["network_ms"] is not None])

    # Comparison table
    print(f"\n{'=' * 90}")
    print(f"  COMPARISON (p50 values in ms)")
    print(f"{'=' * 90}")
    print(f"  {'Contract':>12s}  {'Total':>8s}  {'Internal':>8s}  {'Network':>8s}  {'Samples':>8s}")
    print(f"  {'-' * 12}  {'-' * 8}  {'-' * 8}  {'-' * 8}  {'-' * 8}")

    for contract, samples in all_results.items():
        if not samples:
            print(f"  {contract:>12s}  {'--':>8s}  {'--':>8s}  {'--':>8s}  {'0':>8s}")
            continue

        total_s = sorted(s["total_ms"] for s in samples)
        internal_s = sorted(s["internal_ms"] for s in samples if s["internal_ms"] is not None)
        network_s = sorted(s["network_ms"] for s in samples if s["network_ms"] is not None)

        print(
            f"  {contract:>12s}  "
            f"{percentile(total_s, 0.5):8.2f}  "
            f"{percentile(internal_s, 0.5):8.2f}  "
            f"{percentile(network_s, 0.5):8.2f}  "
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
    parser = argparse.ArgumentParser(description="Gate.io cross-contract latency comparison")
    parser.add_argument("--contracts", nargs="+", default=DEFAULT_CONTRACTS,
                        help=f"Contracts to compare (default: {' '.join(DEFAULT_CONTRACTS)})")
    parser.add_argument("--samples", type=int, default=30,
                        help="Samples per contract (default: 30)")
    parser.add_argument("--sync-rounds", type=int, default=10,
                        help="Clock sync rounds (default: 10)")
    args = parser.parse_args()

    print(f"{'=' * 90}")
    print(f"  Gate.io Futures Cross-Contract BookTicker Latency Comparison")
    print(f"  Contracts : {', '.join(args.contracts)}")
    print(f"  Samples   : {args.samples} per contract")
    print(f"{'=' * 90}")

    # Phase 1: Clock sync
    print(f"\n[Phase 1] Clock Synchronization ({args.sync_rounds} rounds)...")
    async with aiohttp.ClientSession() as session:
        clock = await sync_clock(session, args.sync_rounds)
    print(f"  Offset: {clock['offset_us'] / 1000:+.3f} ms  "
          f"RTT: {clock['rtt_us'] / 1000:.3f} ms")

    # Phase 2: Measure each contract sequentially
    all_results: dict[str, list[dict]] = {}
    for contract in args.contracts:
        print(f"\n[Phase 2] Measuring {contract}...")
        samples = await measure_contract(contract, clock["offset_us"], args.samples)
        all_results[contract] = samples

    # Phase 3: Report
    print(f"\n{'=' * 90}")
    print(f"  RESULTS")
    print(f"{'=' * 90}")
    report(all_results, clock)


if __name__ == "__main__":
    asyncio.run(main())

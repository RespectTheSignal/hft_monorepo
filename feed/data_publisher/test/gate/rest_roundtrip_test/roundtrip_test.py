#!/usr/bin/env python3
"""
Gate.io Futures REST API Round-Trip Latency Test

Measures round-trip time for key REST endpoints, decomposing into:
  - Total RTT:    local_recv - local_send
  - Server proc:  X-Out-Time - X-In-Time   (μs precision from response headers)
  - Network RTT:  Total RTT - Server proc   (bidirectional network time)

Endpoints tested:
  1. GET /futures/usdt/tickers?contract=X     — single ticker (lightweight)
  2. GET /futures/usdt/order_book?contract=X   — orderbook snapshot
  3. GET /futures/usdt/trades?contract=X       — recent trades
  4. GET /futures/usdt/contracts/X              — single contract info

Usage:
  pip install -r requirements.txt
  python roundtrip_test.py
  python roundtrip_test.py --contract BTC_USDT --rounds 50
"""

import argparse
import asyncio
import statistics
import time

import aiohttp

REST_URL = "https://api.gateio.ws/api/v4"
CONTRACT = "SOL_USDT"


# ---------------------------------------------------------------------------
# Endpoint definitions
# ---------------------------------------------------------------------------

def get_endpoints(contract: str) -> list[dict]:
    """Return list of endpoints to benchmark."""
    return [
        {
            "name": "tickers",
            "path": "/futures/usdt/tickers",
            "params": {"contract": contract},
        },
        {
            "name": "order_book",
            "path": "/futures/usdt/order_book",
            "params": {"contract": contract, "limit": 5},
        },
        {
            "name": "trades",
            "path": "/futures/usdt/trades",
            "params": {"contract": contract, "limit": 1},
        },
        {
            "name": "contract_info",
            "path": f"/futures/usdt/contracts/{contract}",
            "params": {},
        },
    ]


# ---------------------------------------------------------------------------
# Single request measurement
# ---------------------------------------------------------------------------

async def measure_once(
    session: aiohttp.ClientSession, url: str, params: dict
) -> dict | None:
    """
    Fire one GET request and return timing breakdown.
    Returns None if X-In-Time/X-Out-Time headers are missing.
    """
    local_send_us = time.time_ns() / 1_000

    async with session.get(url, params=params) as resp:
        local_recv_us = time.time_ns() / 1_000
        await resp.read()

        x_in = resp.headers.get("X-In-Time")
        x_out = resp.headers.get("X-Out-Time")
        status = resp.status

    if status != 200 or not (x_in and x_out):
        return None

    server_recv_us = float(x_in)
    server_send_us = float(x_out)

    total_us = local_recv_us - local_send_us
    server_proc_us = server_send_us - server_recv_us
    network_us = total_us - server_proc_us

    return {
        "total_us": total_us,
        "server_proc_us": server_proc_us,
        "network_us": network_us,
    }


# ---------------------------------------------------------------------------
# Run benchmark for one endpoint
# ---------------------------------------------------------------------------

async def benchmark_endpoint(
    session: aiohttp.ClientSession,
    endpoint: dict,
    rounds: int,
    warmup: int,
) -> list[dict]:
    """Benchmark a single endpoint with warmup + measured rounds."""
    url = f"{REST_URL}{endpoint['path']}"
    params = endpoint["params"]

    # Warmup (discard results, primes TCP/TLS connection)
    for _ in range(warmup):
        await measure_once(session, url, params)
        await asyncio.sleep(0.02)

    samples: list[dict] = []
    for i in range(rounds):
        result = await measure_once(session, url, params)
        if result:
            samples.append(result)

        # Print progress for first 3 and every 20th
        if result and (len(samples) <= 3 or len(samples) % 20 == 0 or len(samples) == rounds):
            print(
                f"    [{len(samples):4d}/{rounds}]  "
                f"total={result['total_us'] / 1000:7.2f}ms  "
                f"server={result['server_proc_us'] / 1000:6.2f}ms  "
                f"network={result['network_us'] / 1000:7.2f}ms"
            )

        await asyncio.sleep(0.05)  # rate-limit to avoid 429

    return samples


# ---------------------------------------------------------------------------
# Statistics
# ---------------------------------------------------------------------------

def percentile(sorted_vals: list[float], p: float) -> float:
    idx = int(len(sorted_vals) * p)
    return sorted_vals[min(idx, len(sorted_vals) - 1)]


def print_stats(label: str, values_us: list[float]):
    """Print percentile stats. Input in μs, output in ms."""
    if not values_us:
        return
    ms = sorted(v / 1_000 for v in values_us)
    print(
        f"    {label:>12s}:  "
        f"min={min(ms):7.2f}  "
        f"p50={percentile(ms, 0.5):7.2f}  "
        f"p90={percentile(ms, 0.9):7.2f}  "
        f"p95={percentile(ms, 0.95):7.2f}  "
        f"p99={percentile(ms, 0.99):7.2f}  "
        f"max={max(ms):7.2f}  "
        f"avg={statistics.mean(ms):7.2f}  "
        f"std={statistics.stdev(ms):6.2f} ms"
    )


def report_endpoint(name: str, samples: list[dict]):
    """Print stats for one endpoint."""
    if not samples:
        print(f"  {name}: no valid samples")
        return

    totals = [s["total_us"] for s in samples]
    server = [s["server_proc_us"] for s in samples]
    network = [s["network_us"] for s in samples]

    print(f"\n  [{name}] ({len(samples)} samples)")
    print_stats("Total RTT", totals)
    print_stats("Server proc", server)
    print_stats("Network RTT", network)


def report_comparison(all_results: dict[str, list[dict]]):
    """Print side-by-side comparison of median RTTs across endpoints."""
    print(f"\n{'=' * 90}")
    print(f"  COMPARISON (p50 values in ms)")
    print(f"{'=' * 90}")
    print(f"  {'Endpoint':>15s}  {'Total':>8s}  {'Server':>8s}  {'Network':>8s}  {'Samples':>8s}")
    print(f"  {'-' * 15}  {'-' * 8}  {'-' * 8}  {'-' * 8}  {'-' * 8}")

    for name, samples in all_results.items():
        if not samples:
            print(f"  {name:>15s}  {'--':>8s}  {'--':>8s}  {'--':>8s}  {'0':>8s}")
            continue

        total_ms = sorted(s["total_us"] / 1000 for s in samples)
        server_ms = sorted(s["server_proc_us"] / 1000 for s in samples)
        network_ms = sorted(s["network_us"] / 1000 for s in samples)

        print(
            f"  {name:>15s}  "
            f"{percentile(total_ms, 0.5):8.2f}  "
            f"{percentile(server_ms, 0.5):8.2f}  "
            f"{percentile(network_ms, 0.5):8.2f}  "
            f"{len(samples):8d}"
        )

    print(f"{'=' * 90}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

async def main():
    parser = argparse.ArgumentParser(description="Gate.io REST API round-trip latency test")
    parser.add_argument("--contract", type=str, default=CONTRACT,
                        help=f"Futures contract (default: {CONTRACT})")
    parser.add_argument("--rounds", type=int, default=30,
                        help="Requests per endpoint (default: 30)")
    parser.add_argument("--warmup", type=int, default=3,
                        help="Warmup requests per endpoint (default: 3)")
    args = parser.parse_args()

    endpoints = get_endpoints(args.contract)

    print(f"{'=' * 90}")
    print(f"  Gate.io Futures REST API Round-Trip Latency Test")
    print(f"  Contract  : {args.contract}")
    print(f"  Rounds    : {args.rounds} (+{args.warmup} warmup)")
    print(f"  Endpoints : {', '.join(e['name'] for e in endpoints)}")
    print(f"  Base URL  : {REST_URL}")
    print(f"{'=' * 90}")

    all_results: dict[str, list[dict]] = {}

    async with aiohttp.ClientSession() as session:
        for ep in endpoints:
            print(f"\n  Benchmarking: {ep['name']}  ({ep['path']})")
            samples = await benchmark_endpoint(session, ep, args.rounds, args.warmup)
            all_results[ep["name"]] = samples
            report_endpoint(ep["name"], samples)

    report_comparison(all_results)

    print()
    print("  Total RTT   = local_recv - local_send (full round-trip)")
    print("  Server proc = X-Out-Time - X-In-Time (server-side processing)")
    print("  Network RTT = Total RTT - Server proc (bidirectional network)")


if __name__ == "__main__":
    asyncio.run(main())

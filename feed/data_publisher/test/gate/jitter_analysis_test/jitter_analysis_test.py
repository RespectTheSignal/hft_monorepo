#!/usr/bin/env python3
"""
Gate.io Futures BookTicker Jitter Analysis

Analyzes the variance and distribution of message arrival intervals,
latency fluctuations, and burst patterns. Stable, low-jitter feeds
are critical for HFT systems that rely on predictable timing.

Metrics:
  - Inter-arrival time: gap between consecutive messages
  - Latency jitter: variance in end-to-end latency
  - Burst detection: clusters of messages arriving in rapid succession

Usage:
  python jitter_analysis_test.py
  python jitter_analysis_test.py --contract BTC_USDT --samples 200
"""

import argparse
import asyncio
import json
import math
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
# Collect samples with precise timestamps
# ---------------------------------------------------------------------------

async def collect_samples(
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
            raise RuntimeError(f"Subscribe failed: {ack['error']}")
        print(f"  Subscribed: {ack.get('event')}")

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

            samples.append({
                "local_recv_us": local_recv_us,
                "t_us": t_us,
                "gw_us": gw_us,
                "total_ms": total_ms,
            })

            n = len(samples)
            if n <= 3 or n % 50 == 0 or n == num_samples:
                print(f"  [{n:4d}/{num_samples}]  total={total_ms:7.2f}ms")

    return samples


# ---------------------------------------------------------------------------
# Analysis
# ---------------------------------------------------------------------------

def percentile(sorted_vals: list[float], p: float) -> float:
    idx = int(len(sorted_vals) * p)
    return sorted_vals[min(idx, len(sorted_vals) - 1)]


def print_stats(label: str, values: list[float], unit: str = "ms"):
    if not values:
        return
    s = sorted(values)
    print(
        f"  {label:>18s}:  "
        f"min={min(s):8.2f}  "
        f"p50={percentile(s, 0.5):8.2f}  "
        f"p90={percentile(s, 0.9):8.2f}  "
        f"p95={percentile(s, 0.95):8.2f}  "
        f"p99={percentile(s, 0.99):8.2f}  "
        f"max={max(s):8.2f}  "
        f"avg={statistics.mean(s):8.2f}  "
        f"std={statistics.stdev(s):7.2f} {unit}"
    )


def analyze_jitter(samples: list[dict]) -> dict:
    """Compute jitter metrics from raw samples."""
    # Inter-arrival times (local clock)
    inter_arrivals_ms = [
        (samples[i]["local_recv_us"] - samples[i - 1]["local_recv_us"]) / 1_000
        for i in range(1, len(samples))
    ]

    # End-to-end latency series
    latencies_ms = [s["total_ms"] for s in samples]

    # Latency deltas (consecutive differences)
    latency_deltas_ms = [
        abs(latencies_ms[i] - latencies_ms[i - 1])
        for i in range(1, len(latencies_ms))
    ]

    # Burst detection: messages arriving < 1ms apart
    burst_threshold_ms = 1.0
    bursts = []
    current_burst = 1
    for ia in inter_arrivals_ms:
        if ia < burst_threshold_ms:
            current_burst += 1
        else:
            if current_burst > 1:
                bursts.append(current_burst)
            current_burst = 1
    if current_burst > 1:
        bursts.append(current_burst)

    # Gap detection: inter-arrival > 5x median
    if inter_arrivals_ms:
        median_ia = statistics.median(inter_arrivals_ms)
        gaps = [ia for ia in inter_arrivals_ms if ia > median_ia * 5]
    else:
        gaps = []

    return {
        "inter_arrivals_ms": inter_arrivals_ms,
        "latencies_ms": latencies_ms,
        "latency_deltas_ms": latency_deltas_ms,
        "bursts": bursts,
        "gaps_ms": gaps,
    }


def build_histogram(values: list[float], num_bins: int = 10) -> list[tuple[float, float, int]]:
    """Build a simple histogram. Returns list of (bin_start, bin_end, count)."""
    if not values:
        return []
    lo, hi = min(values), max(values)
    if lo == hi:
        return [(lo, hi, len(values))]
    bin_width = (hi - lo) / num_bins
    bins = [(lo + i * bin_width, lo + (i + 1) * bin_width, 0) for i in range(num_bins)]
    for v in values:
        idx = min(int((v - lo) / bin_width), num_bins - 1)
        start, end, count = bins[idx]
        bins[idx] = (start, end, count + 1)
    return bins


def print_histogram(label: str, values: list[float], unit: str = "ms", num_bins: int = 10):
    """Print ASCII histogram."""
    bins = build_histogram(values, num_bins)
    if not bins:
        return
    max_count = max(b[2] for b in bins)
    bar_width = 40

    print(f"\n  {label} distribution:")
    for start, end, count in bins:
        bar_len = int(count / max_count * bar_width) if max_count > 0 else 0
        bar = "#" * bar_len
        print(f"    [{start:8.2f}-{end:8.2f}) {unit}  {bar}  ({count})")


def report(analysis: dict, clock: dict, contract: str, num_samples: int):
    ia = analysis["inter_arrivals_ms"]
    lat = analysis["latencies_ms"]
    deltas = analysis["latency_deltas_ms"]
    bursts = analysis["bursts"]
    gaps = analysis["gaps_ms"]

    print(f"\n{'=' * 90}")
    print(f"  JITTER ANALYSIS  ({num_samples} samples, contract={contract})")
    print(f"{'=' * 90}")

    print(f"\n  --- Inter-Arrival Time ---")
    print_stats("Inter-arrival", ia)
    if ia:
        total_time_s = sum(ia) / 1_000
        msg_rate = len(ia) / total_time_s if total_time_s > 0 else 0
        print(f"  {'Message rate':>18s}:  {msg_rate:.1f} msg/s  (over {total_time_s:.1f}s)")

    print(f"\n  --- End-to-End Latency ---")
    print_stats("Latency", lat)

    print(f"\n  --- Latency Jitter (|Δ| between consecutive) ---")
    print_stats("Jitter", deltas)

    print(f"\n  --- Burst Detection (inter-arrival < 1ms) ---")
    if bursts:
        print(f"  {'Burst count':>18s}:  {len(bursts)}")
        print(f"  {'Max burst size':>18s}:  {max(bursts)} messages")
        print(f"  {'Avg burst size':>18s}:  {statistics.mean(bursts):.1f} messages")
    else:
        print(f"  {'Bursts detected':>18s}:  0")

    print(f"\n  --- Gap Detection (inter-arrival > 5x median) ---")
    if gaps:
        print(f"  {'Gaps detected':>18s}:  {len(gaps)}")
        print(f"  {'Max gap':>18s}:  {max(gaps):.2f} ms")
        print(f"  {'Avg gap':>18s}:  {statistics.mean(gaps):.2f} ms")
    else:
        print(f"  {'Gaps detected':>18s}:  0")

    # Histograms
    print_histogram("Inter-arrival time", ia)
    print_histogram("End-to-end latency", lat)

    # Stability score
    if lat and ia:
        latency_cv = statistics.stdev(lat) / statistics.mean(lat) if statistics.mean(lat) != 0 else float('inf')
        ia_cv = statistics.stdev(ia) / statistics.mean(ia) if statistics.mean(ia) != 0 else float('inf')

        print(f"\n{'=' * 90}")
        print(f"  STABILITY METRICS")
        print(f"{'=' * 90}")
        print(f"  Latency CoV (stdev/mean)       : {latency_cv:.4f}  {'(stable)' if latency_cv < 0.2 else '(variable)' if latency_cv < 0.5 else '(unstable)'}")
        print(f"  Inter-arrival CoV (stdev/mean)  : {ia_cv:.4f}  {'(regular)' if ia_cv < 0.5 else '(irregular)' if ia_cv < 1.0 else '(bursty)'}")
        print(f"  p99/p50 latency ratio           : {percentile(sorted(lat), 0.99) / percentile(sorted(lat), 0.5):.2f}x  (tail amplification)")

    print(f"\n{'=' * 90}")
    print(f"  Clock offset : {clock['offset_us'] / 1000:+.3f} ms  "
          f"(stdev={clock['offset_stdev_us'] / 1000:.3f} ms, n={clock['n']})")
    print(f"{'=' * 90}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

async def main():
    parser = argparse.ArgumentParser(description="Gate.io BookTicker jitter analysis")
    parser.add_argument("--contract", type=str, default=CONTRACT,
                        help=f"Futures contract (default: {CONTRACT})")
    parser.add_argument("--samples", type=int, default=200,
                        help="Number of messages to collect (default: 200)")
    parser.add_argument("--sync-rounds", type=int, default=10,
                        help="Clock sync rounds (default: 10)")
    args = parser.parse_args()

    print(f"{'=' * 90}")
    print(f"  Gate.io Futures BookTicker Jitter Analysis")
    print(f"  Contract : {args.contract}")
    print(f"  Samples  : {args.samples}")
    print(f"{'=' * 90}")

    # Phase 1: Clock sync
    print(f"\n[Phase 1] Clock Synchronization ({args.sync_rounds} rounds)...")
    async with aiohttp.ClientSession() as session:
        clock = await sync_clock(session, args.sync_rounds)
    print(f"  Offset: {clock['offset_us'] / 1000:+.3f} ms")

    # Phase 2: Collect data
    print(f"\n[Phase 2] Collecting {args.samples} book_ticker updates...")
    samples = await collect_samples(args.contract, clock["offset_us"], args.samples)

    # Phase 3: Analysis
    analysis = analyze_jitter(samples)
    report(analysis, clock, args.contract, args.samples)


if __name__ == "__main__":
    asyncio.run(main())

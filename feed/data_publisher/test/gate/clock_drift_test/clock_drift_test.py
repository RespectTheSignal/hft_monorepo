#!/usr/bin/env python3
"""
Gate.io Clock Drift Monitor

Monitors local-vs-server clock offset over time to detect drift.
If clocks drift significantly between sync intervals, latency measurements
become unreliable. This test quantifies how fast the offset changes.

Performs periodic NTP-style clock sync samples and tracks:
  - Absolute offset at each measurement point
  - Delta (drift) between consecutive measurements
  - Drift rate (μs/s)

Usage:
  python clock_drift_test.py
  python clock_drift_test.py --duration 300 --interval 5
"""

import argparse
import asyncio
import statistics
import time

import aiohttp

REST_URL = "https://api.gateio.ws/api/v4"


async def measure_offset(session: aiohttp.ClientSession) -> dict | None:
    """Single clock offset measurement using X-In-Time/X-Out-Time headers."""
    url = f"{REST_URL}/futures/usdt/tickers"
    params = {"contract": "BTC_USDT"}

    local_send = time.time_ns() / 1_000
    async with session.get(url, params=params) as resp:
        local_recv = time.time_ns() / 1_000
        await resp.read()
        x_in = resp.headers.get("X-In-Time")
        x_out = resp.headers.get("X-Out-Time")
        if not (x_in and x_out):
            return None

        server_recv = float(x_in)
        server_send = float(x_out)
        rtt = (local_recv - local_send) - (server_send - server_recv)
        offset = (server_recv + server_send) / 2 - (local_send + local_recv) / 2

        return {
            "offset_us": offset,
            "rtt_us": rtt,
            "wall_time": time.time(),
        }


async def measure_offset_robust(
    session: aiohttp.ClientSession, sub_rounds: int
) -> dict | None:
    """Take multiple sub-samples and use median for robustness."""
    offsets = []
    rtts = []
    for _ in range(sub_rounds):
        result = await measure_offset(session)
        if result:
            offsets.append(result["offset_us"])
            rtts.append(result["rtt_us"])
        await asyncio.sleep(0.05)

    if not offsets:
        return None
    return {
        "offset_us": statistics.median(offsets),
        "rtt_us": statistics.median(rtts),
        "offset_stdev_us": statistics.stdev(offsets) if len(offsets) > 1 else 0.0,
        "wall_time": time.time(),
        "n": len(offsets),
    }


async def main():
    parser = argparse.ArgumentParser(description="Gate.io clock drift monitor")
    parser.add_argument("--duration", type=int, default=120,
                        help="Total monitoring duration in seconds (default: 120)")
    parser.add_argument("--interval", type=int, default=10,
                        help="Seconds between measurements (default: 10)")
    parser.add_argument("--sub-rounds", type=int, default=5,
                        help="Sub-samples per measurement point (default: 5)")
    args = parser.parse_args()

    num_points = args.duration // args.interval + 1

    print(f"{'=' * 90}")
    print(f"  Gate.io Clock Drift Monitor")
    print(f"  Duration    : {args.duration}s")
    print(f"  Interval    : {args.interval}s")
    print(f"  Sub-rounds  : {args.sub_rounds} per point")
    print(f"  Est. points : {num_points}")
    print(f"{'=' * 90}")

    samples: list[dict] = []
    start_time = time.time()

    print(f"\n  {'#':>4s}  {'Elapsed':>8s}  {'Offset':>10s}  {'RTT':>8s}  {'Delta':>10s}  {'Drift rate':>12s}  {'Stdev':>8s}")
    print(f"  {'-' * 4}  {'-' * 8}  {'-' * 10}  {'-' * 8}  {'-' * 10}  {'-' * 12}  {'-' * 8}")

    async with aiohttp.ClientSession() as session:
        point = 0
        while True:
            elapsed = time.time() - start_time
            if elapsed > args.duration:
                break

            result = await measure_offset_robust(session, args.sub_rounds)
            if result is None:
                print(f"  {point:4d}  {elapsed:7.1f}s  FAILED")
                point += 1
                await asyncio.sleep(args.interval)
                continue

            samples.append(result)
            offset_ms = result["offset_us"] / 1_000
            rtt_ms = result["rtt_us"] / 1_000
            stdev_ms = result["offset_stdev_us"] / 1_000

            # Calculate drift from previous sample
            if len(samples) >= 2:
                prev = samples[-2]
                delta_us = result["offset_us"] - prev["offset_us"]
                dt_s = result["wall_time"] - prev["wall_time"]
                drift_rate = delta_us / dt_s if dt_s > 0 else 0
                delta_ms = delta_us / 1_000
                print(
                    f"  {point:4d}  {elapsed:7.1f}s  {offset_ms:+9.3f}ms  "
                    f"{rtt_ms:7.2f}ms  {delta_ms:+9.3f}ms  "
                    f"{drift_rate:+10.2f}μs/s  {stdev_ms:7.3f}ms"
                )
            else:
                print(
                    f"  {point:4d}  {elapsed:7.1f}s  {offset_ms:+9.3f}ms  "
                    f"{rtt_ms:7.2f}ms  {'--':>10s}  "
                    f"{'--':>12s}  {stdev_ms:7.3f}ms"
                )

            point += 1

            # Wait for next interval
            next_time = start_time + point * args.interval
            sleep_time = next_time - time.time()
            if sleep_time > 0:
                await asyncio.sleep(sleep_time)

    if len(samples) < 2:
        print("\n  Not enough samples for drift analysis.")
        return

    # Final report
    offsets_ms = [s["offset_us"] / 1_000 for s in samples]
    deltas_us = [
        samples[i]["offset_us"] - samples[i - 1]["offset_us"]
        for i in range(1, len(samples))
    ]
    dts = [
        samples[i]["wall_time"] - samples[i - 1]["wall_time"]
        for i in range(1, len(samples))
    ]
    drift_rates = [d / t for d, t in zip(deltas_us, dts) if t > 0]

    total_drift_us = samples[-1]["offset_us"] - samples[0]["offset_us"]
    total_time_s = samples[-1]["wall_time"] - samples[0]["wall_time"]
    avg_drift_rate = total_drift_us / total_time_s if total_time_s > 0 else 0

    print(f"\n{'=' * 90}")
    print(f"  RESULTS  ({len(samples)} points over {total_time_s:.0f}s)")
    print(f"{'=' * 90}")
    print(f"  Offset range  : {min(offsets_ms):+.3f} ~ {max(offsets_ms):+.3f} ms")
    print(f"  Offset stdev  : {statistics.stdev(offsets_ms):.3f} ms")
    print(f"  Total drift   : {total_drift_us / 1000:+.3f} ms over {total_time_s:.0f}s")
    print(f"  Avg drift rate: {avg_drift_rate:+.2f} μs/s")

    if drift_rates:
        print(f"  Max drift rate: {max(drift_rates, key=abs):+.2f} μs/s")
        print(f"  Drift stability: stdev={statistics.stdev(drift_rates):.2f} μs/s")

    print(f"{'=' * 90}")
    print()

    # Interpretation
    abs_rate = abs(avg_drift_rate)
    if abs_rate < 1:
        grade = "EXCELLENT — drift negligible, clock sync every few minutes is sufficient"
    elif abs_rate < 10:
        grade = "GOOD — minor drift, re-sync every 30-60 seconds recommended"
    elif abs_rate < 100:
        grade = "MODERATE — noticeable drift, re-sync every 10-15 seconds recommended"
    else:
        grade = "POOR — significant drift, continuous re-sync needed"

    print(f"  Assessment: {grade}")
    print(f"  At {abs_rate:.1f} μs/s drift, offset error after 60s ≈ {abs_rate * 60 / 1000:.2f} ms")


if __name__ == "__main__":
    asyncio.run(main())

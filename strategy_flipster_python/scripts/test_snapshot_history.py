"""SnapshotHistory / SymbolRingBuffer 단위 + 부하 벤치.

외부 의존 없음. 실행:
    uv run python scripts/test_snapshot_history.py
"""

from __future__ import annotations

import asyncio
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

import numpy as np

from strategy_flipster.market_data.history import (
    SnapshotHistory,
    SnapshotSampler,
    SymbolRingBuffer,
)
from strategy_flipster.market_data.latest_cache import LatestTickerCache
from strategy_flipster.types import BookTicker

FAIL: list[str] = []


def check(name: str, cond: bool, detail: str = "") -> None:
    tag = "✓" if cond else "✗"
    print(f"  {tag} {name}{(' — ' + detail) if detail else ''}")
    if not cond:
        FAIL.append(name)


def hr(title: str) -> None:
    print(f"\n{'=' * 6} {title} {'=' * (60 - len(title))}")


def _mk_ticker(exch: str, sym: str, bid: float, ask: float) -> BookTicker:
    return BookTicker(
        exchange=exch, symbol=sym,
        bid_price=bid, ask_price=ask,
        bid_size=0.0, ask_size=0.0,
        last_price=0.0, mark_price=0.0, index_price=0.0,
        event_time_ms=0, recv_ts_ns=time.time_ns(),
    )


# ── 1. SymbolRingBuffer 기본 ──

def test_ring_buffer_basic() -> None:
    hr("1. SymbolRingBuffer 기본 동작")
    buf = SymbolRingBuffer(capacity=10)
    check("초기 count=0", buf.count == 0)

    for i in range(5):
        buf.append(1000 + i, 100.0 + i, 101.0 + i)
    check("5회 append 후 count=5", buf.count == 5)
    latest = buf.latest()
    check(
        "latest = (1004, 104.0, 105.0, 104.5)",
        latest is not None and latest[0] == 1004 and latest[3] == 104.5,
    )


def test_ring_buffer_shift() -> None:
    hr("2. SymbolRingBuffer shift-back (scratch 가득 차면 memcpy)")
    cap = 10
    buf = SymbolRingBuffer(capacity=cap)
    # scratch = 2*cap = 20. 2*cap까지 쓰면 뒤쪽 cap만 남음.
    for i in range(25):
        buf.append(1000 + i, 100.0 + i, 101.0 + i)
    check(
        f"25 append 후 count가 capacity({cap}) 이상 scratch({2*cap}) 이하",
        cap <= buf.count <= 2 * cap,
        f"count={buf.count}",
    )
    # 가장 최근 샘플은 i=24
    latest = buf.latest()
    check(
        "latest ts = 1024",
        latest is not None and latest[0] == 1024,
    )
    # 가장 오래된 샘플도 shift-back 때문에 사라지지 않고 capacity만큼 유지
    ts_arr = buf.ts[: buf.count]
    check(
        "ts가 chronological (strictly increasing)",
        bool(np.all(np.diff(ts_arr) > 0)),
    )


def test_ring_buffer_searchsorted() -> None:
    hr("3. searchsorted 기반 cutoff 슬라이스")
    buf = SymbolRingBuffer(capacity=100)
    # 1000..1099
    for i in range(100):
        buf.append(1000 + i, 100.0, 101.0)
    mids = buf.mid_slice(cutoff_ns=1050)
    check("cutoff=1050 → 50개", mids.shape[0] == 50)
    mids = buf.mid_slice(cutoff_ns=0)
    check("cutoff=0 → 전체", mids.shape[0] == 100)
    mids = buf.mid_slice(cutoff_ns=2000)
    check("cutoff=미래 → 0개", mids.shape[0] == 0)


# ── 4. SnapshotHistory aggregate helpers ──

def test_history_spread_mean() -> None:
    hr("4. SnapshotHistory.spread_mean")
    hist = SnapshotHistory(interval_sec=0.05, max_age_sec=60.0)
    now = time.time_ns()
    # binance: mid=100, flipster: mid=101.5 (+1.5 spread)
    for i in range(50):
        ts = now - (50 - i) * 1_000_000  # 1ms 간격
        hist.append("binance", "BTCUSDT", ts, 99.5, 100.5)  # mid=100
        hist.append("flipster", "BTCUSDT.PERP", ts, 101.0, 102.0)  # mid=101.5
    diff = hist.spread_mean(
        "flipster", "BTCUSDT.PERP",
        "binance", "BTCUSDT",
        duration_sec=1.0,
    )
    check(f"spread ≈ +1.5 (실측 {diff:.4f})", abs(diff - 1.5) < 1e-9)

    mean_bn = hist.mid_mean("binance", "BTCUSDT", duration_sec=1.0)
    check(f"binance mid_mean=100.0 (실측 {mean_bn})", abs(mean_bn - 100.0) < 1e-9)


# ── 5. SnapshotSampler 스케줄 정확도 + 부하 ──

async def test_sampler_throughput() -> None:
    hr("5. SnapshotSampler 스케줄 정확도 + 부하 벤치")
    cache = LatestTickerCache()
    hist = SnapshotHistory(interval_sec=0.05, max_age_sec=60.0)

    # 400개 심볼 × 2 거래소 = 800 pairs
    for i in range(400):
        cache.update(_mk_ticker("binance", f"SYM{i}USDT", 100.0 + i, 100.1 + i))
        cache.update(_mk_ticker("flipster", f"SYM{i}USDT.PERP", 100.0 + i, 100.1 + i))

    sampler = SnapshotSampler(cache=cache, history=hist, interval_sec=0.05)
    task = asyncio.create_task(sampler.start())

    # 2초간 돌려서 약 40 샘플 수집
    await asyncio.sleep(2.0)
    await sampler.stop()
    try:
        await asyncio.wait_for(task, timeout=1.0)
    except asyncio.TimeoutError:
        task.cancel()

    expected = 40
    samples = sampler.sample_count
    check(
        f"샘플 {samples}회 (기대 ≈ {expected}, ±5 허용)",
        abs(samples - expected) <= 5,
    )
    # 성능: 전체 800 심볼 append 루프 한번 걸린 시간
    print(f"  마지막 sample 루프 소요: {sampler.last_elapsed_us} μs")
    check(
        "1회 sample loop < 5ms (800 심볼)",
        sampler.last_elapsed_us < 5000,
        f"{sampler.last_elapsed_us}μs",
    )

    # 히스토리 크기 검증
    check(
        f"심볼 개수 = 800 (실측 {hist.series_count()})",
        hist.series_count() == 800,
    )
    btc_buf = hist.buffer("binance", "SYM0USDT")
    check(
        f"심볼당 샘플 개수 ≥ 38 (실측 {btc_buf.count if btc_buf else 0})",
        btc_buf is not None and btc_buf.count >= 38,
    )


# ── 6. 전략 조회 벤치 (핫패스 시뮬) ──

def test_query_bench() -> None:
    hr("6. 전략 조회 벤치 (spread_mean × 많은 심볼)")
    hist = SnapshotHistory(interval_sec=0.05, max_age_sec=60.0)
    now = time.time_ns()
    # 200 심볼 × 2 거래소, 1200 샘플 (full)
    for s in range(200):
        for i in range(1200):
            ts = now - (1200 - i) * 50_000_000  # 50ms 간격
            hist.append("binance", f"S{s}", ts, 100.0, 100.1)
            hist.append("flipster", f"S{s}.PERP", ts, 101.0, 101.1)

    import time as _t
    # spread_mean 반복 호출 (전략이 심볼별로 조회하는 시뮬)
    N = 200
    t0 = _t.perf_counter_ns()
    for s in range(N):
        hist.spread_mean("flipster", f"S{s}.PERP", "binance", f"S{s}", duration_sec=30.0)
    elapsed_us = (_t.perf_counter_ns() - t0) / 1000
    per_call_us = elapsed_us / N
    print(f"  {N}회 spread_mean: 총 {elapsed_us:.1f}μs, per-call {per_call_us:.2f}μs")
    check(
        "per-call < 200μs (심볼당)",
        per_call_us < 200.0,
        f"{per_call_us:.2f}μs",
    )


async def main() -> None:
    test_ring_buffer_basic()
    test_ring_buffer_shift()
    test_ring_buffer_searchsorted()
    test_history_spread_mean()
    await test_sampler_throughput()
    test_query_bench()

    hr("결과 요약")
    if FAIL:
        print(f"  실패 {len(FAIL)}개:")
        for name in FAIL:
            print(f"    - {name}")
        sys.exit(1)
    print("  전체 통과")


if __name__ == "__main__":
    asyncio.run(main())

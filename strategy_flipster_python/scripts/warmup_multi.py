"""멀티 심볼 동시 워밍업 — basis / lead-lag / 경제성 랭킹.

양 거래소에 공통으로 있는 모든 USDT perp 심볼에 대해:
  - basis (Flipster mid − Binance mid) 통계
  - 1s lead-lag 회귀 (β_fl, β_bn, fl_share)
  - Flipster 단방향 전략 경제성 (expected_fl_move / breakeven)
를 측정해서 수익성 기준으로 상위 N개 출력.

실행:
    uv run python scripts/warmup_multi.py [duration_sec] [top_n] [fee_bps] [notional]
    예: uv run python scripts/warmup_multi.py 120 30 0.45 20
"""

from __future__ import annotations

import asyncio
import signal
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

import numpy as np

from strategy_flipster.config import load_config
from strategy_flipster.market_data.aggregator import MarketDataAggregator
from strategy_flipster.market_data.flipster_zmq import FlipsterZmqFeed
from strategy_flipster.market_data.history import SnapshotHistory, SnapshotSampler
from strategy_flipster.market_data.latest_cache import LatestTickerCache
from strategy_flipster.market_data.symbol import (
    EXCHANGE_BINANCE,
    EXCHANGE_FLIPSTER,
    is_supported_symbol,
    to_canonical,
)
from strategy_flipster.market_data.zmq_feed import ExchangeZmqFeed
from strategy_flipster.types import BookTicker


def _accept(t: BookTicker) -> bool:
    return is_supported_symbol(t.exchange, t.symbol)


def analyze_pair(
    history: SnapshotHistory,
    canonical: str,
    fl_sym: str,
    bn_sym: str,
    duration: float,
    fee_bps: float,
    notional: float,
) -> dict | None:
    """한 canonical 심볼에 대해 basis + lead-lag + 경제성 계산"""
    diff = history.cross_mid_diff_array(
        EXCHANGE_FLIPSTER, fl_sym, EXCHANGE_BINANCE, bn_sym,
        duration_sec=duration,
    )
    n = diff.size
    if n < 400:  # 최소 20초 샘플 필요
        return None

    mean = float(diff.mean())
    std = float(diff.std())
    if std < 1e-10:
        return None

    fl_mid = history.mid_array(EXCHANGE_FLIPSTER, fl_sym, duration_sec=duration)
    bn_mid = history.mid_array(EXCHANGE_BINANCE, bn_sym, duration_sec=duration)
    common = min(fl_mid.size, bn_mid.size, n)
    if common < 100:
        return None
    fl_mid = fl_mid[-common:]
    bn_mid = bn_mid[-common:]
    diff_a = diff[-common:]

    price = float(fl_mid.mean())
    if price < 1e-9:
        return None
    qty = notional / price
    round_trip_fee = 2 * notional * fee_bps * 1e-4
    breakeven_move = round_trip_fee / qty  # Flipster 가격 단위

    # h=20 (1s) lead-lag
    horizon = 20
    if common <= horizon + 1:
        return None
    b_t = diff_a[:-horizon] - mean
    d_fl = fl_mid[horizon:] - fl_mid[:-horizon]
    d_bn = bn_mid[horizon:] - bn_mid[:-horizon]
    var_b = float(np.var(b_t))
    if var_b < 1e-18:
        return None
    beta_fl = float(np.mean(b_t * d_fl) / var_b)
    beta_bn = float(np.mean(b_t * d_bn) / var_b)
    denom = abs(beta_fl) + abs(beta_bn)
    fl_share = abs(beta_fl) / denom if denom > 1e-18 else 0.0

    # 2σ 이탈 시 예상 Flipster 이동
    expected_fl_move = abs(beta_fl) * 2.0 * std
    econ = expected_fl_move / breakeven_move if breakeven_move > 1e-12 else float("inf")

    fl_spread = history.avg_spread(EXCHANGE_FLIPSTER, fl_sym, duration)
    bn_spread = history.avg_spread(EXCHANGE_BINANCE, bn_sym, duration)

    return {
        "canonical": canonical,
        "n": n,
        "price": price,
        "basis_mean": mean,
        "basis_std": std,
        "beta_fl": beta_fl,
        "beta_bn": beta_bn,
        "fl_share": fl_share,
        "expected_fl_move": expected_fl_move,
        "breakeven": breakeven_move,
        "econ_ratio": econ,
        "fl_spread": fl_spread,
        "bn_spread": bn_spread,
    }


async def main() -> None:
    duration = float(sys.argv[1]) if len(sys.argv) > 1 else 120.0
    top_n = int(sys.argv[2]) if len(sys.argv) > 2 else 30
    fee_bps = float(sys.argv[3]) if len(sys.argv) > 3 else 0.45
    notional = float(sys.argv[4]) if len(sys.argv) > 4 else 20.0

    config = load_config("config.toml")

    feeds: list[object] = [
        FlipsterZmqFeed(zmq_address=config.flipster_feed.zmq_address),
    ]
    for fc in config.exchange_feeds:
        if fc.enabled:
            feeds.append(ExchangeZmqFeed(fc.zmq_address, fc.exchange))

    latest = LatestTickerCache()
    history = SnapshotHistory(
        interval_sec=0.05,
        max_age_sec=max(duration + 30.0, 60.0),
    )
    sampler = SnapshotSampler(latest, history, interval_sec=0.05)
    agg = MarketDataAggregator(feeds, latest_cache=latest, accept=_accept)

    print(f"warmup_multi:")
    print(f"  duration={duration:.0f}s  top_n={top_n}  fee_bps={fee_bps}  notional=${notional}\n")

    await agg.start()
    sampler_task = asyncio.create_task(sampler.start())

    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stop.set)

    t0 = time.monotonic()
    deadline = t0 + duration

    try:
        while not stop.is_set() and time.monotonic() < deadline:
            await asyncio.sleep(10.0)
            elapsed = time.monotonic() - t0
            n_fl = sum(1 for (e, _) in history.keys() if e == EXCHANGE_FLIPSTER)
            n_bn = sum(1 for (e, _) in history.keys() if e == EXCHANGE_BINANCE)
            print(f"  t={elapsed:5.1f}s  fl_symbols={n_fl}  bn_symbols={n_bn}")
    except asyncio.CancelledError:
        pass
    finally:
        await sampler.stop()
        try:
            await asyncio.wait_for(sampler_task, timeout=2.0)
        except (asyncio.TimeoutError, asyncio.CancelledError):
            sampler_task.cancel()
        await agg.stop()

    # ── canonical 별로 페어 수집 ──
    canonical_map: dict[str, dict[str, str]] = {}
    for exch, sym in history.keys():
        c = to_canonical(exch, sym)
        if c is None:
            continue
        canonical_map.setdefault(c, {})[exch] = sym

    pairs_both: list[tuple[str, str, str]] = [
        (c, m[EXCHANGE_FLIPSTER], m[EXCHANGE_BINANCE])
        for c, m in canonical_map.items()
        if EXCHANGE_FLIPSTER in m and EXCHANGE_BINANCE in m
    ]
    print(f"\n공통 심볼(양쪽 존재): {len(pairs_both)}개")

    # ── 각 심볼 분석 ──
    results: list[dict] = []
    for canonical, fl_sym, bn_sym in pairs_both:
        r = analyze_pair(history, canonical, fl_sym, bn_sym, duration, fee_bps, notional)
        if r is not None:
            results.append(r)

    print(f"분석 완료: {len(results)}개 (n≥400, std>0)")

    # 경제성 내림차순
    results.sort(key=lambda r: r["econ_ratio"], reverse=True)

    # ── 테이블 출력 ──
    print(f"\n{'=' * 6} 상위 {top_n}개 (경제성 기준, 2σ 진입, 1s 보유) {'=' * 6}")
    header = (
        f"{'symbol':>14}  {'n':>5}  {'price':>10}  "
        f"{'std':>9}  {'β_fl':>8}  {'fl_sh':>6}  "
        f"{'exp_mv':>9}  {'br_even':>9}  {'econ':>6}"
    )
    print(header)
    print("-" * len(header))
    for r in results[:top_n]:
        print(
            f"{r['canonical']:>14}  "
            f"{r['n']:>5}  {r['price']:>10.5f}  "
            f"{r['basis_std']:>9.5f}  {r['beta_fl']:>+8.4f}  "
            f"{r['fl_share']*100:>5.1f}%  "
            f"{r['expected_fl_move']:>9.5f}  "
            f"{r['breakeven']:>9.5f}  "
            f"{r['econ_ratio']:>6.2f}"
        )

    # 요약 통계
    viable = [r for r in results if r["econ_ratio"] >= 1.5]
    marginal = [r for r in results if 1.0 <= r["econ_ratio"] < 1.5]
    print(f"\n{'=' * 6} 요약 {'=' * 55}")
    print(f"  viable (econ ≥ 1.5):    {len(viable):3d}개")
    print(f"  marginal (1.0 ≤ econ < 1.5): {len(marginal):3d}개")
    print(f"  unviable (< 1.0):       {len([r for r in results if r['econ_ratio'] < 1.0]):3d}개")
    if results:
        avg_fl_share = sum(r["fl_share"] for r in results) / len(results)
        print(f"  avg fl_share:           {avg_fl_share*100:.1f}%")
        # β_fl 분포
        beta_fls = [r["beta_fl"] for r in results]
        print(
            f"  β_fl (1s) distribution: "
            f"min={min(beta_fls):+.4f} max={max(beta_fls):+.4f} "
            f"med={sorted(beta_fls)[len(beta_fls)//2]:+.4f}"
        )


if __name__ == "__main__":
    asyncio.run(main())

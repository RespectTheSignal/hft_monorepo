"""실시간 basis 분포 측정 — 전략 파라미터 캘리브레이션용.

지정 canonical 심볼에 대해 Flipster − Binance mid 차이의 시계열을 N초 동안
기록하고, 분포/분위수/z-score/autocorr/lead-lag 분해/호가 스프레드/
Flipster 단방향 전략 경제성을 출력.

실행:
    uv run python scripts/warmup_basis.py [canonical] [duration_sec] [fee_bps]
    예: uv run python scripts/warmup_basis.py SOL 120 0.45
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
    to_exchange_symbol,
)
from strategy_flipster.market_data.zmq_feed import ExchangeZmqFeed
from strategy_flipster.types import BookTicker


def _accept(t: BookTicker) -> bool:
    return is_supported_symbol(t.exchange, t.symbol)


def _fmt(x: float, digits: int = 5) -> str:
    return f"{x:+.{digits}f}"


async def main() -> None:
    canonical = sys.argv[1].upper() if len(sys.argv) > 1 else "SOL"
    duration = float(sys.argv[2]) if len(sys.argv) > 2 else 120.0
    fee_bps = float(sys.argv[3]) if len(sys.argv) > 3 else 0.45
    # 기본 가정 notional (USD) — 경제성 예시용
    notional = 20.0

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

    fl_sym = to_exchange_symbol(EXCHANGE_FLIPSTER, canonical)
    bn_sym = to_exchange_symbol(EXCHANGE_BINANCE, canonical)

    print(f"warmup: canonical={canonical}")
    print(f"  flipster: {fl_sym}")
    print(f"  binance : {bn_sym}")
    print(f"  duration: {duration:.0f}s\n")

    await agg.start()
    sampler_task = asyncio.create_task(sampler.start())

    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stop.set)

    t0 = time.monotonic()
    deadline = t0 + duration

    # 진행 로그 — 5초마다
    try:
        while not stop.is_set() and time.monotonic() < deadline:
            await asyncio.wait_for(stop.wait(), timeout=5.0) if False else await asyncio.sleep(5.0)
            elapsed = time.monotonic() - t0
            diff = history.cross_mid_diff_array(
                EXCHANGE_FLIPSTER, fl_sym, EXCHANGE_BINANCE, bn_sym,
            )
            if diff.size > 1:
                print(
                    f"  t={elapsed:5.1f}s  n={diff.size:>5}  "
                    f"mean={_fmt(float(diff.mean()))}  "
                    f"std={float(diff.std()):.5f}  "
                    f"min={_fmt(float(diff.min()))}  "
                    f"max={_fmt(float(diff.max()))}"
                )
            else:
                print(f"  t={elapsed:5.1f}s  n={diff.size} (수집 중)")
            if stop.is_set():
                break
    except asyncio.CancelledError:
        pass
    finally:
        await sampler.stop()
        try:
            await asyncio.wait_for(sampler_task, timeout=2.0)
        except (asyncio.TimeoutError, asyncio.CancelledError):
            sampler_task.cancel()
        await agg.stop()

    # ── 최종 분석 ──
    diff = history.cross_mid_diff_array(
        EXCHANGE_FLIPSTER, fl_sym, EXCHANGE_BINANCE, bn_sym,
    )
    if diff.size == 0:
        print("\nno data collected"); return

    mean = float(diff.mean())
    std = float(diff.std())
    vmin = float(diff.min())
    vmax = float(diff.max())

    print(f"\n{'=' * 6} basis 분포 (n={diff.size}) {'=' * 40}")
    print(f"  mean    {_fmt(mean)}")
    print(f"  std     {std:.5f}")
    print(f"  min     {_fmt(vmin)}")
    print(f"  max     {_fmt(vmax)}")
    print(f"  range   {vmax - vmin:.5f}")
    for q in (1, 5, 25, 50, 75, 95, 99):
        p = float(np.percentile(diff, q))
        print(f"  p{q:<3}    {_fmt(p)}")

    # z-score 분포
    if std > 1e-12:
        z = (diff - mean) / std
        print(f"\n{'=' * 6} z-score threshold crossing {'=' * 31}")
        for k in (1.0, 1.5, 2.0, 2.5, 3.0, 4.0):
            pct = float((np.abs(z) > k).mean()) * 100
            print(f"  |z| > {k:.1f}    {pct:6.2f}%  (abs dev ≥ {k*std:.5f})")
    else:
        print("\n  std≈0, z-score 분석 생략")

    # Autocorrelation (lag-1, lag-20=1초, lag-200=10초)
    print(f"\n{'=' * 6} autocorrelation (mean reversion 신호) {'=' * 20}")
    for lag, label in [(1, "50ms"), (20, "1s"), (100, "5s"), (200, "10s")]:
        if diff.size > lag + 1:
            a = diff[:-lag]
            b = diff[lag:]
            if a.std() > 1e-12 and b.std() > 1e-12:
                c = float(np.corrcoef(a, b)[0, 1])
                print(f"  lag {lag:>4} ({label:>5}) : {c:+.4f}")

    # 호가 스프레드
    fl_spread = history.avg_spread(EXCHANGE_FLIPSTER, fl_sym, duration_sec=duration)
    bn_spread = history.avg_spread(EXCHANGE_BINANCE, bn_sym, duration_sec=duration)
    print(f"\n{'=' * 6} 호가 스프레드 (평균) {'=' * 40}")
    print(f"  flipster {fl_sym:20s}: {fl_spread:.5f}")
    print(f"  binance  {bn_sym:20s}: {bn_spread:.5f}")

    # Lead-lag 분해 — basis 이탈 시 누가 움직여서 복귀하는가?
    # Δfl_{t+h} = β_fl × (basis_t − mean)
    # Δbn_{t+h} = β_bn × (basis_t − mean)
    # β_fl이 음수에 가까울수록 "Flipster가 복귀를 주도" → 단방향 전략에 유리
    fl_mid = history.mid_array(EXCHANGE_FLIPSTER, fl_sym, duration_sec=duration)
    bn_mid = history.mid_array(EXCHANGE_BINANCE, bn_sym, duration_sec=duration)

    # align fl/bn to same grid as diff (같은 버퍼 인덱스 만들기)
    # diff 는 cross_mid_diff_array 가 이미 정렬해서 반환함 → 그 크기와 동일한
    # 뒤쪽 구간의 fl/bn을 사용
    n = diff.size
    fl_aligned = fl_mid[-n:] if fl_mid.size >= n else fl_mid
    bn_aligned = bn_mid[-n:] if bn_mid.size >= n else bn_mid
    common = min(fl_aligned.size, bn_aligned.size, n)
    fl_aligned = fl_aligned[-common:]
    bn_aligned = bn_aligned[-common:]
    diff_aligned = diff[-common:]

    print(f"\n{'=' * 6} lead-lag 분해 (basis 복귀 기여도) {'=' * 25}")
    if std > 1e-12 and common > 250:
        basis_dev = diff_aligned - mean
        for horizon, label in [(1, "50ms"), (20, "1s"), (100, "5s")]:
            if common <= horizon + 1:
                continue
            b_t = basis_dev[:-horizon]
            d_fl = fl_aligned[horizon:] - fl_aligned[:-horizon]
            d_bn = bn_aligned[horizon:] - bn_aligned[:-horizon]
            var_b = float(np.var(b_t))
            if var_b < 1e-18:
                continue
            beta_fl = float(np.mean(b_t * d_fl) / var_b)
            beta_bn = float(np.mean(b_t * d_bn) / var_b)
            # revert_frac: basis 가 (β_fl − β_bn) 만큼 돌아옴. 이상적(full revert)이면 −1
            revert = beta_fl - beta_bn
            # flipster 기여 비율: |β_fl| / (|β_fl| + |β_bn|)
            denom = abs(beta_fl) + abs(beta_bn)
            fl_share = abs(beta_fl) / denom if denom > 1e-18 else 0.0
            print(
                f"  h={horizon:>3} ({label:>4}) : "
                f"β_fl={beta_fl:+.4f}  β_bn={beta_bn:+.4f}  "
                f"revert={revert:+.4f}  fl_share={fl_share*100:5.1f}%"
            )
    else:
        print("  데이터 부족 또는 std≈0")

    # 경제성 (Flipster 단방향 기준)
    price_avg = float(fl_aligned.mean()) if fl_aligned.size else 0.0
    qty = notional / price_avg if price_avg > 0 else 0.0
    fee_per_trade = notional * fee_bps * 1e-4   # bps → ratio
    round_trip_fee = 2 * fee_per_trade
    breakeven_move = round_trip_fee / qty if qty > 0 else float("inf")

    print(f"\n{'=' * 6} 경제성 (단방향 Flipster, notional=${notional:.0f}) {'=' * 12}")
    print(f"  SOL avg price         : ${price_avg:.4f}")
    print(f"  position qty          : {qty:.6f}")
    print(f"  fee per trade         : ${fee_per_trade:.5f} ({fee_bps:.2f} bp)")
    print(f"  round-trip fee        : ${round_trip_fee:.5f}  (= {2*fee_bps:.2f} bp of notional)")
    print(f"  break-even fl price move: ${breakeven_move:.5f}")

    if std > 1e-12:
        k_in = 2.0
        k_out = 0.5
        k_stop = 4.0
        dev_at_kin = k_in * std

        # Flipster 쪽 기대 이동 ≈ |β_fl(1s)| × dev_at_kin
        # (위 loop 에서 계산한 h=20 결과 재계산)
        expected_fl_move = 0.0
        if common > 21:
            b_t = (diff_aligned - mean)[:-20]
            d_fl = fl_aligned[20:] - fl_aligned[:-20]
            var_b = float(np.var(b_t))
            if var_b > 1e-18:
                beta_fl_1s = float(np.mean(b_t * d_fl) / var_b)
                expected_fl_move = abs(beta_fl_1s) * dev_at_kin

        print(f"\n{'=' * 6} 권장 초기 파라미터 {'=' * 42}")
        print(f"  basis_mean (복귀 목표)  : {_fmt(mean)}")
        print(f"  basis_std               : {std:.5f}")
        print(f"  k_in  = {k_in:.1f}   threshold = {k_in*std:.5f}")
        print(f"  k_out = {k_out:.1f}   threshold = {k_out*std:.5f}")
        print(f"  k_stop= {k_stop:.1f}   threshold = {k_stop*std:.5f}")
        print(f"\n  예상 Flipster 이동 (1s, |β_fl|·2σ): {expected_fl_move:.5f}")
        print(f"  break-even 필요 이동            : {breakeven_move:.5f}")
        if expected_fl_move > breakeven_move * 1.5:
            print(f"  ✓ 예상 이동 ≥ 1.5× breakeven — 수익성 있음")
        elif expected_fl_move > breakeven_move:
            print(f"  ~ 예상 이동 > breakeven 이지만 여유 부족")
        else:
            ratio = expected_fl_move / breakeven_move if breakeven_move > 0 else 0
            print(f"  ✗ 예상 이동 < breakeven ({ratio:.2f}x) — 현재 파라미터로는 손실")


if __name__ == "__main__":
    asyncio.run(main())

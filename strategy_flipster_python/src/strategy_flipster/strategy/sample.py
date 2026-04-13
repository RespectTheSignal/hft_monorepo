"""샘플 전략 — 구조 데모용.

Binance-Flipster 스프레드 모니터링 전략.
실제 매매 로직은 비어 있음 — 인터페이스 사용 예시.
"""

from __future__ import annotations

import time

import structlog

from strategy_flipster.market_data.history import SnapshotHistory
from strategy_flipster.market_data.latest_cache import LatestTickerCache
from strategy_flipster.market_data.stats_cache import MarketStatsCache
from strategy_flipster.types import BookTicker, OrderRequest
from strategy_flipster.user_data.state import UserState

logger = structlog.get_logger(__name__)


class SampleStrategy:
    """Binance/Flipster 스프레드 모니터링 — 샘플 구현"""

    def __init__(self) -> None:
        self._last_log_ns: int = 0

    async def on_book_ticker(
        self,
        ticker: BookTicker,
        state: UserState,
        latest: LatestTickerCache,
        market_stats: MarketStatsCache,
        history: SnapshotHistory,
    ) -> list[OrderRequest]:
        # 주기적 로깅 (1초마다)
        now = time.time_ns()
        if now - self._last_log_ns > 1_000_000_000:
            self._last_log_ns = now
            # SOL USDT-perp: Flipster("SOLUSDT.PERP") vs Binance("SOL_USDT")
            fl_sym = "SOLUSDT.PERP"
            bn_sym = "SOL_USDT"
            spread_30s = history.spread_mean(
                "flipster", fl_sym,
                "binance", bn_sym,
                duration_sec=30.0,
            )
            fl_latest = history.latest("flipster", fl_sym)
            bn_latest = history.latest("binance", bn_sym)
            fl_mid = fl_latest[3] if fl_latest else 0.0
            bn_mid = bn_latest[3] if bn_latest else 0.0
            logger.info(
                "strategy_tick",
                latest_symbols=len(latest),
                positions=len(state.positions),
                stats_symbols=len(market_stats),
                history_series=history.series_count(),
                history_samples=len(history),
                sol_fl_mid=round(fl_mid, 4),
                sol_bn_mid=round(bn_mid, 4),
                sol_fl_minus_bn=round(fl_mid - bn_mid, 4),
                sol_spread_30s_avg=round(spread_30s, 4),
            )

        return []

    async def on_timer(
        self,
        state: UserState,
        latest: LatestTickerCache,
        market_stats: MarketStatsCache,
        history: SnapshotHistory,
    ) -> list[OrderRequest]:
        return []

    async def on_start(
        self,
        state: UserState,
        latest: LatestTickerCache,
        market_stats: MarketStatsCache,
        history: SnapshotHistory,
    ) -> None:
        logger.info("sample_strategy_started")

    async def on_stop(self) -> None:
        logger.info("sample_strategy_stopped")

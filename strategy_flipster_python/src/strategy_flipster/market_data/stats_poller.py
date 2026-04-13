"""FlipsterMarketStatsPoller — 전체 심볼 ticker 주기 풀링.

/api/v1/market/ticker (심볼 필터 없음) 를 N초 간격으로 호출.
응답 배열을 파싱해 MarketStatsCache 갱신.
"""

from __future__ import annotations

import asyncio
import time
from typing import Any

import structlog

from strategy_flipster.execution.rest_client import FlipsterExecutionClient
from strategy_flipster.market_data.stats_cache import MarketStatsCache
from strategy_flipster.types import MarketStats

logger = structlog.get_logger(__name__)


def _to_float(raw: Any, default: float = 0.0) -> float:
    if raw is None:
        return default
    try:
        return float(raw)
    except (TypeError, ValueError):
        return default


def _to_int(raw: Any, default: int = 0) -> int:
    if raw is None:
        return default
    try:
        return int(raw)
    except (TypeError, ValueError):
        return default


def parse_ticker_row(row: dict) -> MarketStats | None:
    symbol = row.get("symbol")
    if not isinstance(symbol, str) or not symbol:
        return None
    return MarketStats(
        symbol=symbol,
        bid_price=_to_float(row.get("bidPrice")),
        ask_price=_to_float(row.get("askPrice")),
        last_price=_to_float(row.get("lastPrice")),
        mark_price=_to_float(row.get("markPrice")),
        index_price=_to_float(row.get("indexPrice")),
        funding_rate=_to_float(row.get("fundingRate")),
        next_funding_time_ns=_to_int(row.get("nextFundingTime")),
        open_interest=_to_float(row.get("openInterest")),
        volume_24h=_to_float(row.get("volume24h")),
        turnover_24h=_to_float(row.get("turnover24h")),
        price_change_24h=_to_float(row.get("priceChange24h")),
        updated_ns=time.time_ns(),
    )


class FlipsterMarketStatsPoller:
    """전체 perpetual ticker 주기 풀링.

    단일 REST 호출로 N개 심볼 수집 → MarketStatsCache 갱신.
    """

    def __init__(
        self,
        client: FlipsterExecutionClient,
        cache: MarketStatsCache,
        interval_sec: float = 10.0,
    ) -> None:
        self._client: FlipsterExecutionClient = client
        self._cache: MarketStatsCache = cache
        self._interval: float = interval_sec
        self._running: bool = False

    async def poll_once(self) -> int:
        """1회 풀링. 성공 시 갱신된 심볼 수 반환."""
        rows = await self._client.get_all_tickers()
        count = 0
        for row in rows:
            if not isinstance(row, dict):
                continue
            stats = parse_ticker_row(row)
            if stats is not None:
                self._cache.update(stats)
                count += 1
        self._cache.mark_polled(count)
        return count

    async def start(self) -> None:
        """주기 풀링 루프 — cancellation으로 종료"""
        self._running = True
        logger.info("market_stats_poller_started", interval_sec=self._interval)
        while self._running:
            try:
                t0 = time.monotonic()
                count = await self.poll_once()
                elapsed_ms = (time.monotonic() - t0) * 1000
                logger.info(
                    "market_stats_polled",
                    count=count,
                    elapsed_ms=round(elapsed_ms, 1),
                )
            except asyncio.CancelledError:
                break
            except Exception:
                logger.exception("market_stats_poll_failed")
            try:
                await asyncio.sleep(self._interval)
            except asyncio.CancelledError:
                break
        logger.info("market_stats_poller_stopped")

    async def stop(self) -> None:
        self._running = False

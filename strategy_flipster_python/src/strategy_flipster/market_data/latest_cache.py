"""LatestTickerCache — (거래소, 심볼)별 최신 BookTicker 라이브 캐시.

aggregator가 매 수신마다 갱신. Strategy와 SnapshotSampler가 읽음.
asyncio 단일 스레드 전제.

Rust: HashMap<(String, String), BookTicker>
"""

from __future__ import annotations

from collections.abc import ItemsView

from strategy_flipster.types import BookTicker


class LatestTickerCache:
    """(exchange, symbol) → 최신 BookTicker"""

    __slots__ = ("_data",)

    def __init__(self) -> None:
        self._data: dict[tuple[str, str], BookTicker] = {}

    def update(self, ticker: BookTicker) -> None:
        self._data[(ticker.exchange, ticker.symbol)] = ticker

    def get(self, exchange: str, symbol: str) -> BookTicker | None:
        return self._data.get((exchange, symbol))

    def items(self) -> ItemsView[tuple[str, str], BookTicker]:
        """dict_items 뷰 — 리스트 할당 없음"""
        return self._data.items()

    def __len__(self) -> int:
        return len(self._data)

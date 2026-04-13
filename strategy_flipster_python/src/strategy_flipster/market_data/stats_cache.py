"""MarketStatsCache — 주기적 ticker 폴링 결과 저장.

asyncio 단일 스레드에서 사용. Rust: Arc<RwLock<HashMap<String, MarketStats>>>.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field

from strategy_flipster.types import MarketStats


@dataclass
class MarketStatsCache:
    """심볼 → 최신 MarketStats 매핑"""

    stats: dict[str, MarketStats] = field(default_factory=dict)
    last_poll_ns: int = 0
    last_poll_count: int = 0

    def update(self, stats: MarketStats) -> None:
        self.stats[stats.symbol] = stats

    def mark_polled(self, count: int) -> None:
        self.last_poll_ns = time.time_ns()
        self.last_poll_count = count

    def get(self, symbol: str) -> MarketStats | None:
        return self.stats.get(symbol)

    def symbols(self) -> list[str]:
        return list(self.stats.keys())

    def __len__(self) -> int:
        return len(self.stats)

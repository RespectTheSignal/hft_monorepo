"""Clock 추상 — 라이브 vs 시뮬레이션 시간 소스.

라이브: LiveClock.now_ns() = time.time_ns()
백테스트: SimClock.now_ns() = 현재 이벤트 타임스탬프

SnapshotHistory 와 전략 코드가 동일한 인터페이스로 현재 시각을 조회하도록
주입 가능하게 만든 것. Rust: trait Clock { fn now_ns(&self) -> i64; }
"""

from __future__ import annotations

import time
from typing import Protocol


class Clock(Protocol):
    """현재 시각 (ns) 을 반환하는 추상 시계"""

    def now_ns(self) -> int:
        ...


class LiveClock:
    """실시간 시계 — time.time_ns() 래퍼"""

    __slots__ = ()

    def now_ns(self) -> int:
        return time.time_ns()


class SimClock:
    """시뮬 시계 — 외부에서 set 으로 현재 시각 지정"""

    __slots__ = ("_now",)

    def __init__(self, start_ns: int = 0) -> None:
        self._now: int = start_ns

    def now_ns(self) -> int:
        return self._now

    def set(self, ns: int) -> None:
        self._now = ns

    def advance(self, delta_ns: int) -> None:
        self._now += delta_ns

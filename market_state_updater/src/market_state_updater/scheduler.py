"""Schedule dataclass + tick 디스패처.

각 Schedule = "이 job 을 cadence_secs 마다 1번 돌린다" 의 단위.
main loop 가 짧게 (tick_interval_secs, default 1s) sleep 하면서 due 한 것만 실행.

이전 'cycle + period' 모델 (interval_secs × period_count) 에서 절대 시간 cadence 모델로
변경 — schedule 별 다른 cadence 가능, 윈도우 길이에 맞는 갱신 주기 가능.
"""

from __future__ import annotations

import math
from collections.abc import Callable
from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class Schedule:
    name: str
    cadence_secs: float           # 이 schedule 의 갱신 주기 (초)
    run: Callable[[], bool]       # 미리 bound 된 closure


@dataclass(frozen=True, slots=True)
class TickResult:
    due: int
    ok: int


def tick(
    schedules: list[Schedule],
    now: float,
    last_run_at: dict[str, float],
) -> TickResult:
    """now 시점에 due 한 schedule 들 직렬 실행. last_run_at 은 호출자가 관리 (mutable).

    schedule 마다 (now - last_run_at[name]) >= cadence_secs 면 due.
    """
    due = 0
    ok = 0
    for s in schedules:
        if s.cadence_secs <= 0:
            continue
        # 처음 보는 schedule (last_run 없음) 은 즉시 due — 데몬 시작 직후
        # 모든 metric 1번 채우게 (이후엔 cadence 따라).
        last = last_run_at.get(s.name, -math.inf)
        if now - last < s.cadence_secs:
            continue
        due += 1
        if s.run():
            ok += 1
        last_run_at[s.name] = now
    return TickResult(due=due, ok=ok)

"""Schedule dataclass + 단일 cycle 디스패처.

각 Schedule = "이 job 을 매 N cycle 마다 1번 돌린다" 의 단위.
main 의 build_schedules() 가 활성 job 들을 평면화해서 list[Schedule] 를 만들면,
run_cycle() 이 run_count 를 보고 due 한 것만 호출.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class Schedule:
    name: str  # logging / 식별용
    period: int  # 매 N cycle 마다 1회 (1 = 매 cycle)
    run: Callable[[], bool]  # 미리 bound 된 closure


@dataclass(frozen=True, slots=True)
class CycleResult:
    due: int
    ok: int


def run_cycle(schedules: list[Schedule], run_count: int) -> CycleResult:
    """이번 cycle 에 due 한 schedule 들을 직렬 실행. 각 run() 의 bool 을 ok 로 집계."""
    due = 0
    ok = 0
    for s in schedules:
        if s.period <= 0 or run_count % s.period != 0:
            continue
        due += 1
        if s.run():
            ok += 1
    return CycleResult(due=due, ok=ok)

"""Schedule dataclass + tick 디스패처.

각 Schedule = "이 job 을 cadence_secs 마다 1번 돌린다" 의 단위.
main loop 가 짧게 (tick_interval_secs, default 1s) sleep 하면서 due 한 것만 실행.

이전 'cycle + period' 모델 (interval_secs × period_count) 에서 절대 시간 cadence 모델로
변경 — schedule 별 다른 cadence 가능, 윈도우 길이에 맞는 갱신 주기 가능.
"""

from __future__ import annotations

import math
from collections.abc import Callable
from concurrent.futures import Executor
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


def stagger_initial_runs(
    schedules: list[Schedule],
    last_run_at: dict[str, float],
    start_now: float,
    stagger_step_secs: float,
) -> None:
    """첫 tick burst 회피.

    각 schedule 의 last_run_at 을 (start - cadence + offset_i) 로 설정해서
    next_due = start + offset_i 가 되게 함.

    offset_i = min(i × stagger_step_secs, cadence_secs / 2)
      - cadence 짧은 schedule 의 첫 실행이 너무 늦어지지 않게 cap (cadence/2).
      - stagger_step_secs == 0 이면 비활성 (모든 schedule 즉시 due — 이전 동작).

    LCM 시점의 동기화 burst 도 자연 분산됨 (각 schedule 의 first_run 시점이 다르니).
    """
    if stagger_step_secs <= 0:
        return
    for i, s in enumerate(schedules):
        offset = min(i * stagger_step_secs, s.cadence_secs / 2)
        last_run_at[s.name] = start_now - s.cadence_secs + offset


def tick(
    schedules: list[Schedule],
    now: float,
    last_run_at: dict[str, float],
    executor: Executor | None = None,
) -> TickResult:
    """now 시점에 due 한 schedule 들 실행. last_run_at 은 호출자가 관리 (mutable).

    executor 가 주어지면 due schedule 들을 병렬 실행 (I/O 바운드 — QuestDB HTTP).
    None 이면 직렬 (test/once 모드).

    last_run_at 은 submit 전에 갱신 — 같은 schedule 가 동시에 두 번 trigger 되지 않게.
    """
    due_list: list[Schedule] = []
    for s in schedules:
        if s.cadence_secs <= 0:
            continue
        last = last_run_at.get(s.name, -math.inf)
        if now - last < s.cadence_secs:
            continue
        due_list.append(s)
        last_run_at[s.name] = now  # submit 전에 갱신 → 다음 tick 에서 재실행 방지

    if not due_list:
        return TickResult(due=0, ok=0)

    if executor is None:
        ok = sum(1 for s in due_list if s.run())
        return TickResult(due=len(due_list), ok=ok)

    futures = [executor.submit(s.run) for s in due_list]
    ok = sum(1 for f in futures if f.result())
    return TickResult(due=len(due_list), ok=ok)

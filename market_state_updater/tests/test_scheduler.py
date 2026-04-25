from __future__ import annotations

from market_state_updater.scheduler import Schedule, run_cycle


def _make(name: str, period: int, ok: bool = True) -> Schedule:
    return Schedule(name=name, period=period, run=lambda: ok)


def test_run_cycle_skips_when_period_does_not_divide() -> None:
    schedules = [_make("every", 1), _make("five", 5), _make("ten", 10)]

    # cycle 0: 1, 5, 10 모두 % == 0
    r = run_cycle(schedules, 0)
    assert r.due == 3
    assert r.ok == 3

    # cycle 1: every 만
    r = run_cycle(schedules, 1)
    assert r.due == 1

    # cycle 5: every + five
    r = run_cycle(schedules, 5)
    assert r.due == 2

    # cycle 10: every + five + ten
    r = run_cycle(schedules, 10)
    assert r.due == 3


def test_run_cycle_counts_failures() -> None:
    r = run_cycle([_make("ok", 1, ok=True), _make("fail", 1, ok=False)], 0)
    assert r.due == 2
    assert r.ok == 1


def test_run_cycle_invocation_count() -> None:
    """5 cycle 동안 period=1 은 5번, period=5 는 1번 (cycle 0)."""
    n_every = 0
    n_five = 0

    def every() -> bool:
        nonlocal n_every
        n_every += 1
        return True

    def five() -> bool:
        nonlocal n_five
        n_five += 1
        return True

    schedules = [
        Schedule(name="every", period=1, run=every),
        Schedule(name="five", period=5, run=five),
    ]
    for cycle in range(5):
        run_cycle(schedules, cycle)
    assert n_every == 5
    assert n_five == 1  # cycle 0 만

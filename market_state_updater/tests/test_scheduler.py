from __future__ import annotations

from market_state_updater.scheduler import Schedule, tick


def _make(name: str, cadence_secs: float, ok: bool = True) -> Schedule:
    return Schedule(name=name, cadence_secs=cadence_secs, run=lambda: ok)


def test_first_tick_triggers_all_due() -> None:
    """last_run_at 이 비어있으면 (now - 0) >= cadence 라 모두 due."""
    schedules = [_make("a", 5), _make("b", 60), _make("c", 1)]
    last_run: dict[str, float] = {}
    r = tick(schedules, now=1000.0, last_run_at=last_run)
    assert r.due == 3
    assert r.ok == 3
    assert set(last_run.keys()) == {"a", "b", "c"}


def test_subsequent_tick_only_due_ones() -> None:
    s_fast = _make("fast", 5)
    s_slow = _make("slow", 60)
    last_run: dict[str, float] = {"fast": 1000.0, "slow": 1000.0}

    # 3 초 후 — 둘 다 not due
    r = tick([s_fast, s_slow], now=1003.0, last_run_at=last_run)
    assert r.due == 0

    # 5 초 후 — fast 만 due (cadence 5s)
    r = tick([s_fast, s_slow], now=1005.0, last_run_at=last_run)
    assert r.due == 1
    assert last_run["fast"] == 1005.0
    assert last_run["slow"] == 1000.0  # 갱신 안 됨

    # 60 초 후 — 둘 다 due
    r = tick([s_fast, s_slow], now=1065.0, last_run_at=last_run)
    assert r.due == 2


def test_tick_counts_failures() -> None:
    last_run: dict[str, float] = {}
    r = tick([_make("ok", 1, ok=True), _make("fail", 1, ok=False)], 100.0, last_run)
    assert r.due == 2
    assert r.ok == 1


def test_tick_skips_zero_cadence() -> None:
    """cadence_secs == 0 은 disabled — skip (디버그/테스트용)."""
    last_run: dict[str, float] = {}
    r = tick([_make("zero", 0)], 100.0, last_run)
    assert r.due == 0


def test_tick_invocation_pattern_over_time() -> None:
    """0 ~ 30s 동안 cadence=10 인 schedule 은 4 번 (t=0, 10, 20, 30)."""
    n_runs = 0

    def run() -> bool:
        nonlocal n_runs
        n_runs += 1
        return True

    schedules = [Schedule(name="x", cadence_secs=10.0, run=run)]
    last_run: dict[str, float] = {}
    for t in range(0, 31):
        tick(schedules, now=float(t), last_run_at=last_run)
    assert n_runs == 4  # 0, 10, 20, 30

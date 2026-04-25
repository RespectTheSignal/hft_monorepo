from __future__ import annotations

from market_state_updater.scheduler import Schedule, stagger_initial_runs, tick


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


# ---- stagger_initial_runs ----


def test_stagger_disabled_when_zero() -> None:
    """stagger_step_secs == 0 → no-op (모든 schedule 즉시 due)."""
    schedules = [_make("a", 5), _make("b", 60), _make("c", 1)]
    last_run: dict[str, float] = {}
    stagger_initial_runs(schedules, last_run, start_now=100.0, stagger_step_secs=0)
    assert last_run == {}
    # 첫 tick 에 모두 due (last_run -inf 폴백)
    r = tick(schedules, now=100.0, last_run_at=last_run)
    assert r.due == 3


def test_stagger_offsets_first_run() -> None:
    schedules = [_make("a", 60), _make("b", 60), _make("c", 60)]
    last_run: dict[str, float] = {}
    stagger_initial_runs(schedules, last_run, start_now=1000.0, stagger_step_secs=0.5)
    # offset_i = i * 0.5 → first_due_i = start + i*0.5 = 1000, 1000.5, 1001
    assert last_run["a"] == 1000.0 - 60 + 0
    assert last_run["b"] == 1000.0 - 60 + 0.5
    assert last_run["c"] == 1000.0 - 60 + 1.0


def test_stagger_caps_offset_at_half_cadence() -> None:
    """offset 이 cadence/2 보다 크면 cap — 짧은 cadence 가 너무 늦지 않게."""
    # cadence=2 인 schedule 의 cap = 1. step=10 이라도 offset 1 로 cap.
    s = _make("short", cadence_secs=2)
    last_run: dict[str, float] = {}
    stagger_initial_runs([s, s, s], last_run, start_now=1000.0, stagger_step_secs=10.0)
    # 모두 cap (cadence/2 = 1) 적용
    assert last_run["short"] == 1000.0 - 2 + 1.0


def test_stagger_first_tick_only_first_schedule_due() -> None:
    """stagger 후 t=start 시점엔 schedule[0] 만 due, 나머지는 delay."""
    schedules = [_make("a", 60), _make("b", 60), _make("c", 60)]
    last_run: dict[str, float] = {}
    start = 1000.0
    stagger_initial_runs(schedules, last_run, start_now=start, stagger_step_secs=0.5)
    # t = start: a (offset 0) 만 due
    r = tick(schedules, now=start, last_run_at=last_run)
    assert r.due == 1
    # t = start + 0.5: b 도 due (a 는 마지막 실행이 t=start 라 cadence 60 안 지남)
    r = tick(schedules, now=start + 0.5, last_run_at=last_run)
    assert r.due == 1
    # t = start + 1.0: c 도 due
    r = tick(schedules, now=start + 1.0, last_run_at=last_run)
    assert r.due == 1

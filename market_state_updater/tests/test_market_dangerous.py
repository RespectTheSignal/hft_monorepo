from __future__ import annotations

import json
from unittest.mock import MagicMock

from market_state_updater.jobs import market_dangerous


# ---- query / parse ----


def test_build_count_query_uses_window() -> None:
    sql = market_dangerous.build_count_query("gate_bookticker", 60)
    assert "FROM gate_bookticker" in sql
    assert "dateadd('s', -60, now())" in sql
    assert "count()" in sql


def test_parse_count_handles_empty() -> None:
    assert market_dangerous.parse_count({"dataset": []}) == 0
    assert market_dangerous.parse_count({"dataset": [[None]]}) == 0


def test_parse_count_value() -> None:
    assert market_dangerous.parse_count({"dataset": [[12345]]}) == 12345


# ---- MarketDangerousJob ----


def _make_job(
    absolute_threshold: int = 150_000,
    window_secs: int = 60,
    sticky_secs: int = 600,
    notifier=None,
):
    return market_dangerous.MarketDangerousJob(
        primary_table="gate_bookticker",
        compare_table="binance_bookticker",
        absolute_threshold=absolute_threshold,
        window_secs=window_secs,
        sticky_secs=sticky_secs,
        redis_key="test:md",
        notifier=notifier,
    )


def _patch_counts(monkeypatch, primary_count: int, compare_count: int) -> None:
    """build_count_query 의 SQL 에 따라 다른 결과 반환."""
    def fake(_url, sql):
        if "gate_bookticker" in sql:
            return {"dataset": [[primary_count]]}
        return {"dataset": [[compare_count]]}

    monkeypatch.setattr(market_dangerous, "questdb_exec", fake)


def test_evaluate_rules_safe() -> None:
    job = _make_job()
    assert job._evaluate_rules(primary=100, compare=200) == []


def test_evaluate_rules_gate_exceeds_compare() -> None:
    job = _make_job()
    tags = job._evaluate_rules(primary=300, compare=200)
    assert market_dangerous.TAG_GATE_EXCEEDS in tags
    assert market_dangerous.TAG_HIGH_RATE not in tags


def test_evaluate_rules_high_rate() -> None:
    job = _make_job(absolute_threshold=100)
    tags = job._evaluate_rules(primary=200, compare=500)  # gate < binance 라 첫 룰 X
    assert market_dangerous.TAG_HIGH_RATE in tags
    assert market_dangerous.TAG_GATE_EXCEEDS not in tags


def test_evaluate_rules_both() -> None:
    job = _make_job(absolute_threshold=100)
    tags = job._evaluate_rules(primary=300, compare=200)
    assert market_dangerous.TAG_GATE_EXCEEDS in tags
    assert market_dangerous.TAG_HIGH_RATE in tags


def test_run_safe_when_neither_rule(monkeypatch) -> None:
    job = _make_job(absolute_threshold=150_000)
    _patch_counts(monkeypatch, primary_count=5_000, compare_count=10_000)
    redis_mock = MagicMock()
    assert job.run("http://q", redis_mock) is True

    blob = json.loads(redis_mock.set.call_args[0][1])
    assert blob["value"] is False
    assert blob["tags"] == []
    assert blob["primary_count"] == 5_000
    assert blob["compare_count"] == 10_000
    assert job.last_dangerous is False


def test_run_dangerous_via_gate_exceeds(monkeypatch) -> None:
    job = _make_job(absolute_threshold=150_000)
    _patch_counts(monkeypatch, primary_count=20_000, compare_count=10_000)
    redis_mock = MagicMock()
    job.run("http://q", redis_mock)

    blob = json.loads(redis_mock.set.call_args[0][1])
    assert blob["value"] is True
    assert market_dangerous.TAG_GATE_EXCEEDS in blob["tags"]
    assert market_dangerous.TAG_HIGH_RATE not in blob["tags"]
    assert blob["sticky_until_ms"] > blob["updated_at_ms"]


def test_run_dangerous_via_absolute_threshold(monkeypatch) -> None:
    job = _make_job(absolute_threshold=150_000)
    _patch_counts(monkeypatch, primary_count=200_000, compare_count=300_000)
    redis_mock = MagicMock()
    job.run("http://q", redis_mock)

    blob = json.loads(redis_mock.set.call_args[0][1])
    assert blob["value"] is True
    assert market_dangerous.TAG_HIGH_RATE in blob["tags"]


def test_sticky_keeps_dangerous_after_drop(monkeypatch) -> None:
    job = _make_job(sticky_secs=600)
    # 첫 호출 trigger, 두 번째 safe
    counts = iter([(20_000, 10_000), (5_000, 10_000)])

    def fake(_url, sql):
        return {"dataset": [[next_p_or_c(sql)]]}

    state = {"current": next(counts)}

    def next_p_or_c(sql):
        p, c = state["current"]
        return p if "gate_bookticker" in sql else c

    monkeypatch.setattr(market_dangerous, "questdb_exec", fake)
    redis_mock = MagicMock()
    job.run("http://q", redis_mock)
    state["current"] = next(counts)
    job.run("http://q", redis_mock)

    blob = json.loads(redis_mock.set.call_args[0][1])
    assert blob["value"] is True
    assert blob["primary_count"] == 5_000  # 실제는 safe count
    assert blob["tags"] == []  # 현재 trigger 룰 없음 (sticky 만)


def test_sliding_sticky_extends_timer(monkeypatch) -> None:
    job = _make_job(sticky_secs=600)
    _patch_counts(monkeypatch, primary_count=20_000, compare_count=10_000)
    redis_mock = MagicMock()
    job.run("http://q", redis_mock)
    first_until = job.sticky_until_ms
    import time as _t
    _t.sleep(0.05)
    job.run("http://q", redis_mock)
    second_until = job.sticky_until_ms
    assert second_until > first_until


def test_telegram_called_on_entry_only(monkeypatch) -> None:
    notifier = MagicMock()
    notifier.enabled = True
    job = _make_job(absolute_threshold=150_000, notifier=notifier)
    _patch_counts(monkeypatch, primary_count=20_000, compare_count=10_000)
    redis_mock = MagicMock()

    job.run("http://q", redis_mock)
    job.run("http://q", redis_mock)
    job.run("http://q", redis_mock)
    assert notifier.send.call_count == 1   # entry 만, dedup


def test_telegram_message_includes_tags(monkeypatch) -> None:
    notifier = MagicMock()
    notifier.enabled = True
    job = _make_job(absolute_threshold=10_000, notifier=notifier)
    _patch_counts(monkeypatch, primary_count=20_000, compare_count=10_000)
    redis_mock = MagicMock()
    job.run("http://q", redis_mock)

    msg = notifier.send.call_args[0][0]
    assert market_dangerous.TAG_GATE_EXCEEDS in msg
    assert market_dangerous.TAG_HIGH_RATE in msg


def test_telegram_not_called_when_safe(monkeypatch) -> None:
    notifier = MagicMock()
    notifier.enabled = True
    job = _make_job(notifier=notifier)
    _patch_counts(monkeypatch, primary_count=5_000, compare_count=10_000)
    redis_mock = MagicMock()
    job.run("http://q", redis_mock)
    assert notifier.send.call_count == 0

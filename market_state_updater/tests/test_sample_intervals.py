from __future__ import annotations

from collections.abc import Iterator

import pytest

from market_state_updater.jobs.common import (
    DEFAULT_SAMPLE_INTERVALS,
    parse_sample_interval_overrides,
    sample_interval_for_window,
    set_sample_interval_overrides,
)


@pytest.fixture(autouse=True)
def reset_overrides() -> Iterator[None]:
    """test 끼리 process-wide state 가 새지 않게 매번 reset."""
    set_sample_interval_overrides({})
    yield
    set_sample_interval_overrides({})


def test_default_intervals_match_user_spec() -> None:
    assert DEFAULT_SAMPLE_INTERVALS == {
        1: "100T",
        5: "200T",
        10: "200T",
        30: "500T",
        60: "1s",
        240: "5s",
        720: "10s",
    }


def test_sample_interval_for_known_windows() -> None:
    for w, expected in DEFAULT_SAMPLE_INTERVALS.items():
        assert sample_interval_for_window(w) == expected


def test_sample_interval_fallback_unknown_window() -> None:
    assert sample_interval_for_window(0) == "100T"  # ≤1
    assert sample_interval_for_window(45) == "1s"  # 30~120
    assert sample_interval_for_window(1000) == "5s"  # >120


def test_overrides_take_precedence() -> None:
    set_sample_interval_overrides({1: "1s", 60: "30s"})
    assert sample_interval_for_window(1) == "1s"
    assert sample_interval_for_window(60) == "30s"
    assert sample_interval_for_window(5) == DEFAULT_SAMPLE_INTERVALS[5]  # untouched


def test_overrides_replaced_not_merged() -> None:
    """set_sample_interval_overrides 는 누적이 아니라 replace."""
    set_sample_interval_overrides({1: "1s"})
    set_sample_interval_overrides({60: "10s"})
    # 1m 은 디폴트로 돌아감
    assert sample_interval_for_window(1) == DEFAULT_SAMPLE_INTERVALS[1]
    assert sample_interval_for_window(60) == "10s"


def test_parse_overrides_basic() -> None:
    assert parse_sample_interval_overrides("1:100T,5:200T,60:1s") == {
        1: "100T", 5: "200T", 60: "1s",
    }


def test_parse_overrides_whitespace() -> None:
    assert parse_sample_interval_overrides(" 1 : 100T , 60 : 1s ") == {1: "100T", 60: "1s"}


def test_parse_overrides_empty() -> None:
    assert parse_sample_interval_overrides("") == {}
    assert parse_sample_interval_overrides("  ") == {}


def test_parse_overrides_malformed_raises() -> None:
    with pytest.raises(ValueError):
        parse_sample_interval_overrides("60")  # no colon


def test_query_uses_overrides_in_gap() -> None:
    """end-to-end: setter → build_query SQL 의 SAMPLE BY 가 바뀜."""
    from market_state_updater.jobs import gap

    set_sample_interval_overrides({10: "30s"})
    sql = gap.build_query("gate", "binance", 10)
    assert "SAMPLE BY 30s" in sql

    set_sample_interval_overrides({})
    sql = gap.build_query("gate", "binance", 10)
    assert "SAMPLE BY 200T" in sql  # 디폴트


def test_query_uses_overrides_in_other_jobs() -> None:
    """spread_pair / gate_web_gap 도 같은 함수 쓰니 자동 반영."""
    from market_state_updater.jobs import gate_web_gap, spread_pair

    set_sample_interval_overrides({30: "2s"})
    assert "SAMPLE BY 2s" in spread_pair.build_query(30)
    assert "SAMPLE BY 2s" in gate_web_gap.build_query(30)

from __future__ import annotations

import pytest

from market_state_updater.jobs.common import (
    DEFAULT_CADENCE_SECS,
    cadence_for_window,
    parse_cadence_overrides,
)


def test_default_cadence_known_windows() -> None:
    for w, expected in DEFAULT_CADENCE_SECS.items():
        assert cadence_for_window(w) == expected


def test_cadence_overrides_take_precedence() -> None:
    assert cadence_for_window(60, {60: 30}) == 30
    # 다른 window 는 디폴트 유지
    assert cadence_for_window(5, {60: 30}) == DEFAULT_CADENCE_SECS[5]


def test_cadence_unknown_window_fallback() -> None:
    """매핑 없는 window 는 max(1, window_min*60/12) — 12분의 1 비율 fallback."""
    assert cadence_for_window(120) == 120 * 60 / 12   # 600s
    assert cadence_for_window(0) == 1.0  # min 1s


def test_parse_overrides_basic() -> None:
    assert parse_cadence_overrides("1:5,5:25,60:120") == {1: 5.0, 5: 25.0, 60: 120.0}


def test_parse_overrides_float() -> None:
    assert parse_cadence_overrides("1:0.5,5:2.5") == {1: 0.5, 5: 2.5}


def test_parse_overrides_empty() -> None:
    assert parse_cadence_overrides("") == {}
    assert parse_cadence_overrides("  ") == {}


def test_parse_overrides_malformed_raises() -> None:
    with pytest.raises(ValueError):
        parse_cadence_overrides("60")  # no colon

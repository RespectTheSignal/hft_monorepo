from __future__ import annotations

import pytest

from market_state_updater.jobs.common import (
    FAST_PRICE_CHANGE_WINDOWS,
    FAST_WINDOWS,
    PRICE_CHANGE_WINDOWS,
    SLOW_PRICE_CHANGE_WINDOWS,
    SLOW_WINDOWS,
    WINDOW_MINUTES,
    windows_for_mode,
)


def test_fast_slow_partition_covers_all() -> None:
    """fast + slow 가 전체 윈도우와 정확히 일치해야."""
    assert tuple(sorted(FAST_WINDOWS + SLOW_WINDOWS)) == tuple(sorted(WINDOW_MINUTES))
    assert tuple(sorted(FAST_PRICE_CHANGE_WINDOWS + SLOW_PRICE_CHANGE_WINDOWS)) == tuple(
        sorted(PRICE_CHANGE_WINDOWS)
    )


def test_fast_slow_disjoint() -> None:
    assert set(FAST_WINDOWS) & set(SLOW_WINDOWS) == set()
    assert set(FAST_PRICE_CHANGE_WINDOWS) & set(SLOW_PRICE_CHANGE_WINDOWS) == set()


def test_windows_for_mode_fast() -> None:
    g, p = windows_for_mode("fast")
    assert g == FAST_WINDOWS
    assert p == FAST_PRICE_CHANGE_WINDOWS


def test_windows_for_mode_slow() -> None:
    g, p = windows_for_mode("slow")
    assert g == SLOW_WINDOWS
    assert p == SLOW_PRICE_CHANGE_WINDOWS


def test_windows_for_mode_all() -> None:
    g, p = windows_for_mode("all")
    assert g == WINDOW_MINUTES
    assert p == PRICE_CHANGE_WINDOWS


def test_windows_for_mode_invalid() -> None:
    with pytest.raises(ValueError):
        windows_for_mode("nope")

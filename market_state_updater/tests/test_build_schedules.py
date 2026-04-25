"""build_schedules 가 mode 에 따라 schedule 갯수를 정확히 조정하는지."""

from __future__ import annotations

from unittest.mock import MagicMock

from market_state_updater.config import AppConfig
from market_state_updater.jobs.common import (
    FAST_PRICE_CHANGE_WINDOWS,
    FAST_WINDOWS,
    PRICE_CHANGE_WINDOWS,
    SLOW_PRICE_CHANGE_WINDOWS,
    SLOW_WINDOWS,
    WINDOW_MINUTES,
)
from market_state_updater.main import build_schedules


def _cfg(window_mode: str, *, include_corr: bool = True) -> AppConfig:
    return AppConfig(
        questdb_url="http://q",
        redis_url="redis://r",
        interval_secs=10,
        once=False,
        window_mode=window_mode,  # type: ignore[arg-type]
        market_gap_prefix="p",
        base_exchange="gate",
        quote_exchanges=("binance",),
        sample_interval_overrides={},
        include_gate_web=True,
        include_spread_pair=True,
        include_gate_gate_web_gap=True,
        include_price_change=True,
        price_change_prefix="pc",
        price_change_sources=("gate", "binance"),
        include_price_change_gap_corr=include_corr,
        corr_quote_exchanges=("binance",),
        corr_return_seconds_overrides={},
        include_mid_corr=False,
        mid_corr_prefix="mc",
        mid_corr_quote_exchanges=("binance",),
        mid_corr_min_samples=30,
        heartbeat_prefix="hb",
        telegram_bot_token=None,
        telegram_chat_id=None,
        alert_after_consecutive_failures=5,
    )


def _expected_count(
    n_gap_windows: int,
    n_pc_windows: int,
    *,
    bases: int = 2,
    quotes: int = 1,
    sources: int = 2,
    corr_quotes: int = 1,
) -> int:
    # gap + spread_pair + gate_web_gap + price_change + corr (corr_quotes × gap_windows)
    return (
        bases * quotes * n_gap_windows
        + n_gap_windows
        + n_gap_windows
        + sources * n_pc_windows
        + corr_quotes * n_gap_windows
    )


def test_build_schedules_fast() -> None:
    schedules = build_schedules(_cfg("fast"), MagicMock())
    expected = _expected_count(len(FAST_WINDOWS), len(FAST_PRICE_CHANGE_WINDOWS))
    assert len(schedules) == expected


def test_build_schedules_slow() -> None:
    schedules = build_schedules(_cfg("slow"), MagicMock())
    expected = _expected_count(len(SLOW_WINDOWS), len(SLOW_PRICE_CHANGE_WINDOWS))
    assert len(schedules) == expected


def test_build_schedules_all() -> None:
    schedules = build_schedules(_cfg("all"), MagicMock())
    expected = _expected_count(len(WINDOW_MINUTES), len(PRICE_CHANGE_WINDOWS))
    assert len(schedules) == expected


def test_build_schedules_names_unique() -> None:
    schedules = build_schedules(_cfg("all"), MagicMock())
    names = [s.name for s in schedules]
    assert len(names) == len(set(names))


def test_fast_includes_only_fast_windows_in_names() -> None:
    schedules = build_schedules(_cfg("fast"), MagicMock())
    for s in schedules:
        if (
            s.name.startswith("gap:")
            or s.name.startswith("spread_pair:")
            or s.name.startswith("gate_web_gap:")
        ):
            window_str = s.name.rsplit(":", 1)[1].rstrip("m")
            assert int(window_str) in FAST_WINDOWS
        elif s.name.startswith("price_change:"):
            window_str = s.name.rsplit(":", 1)[1].rstrip("m")
            assert int(window_str) in FAST_PRICE_CHANGE_WINDOWS
        elif s.name.startswith("corr:"):
            # name: corr:gate_web:binance:{w}m:{ret}s
            window_str = s.name.split(":")[3].rstrip("m")
            assert int(window_str) in FAST_WINDOWS


def test_corr_disabled_drops_schedules() -> None:
    with_corr = build_schedules(_cfg("all", include_corr=True), MagicMock())
    no_corr = build_schedules(_cfg("all", include_corr=False), MagicMock())
    diff = len(with_corr) - len(no_corr)
    # 1 quote × len(WINDOW_MINUTES) gap windows
    from market_state_updater.jobs.common import WINDOW_MINUTES as _W

    assert diff == len(_W)

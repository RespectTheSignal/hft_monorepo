"""build_schedules 가 mode 에 따라 schedule 갯수를 정확히 조정하는지."""

from __future__ import annotations

from dataclasses import replace
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
        cadence_overrides={},
        stagger_step_secs=0.0,
        tick_max_workers=4,
        market_gap_prefix="p",
        base_exchange="gate",
        quote_exchanges=("binance",),
        sample_interval_overrides={},
        include_gate_web=True,
        include_spread_pair=True,
        include_gate_gate_web_gap=True,
        include_flipster_gap=False,
        include_trade_outcomes=False,
        trade_outcomes_lookahead_seconds=(30, 60),
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
        include_return_autocorr=False,
        return_autocorr_prefix="ra",
        return_autocorr_exchanges=("gate", "gate_web", "binance"),
        return_autocorr_min_samples=30,
        include_variance_ratio=False,
        variance_ratio_prefix="vr",
        variance_ratio_exchanges=("gate", "gate_web", "binance"),
        variance_ratio_k_values=(2, 5, 10),
        variance_ratio_min_samples=30,
        variance_ratio_base_seconds_overrides={},
        include_market_dangerous=False,
        market_dangerous_redis_key="gate_hft:market_dangerous",
        market_dangerous_primary_table="gate_bookticker",
        market_dangerous_compare_table="binance_bookticker",
        market_dangerous_absolute_threshold=150000,
        market_dangerous_window_secs=60,
        market_dangerous_sticky_secs=600,
        market_dangerous_cadence_secs=5.0,
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


def test_build_schedules_cadence_matches_window() -> None:
    """1m schedule 은 cadence 5s, 60m 는 300s — DEFAULT_CADENCE_SECS 따라감."""
    schedules = build_schedules(_cfg("all"), MagicMock())
    by_name = {s.name: s.cadence_secs for s in schedules}
    # gap:gate:binance:1m / gap:gate:binance:60m 둘 다 있어야
    assert by_name.get("gap:gate:binance:1m") == 5
    assert by_name.get("gap:gate:binance:60m") == 300


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


def test_flipster_gap_adds_one_per_window() -> None:
    """include_flipster_gap=True 면 gap_windows 만큼 schedule 추가."""
    base = _cfg("all")
    with_fp = replace(base, include_flipster_gap=True)
    diff = len(build_schedules(with_fp, MagicMock())) - len(build_schedules(base, MagicMock()))
    assert diff == len(WINDOW_MINUTES)


def test_flipster_gap_schedule_name_format() -> None:
    cfg_fp = replace(_cfg("fast"), include_flipster_gap=True)
    schedules = build_schedules(cfg_fp, MagicMock())
    fp_names = [s.name for s in schedules if s.name.startswith("flipster_gap:")]
    assert len(fp_names) == len(FAST_WINDOWS)
    for n in fp_names:
        # name: flipster_gap:flipster:binance:{w}m
        parts = n.split(":")
        assert parts[0] == "flipster_gap"
        assert parts[1] == "flipster"
        assert parts[2] == "binance"
        assert int(parts[3].rstrip("m")) in FAST_WINDOWS


def test_trade_outcomes_adds_window_x_lookahead_schedules() -> None:
    """include_trade_outcomes=True 면 gap_windows × len(lookahead_seconds) 만큼 추가."""
    base = _cfg("all")
    with_to = replace(
        base, include_trade_outcomes=True, trade_outcomes_lookahead_seconds=(30, 60)
    )
    diff = len(build_schedules(with_to, MagicMock())) - len(
        build_schedules(base, MagicMock())
    )
    assert diff == len(WINDOW_MINUTES) * 2


def test_trade_outcomes_schedule_name_format() -> None:
    cfg_to = replace(
        _cfg("fast"),
        include_trade_outcomes=True,
        trade_outcomes_lookahead_seconds=(30, 60),
    )
    schedules = build_schedules(cfg_to, MagicMock())
    to_names = [s.name for s in schedules if s.name.startswith("trade_outcomes:")]
    # fast = (1, 5) windows × 2 lookaheads
    assert len(to_names) == len(FAST_WINDOWS) * 2
    expected = {
        f"trade_outcomes:gate:{w}m:{la}s"
        for w in FAST_WINDOWS
        for la in (30, 60)
    }
    assert set(to_names) == expected


def test_corr_disabled_drops_schedules() -> None:
    with_corr = build_schedules(_cfg("all", include_corr=True), MagicMock())
    no_corr = build_schedules(_cfg("all", include_corr=False), MagicMock())
    diff = len(with_corr) - len(no_corr)
    # 1 quote × len(WINDOW_MINUTES) gap windows
    from market_state_updater.jobs.common import WINDOW_MINUTES as _W

    assert diff == len(_W)

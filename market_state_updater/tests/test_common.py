from __future__ import annotations

from market_state_updater.jobs.common import (
    MAX_ABS_GAP,
    bookticker_table,
    bookticker_table_for_gap_base,
    filter_valid_gaps,
    sample_interval_for_window,
)


def test_bookticker_table_default() -> None:
    assert bookticker_table("gate") == "gate_bookticker"
    assert bookticker_table("BINANCE") == "binance_bookticker"


def test_bookticker_table_for_gap_base_gate_web() -> None:
    assert bookticker_table_for_gap_base("gate_web") == "gate_webbookticker"
    assert bookticker_table_for_gap_base("GATE_WEB") == "gate_webbookticker"
    assert bookticker_table_for_gap_base("gate") == "gate_bookticker"
    assert bookticker_table_for_gap_base("bybit") == "bybit_bookticker"


def test_sample_interval_buckets() -> None:
    # 1m -> 100ms (정밀도)
    assert sample_interval_for_window(1) == "100T"
    # 5~120m -> 1s
    assert sample_interval_for_window(5) == "1s"
    assert sample_interval_for_window(60) == "1s"
    assert sample_interval_for_window(120) == "1s"
    # >120m -> 5s (cost↓)
    assert sample_interval_for_window(240) == "5s"
    assert sample_interval_for_window(720) == "5s"


def test_filter_valid_gaps_drops_outliers() -> None:
    gaps = {
        "BTC_USDT": 0.001,
        "BAD_USDT": 0.5,  # > MAX_ABS_GAP
        "NEG_USDT": -0.05,
        "BAD2": -0.99,
    }
    out = filter_valid_gaps(gaps)
    assert "BTC_USDT" in out
    assert "NEG_USDT" in out
    assert "BAD_USDT" not in out
    assert "BAD2" not in out
    assert MAX_ABS_GAP == 0.1

from __future__ import annotations

import pytest

from market_state_updater.jobs import price_change_gap_corr
from market_state_updater.jobs.common import (
    DEFAULT_CORR_RETURN_SECONDS,
    corr_return_seconds,
    parse_corr_return_seconds_overrides,
)


# ---- common helpers ----


def test_corr_return_seconds_defaults() -> None:
    for w, expected in DEFAULT_CORR_RETURN_SECONDS.items():
        assert corr_return_seconds(w) == expected


def test_corr_return_seconds_override_single() -> None:
    assert corr_return_seconds(60, {60: 5}) == 5


def test_corr_return_seconds_override_keeps_others() -> None:
    out = corr_return_seconds(10, {60: 5})
    assert out == DEFAULT_CORR_RETURN_SECONDS[10]


def test_corr_return_seconds_unknown_window_fallback() -> None:
    assert corr_return_seconds(123) == 123


def test_parse_overrides_basic() -> None:
    assert parse_corr_return_seconds_overrides("1:1,5:5,60:30") == {1: 1, 5: 5, 60: 30}


def test_parse_overrides_empty() -> None:
    assert parse_corr_return_seconds_overrides("") == {}
    assert parse_corr_return_seconds_overrides("  ") == {}


def test_parse_overrides_whitespace_tolerant() -> None:
    assert parse_corr_return_seconds_overrides(" 60 : 30 , 240:60 ") == {60: 30, 240: 60}


def test_parse_overrides_malformed_raises() -> None:
    with pytest.raises(ValueError):
        parse_corr_return_seconds_overrides("60")  # no colon
    with pytest.raises(ValueError):
        parse_corr_return_seconds_overrides("60:abc")


# ---- query builder ----


def test_build_query_uses_gate_web_and_quote() -> None:
    sql = price_change_gap_corr.build_query("binance", 10, 5)
    assert "gate_webbookticker" in sql
    assert "binance_bookticker" in sql
    assert "SAMPLE BY 5s FILL(PREV)" in sql
    assert "stddev_pop" in sql
    assert "x_return" in sql
    assert "y_gap" in sql


def test_build_query_lag_uses_dateadd() -> None:
    """gw_prev 가 dateadd('s', return_seconds, timestamp) 로 self-shift."""
    sql = price_change_gap_corr.build_query("binance", 10, 30)
    assert "dateadd('s', 30, timestamp)" in sql


def test_build_query_min_samples_filter() -> None:
    sql = price_change_gap_corr.build_query("binance", 10, 5)
    assert f"n >= {price_change_gap_corr.MIN_SAMPLES}" in sql
    # stddev 임계로 numerical 불안정 심볼 drop
    assert f"sx > {price_change_gap_corr.MIN_STDDEV}" in sql
    assert f"sy > {price_change_gap_corr.MIN_STDDEV}" in sql


def test_build_query_supports_other_quote() -> None:
    """quote 가 일단 binance only 지만 코드는 generic — bybit 도 SQL 짤 수 있어야."""
    sql = price_change_gap_corr.build_query("bybit", 30, 10)
    assert "bybit_bookticker" in sql


# ---- parser ----


def test_parse_dataset_full() -> None:
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "corr"},
            {"name": "n"},
        ],
        "dataset": [
            ["BTC_USDT", 0.42, 120],
            ["ETH_USDT", -0.17, 120],
            ["SOL_USDT", None, 100],   # corr None 은 drop
            ["XRP_USDT", 0.5, None],   # n None 도 drop
            ["", 0.9, 30],              # 빈 symbol drop
        ],
    }
    corrs, counts = price_change_gap_corr.parse_dataset(payload)
    assert corrs == {"BTC_USDT": 0.42, "ETH_USDT": -0.17}
    assert counts == {"BTC_USDT": 120, "ETH_USDT": 120}


def test_parse_dataset_missing_columns_raises() -> None:
    payload = {"columns": [{"name": "symbol"}, {"name": "corr"}], "dataset": []}
    with pytest.raises(ValueError):
        price_change_gap_corr.parse_dataset(payload)


def test_parse_dataset_drops_corr_outside_range() -> None:
    """QuestDB stddev_pop 부동소수점 한계로 |corr| > 1.001 나오는 artifact 제거."""
    payload = {
        "columns": [{"name": "symbol"}, {"name": "corr"}, {"name": "n"}],
        "dataset": [
            ["GOOD", 0.42, 100],
            ["BAD", 3.23, 60],         # numerical artifact
            ["NEG_BAD", -1.5, 60],     # 음수 쪽 artifact
            ["EDGE", 1.0001, 60],      # 임계 안쪽 1.001 — keep
            ["FAR", 1.5, 60],          # drop
        ],
    }
    corrs, _ = price_change_gap_corr.parse_dataset(payload)
    assert "GOOD" in corrs
    assert "EDGE" in corrs
    assert "BAD" not in corrs
    assert "NEG_BAD" not in corrs
    assert "FAR" not in corrs


def test_parse_dataset_drops_nan() -> None:
    payload = {
        "columns": [{"name": "symbol"}, {"name": "corr"}, {"name": "n"}],
        "dataset": [["GOOD", 0.5, 100], ["NAN", float("nan"), 100]],
    }
    corrs, _ = price_change_gap_corr.parse_dataset(payload)
    assert corrs == {"GOOD": 0.5}


def test_build_query_uses_stddev_threshold() -> None:
    sql = price_change_gap_corr.build_query("binance", 10, 5)
    assert f"sx > {price_change_gap_corr.MIN_STDDEV}" in sql
    assert f"sy > {price_change_gap_corr.MIN_STDDEV}" in sql

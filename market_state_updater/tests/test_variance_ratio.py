from __future__ import annotations

import pytest

from market_state_updater.jobs import variance_ratio


# ---- query builder ----


def test_build_query_uses_correct_table() -> None:
    assert "gate_bookticker" in variance_ratio.build_query("gate", 5, 5, 2)
    assert "gate_webbookticker" in variance_ratio.build_query("gate_web", 5, 5, 2)
    assert "binance_bookticker" in variance_ratio.build_query("binance", 5, 5, 2)


def test_build_query_two_sample_intervals() -> None:
    """SAMPLE BY base_seconds 와 SAMPLE BY (k * base_seconds) 두 CTE."""
    sql = variance_ratio.build_query("gate", 30, 10, 5)  # base=10s, k=5 → 50s
    assert "SAMPLE BY 10s FILL(PREV)" in sql
    assert "SAMPLE BY 50s FILL(PREV)" in sql
    # vr ratio 식 확인
    assert "/ (5 * v1.sd * v1.sd)" in sql


def test_build_query_dateadd_for_both_levels() -> None:
    sql = variance_ratio.build_query("gate", 30, 10, 5)
    assert "dateadd('s', 10, timestamp)" in sql   # base shift
    assert "dateadd('s', 50, timestamp)" in sql   # k step shift


def test_build_query_min_samples_filter() -> None:
    sql = variance_ratio.build_query("gate", 5, 5, 2, min_samples=50)
    assert "n >= 50" in sql


# ---- parser ----


def test_parse_dataset_full() -> None:
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "vr"},
            {"name": "n"},
        ],
        "dataset": [
            ["BTC_USDT", 0.95, 60],
            ["ETH_USDT", 1.20, 60],
            ["NULL_SYM", None, 60],
            ["", 1.0, 30],
        ],
    }
    vrs, counts = variance_ratio.parse_dataset(payload)
    assert vrs == {"BTC_USDT": 0.95, "ETH_USDT": 1.20}
    assert counts == {"BTC_USDT": 60, "ETH_USDT": 60}


def test_parse_dataset_drops_nan() -> None:
    payload = {
        "columns": [{"name": "symbol"}, {"name": "vr"}, {"name": "n"}],
        "dataset": [["GOOD", 0.9, 50], ["NAN", float("nan"), 50]],
    }
    vrs, _ = variance_ratio.parse_dataset(payload)
    assert vrs == {"GOOD": 0.9}


def test_parse_dataset_missing_columns_raises() -> None:
    payload = {"columns": [{"name": "symbol"}, {"name": "vr"}], "dataset": []}
    with pytest.raises(ValueError):
        variance_ratio.parse_dataset(payload)

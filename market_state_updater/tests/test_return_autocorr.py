from __future__ import annotations

import pytest

from market_state_updater.jobs import return_autocorr


# ---- query builder ----


def test_build_query_uses_correct_table() -> None:
    assert "gate_bookticker" in return_autocorr.build_query("gate", 5, 5)
    assert "gate_webbookticker" in return_autocorr.build_query("gate_web", 5, 5)
    assert "binance_bookticker" in return_autocorr.build_query("binance", 5, 5)


def test_build_query_uses_two_lag_self_shifts() -> None:
    """returns t, t-step, t-2*step 세 시점 align — dateadd 두 번."""
    sql = return_autocorr.build_query("gate", 5, 5)
    assert "dateadd('s', 5, timestamp)" in sql
    assert "dateadd('s', 10, timestamp)" in sql  # 2 × 5


def test_build_query_uses_corr_aggregate() -> None:
    sql = return_autocorr.build_query("gate", 5, 5)
    assert "corr(r_t, r_prev)" in sql
    assert "FILL(PREV)" in sql


def test_build_query_min_samples_filter() -> None:
    sql = return_autocorr.build_query("gate", 5, 5, min_samples=50)
    assert "n_samples >= 50" in sql
    assert f"nz_t >= {return_autocorr.MIN_NONZERO_SAMPLES}" in sql
    assert f"nz_p >= {return_autocorr.MIN_NONZERO_SAMPLES}" in sql


# ---- parser ----


def test_parse_dataset_full() -> None:
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "autocorr"},
            {"name": "n_samples"},
        ],
        "dataset": [
            ["BTC_USDT", -0.15, 58],
            ["ETH_USDT", 0.02, 60],
            ["NULL_SYM", None, 60],
            ["", 0.5, 30],
        ],
    }
    autos, counts = return_autocorr.parse_dataset(payload)
    assert autos == {"BTC_USDT": -0.15, "ETH_USDT": 0.02}
    assert counts == {"BTC_USDT": 58, "ETH_USDT": 60}


def test_parse_dataset_drops_nan() -> None:
    payload = {
        "columns": [{"name": "symbol"}, {"name": "autocorr"}, {"name": "n_samples"}],
        "dataset": [["GOOD", 0.1, 60], ["NAN", float("nan"), 60]],
    }
    autos, _ = return_autocorr.parse_dataset(payload)
    assert autos == {"GOOD": 0.1}


def test_parse_dataset_missing_columns_raises() -> None:
    payload = {
        "columns": [{"name": "symbol"}, {"name": "autocorr"}],
        "dataset": [],
    }
    with pytest.raises(ValueError):
        return_autocorr.parse_dataset(payload)

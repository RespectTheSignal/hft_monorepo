from __future__ import annotations

import pytest

from market_state_updater.jobs import mid_corr


# ---- query builder ----


def test_build_query_gate_base() -> None:
    sql = mid_corr.build_query("gate", "binance", 10)
    assert "gate_bookticker" in sql
    assert "binance_bookticker" in sql
    assert "corr(base_mid, quote_mid)" in sql
    assert "INNER JOIN" in sql
    # QuestDB HAVING 미지원 → outer SELECT 에서 WHERE
    assert "HAVING" not in sql
    assert "WHERE n_samples >= 30" in sql  # default MIN_SAMPLES


def test_build_query_gate_web_base() -> None:
    sql = mid_corr.build_query("gate_web", "binance", 60)
    assert "gate_webbookticker" in sql
    assert "binance_bookticker" in sql


def test_build_query_min_samples_override() -> None:
    sql = mid_corr.build_query("gate", "binance", 30, min_samples=50)
    assert "WHERE n_samples >= 50" in sql


def test_build_query_uses_window_sample_interval() -> None:
    """SAMPLE BY 단위는 jobs.common 의 디폴트 + override 따라감 (60m → 1s)."""
    sql = mid_corr.build_query("gate", "binance", 60)
    assert "SAMPLE BY 1s" in sql


# ---- parser ----


def test_parse_dataset_full() -> None:
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "corr_mid"},
            {"name": "n_samples"},
        ],
        "dataset": [
            ["BTC_USDT", 0.998, 580],
            ["ETH_USDT", 0.992, 575],
            ["NULL_SYM", None, 100],   # corr None drop
            ["", 0.5, 30],              # 빈 symbol drop
        ],
    }
    corrs, counts = mid_corr.parse_dataset(payload)
    assert corrs == {"BTC_USDT": 0.998, "ETH_USDT": 0.992}
    assert counts == {"BTC_USDT": 580, "ETH_USDT": 575}


def test_parse_dataset_drops_nan() -> None:
    payload = {
        "columns": [{"name": "symbol"}, {"name": "corr_mid"}, {"name": "n_samples"}],
        "dataset": [["BTC_USDT", 0.99, 500], ["NAN_SYM", float("nan"), 100]],
    }
    corrs, _ = mid_corr.parse_dataset(payload)
    assert corrs == {"BTC_USDT": 0.99}


def test_parse_dataset_missing_columns_raises() -> None:
    payload = {"columns": [{"name": "symbol"}, {"name": "corr_mid"}], "dataset": []}
    with pytest.raises(ValueError):
        mid_corr.parse_dataset(payload)

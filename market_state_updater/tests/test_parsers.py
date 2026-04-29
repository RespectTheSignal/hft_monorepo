"""QuestDB /exec JSON payload → dict 파서 테스트.

QuestDB 가 응답하는 모양:
  {
    "columns": [{"name": "symbol", "type": "SYMBOL"}, ...],
    "dataset": [["BTC_USDT", 0.001, 0.0002], ...],
    "count": N
  }
"""

from __future__ import annotations

import pytest

from market_state_updater.jobs import flipster_gap, gap, gate_web_gap, spread_pair


# ---- gap.parse_dataset ----


def test_gap_parse_full_payload() -> None:
    payload = {
        "columns": [
            {"name": "symbol", "type": "SYMBOL"},
            {"name": "avg_mid_gap", "type": "DOUBLE"},
            {"name": "avg_spread", "type": "DOUBLE"},
        ],
        "dataset": [
            ["BTC_USDT", 0.0012, 0.00015],
            ["ETH_USDT", -0.0008, 0.00025],
            ["SOL_USDT", None, 0.0003],  # gap None 은 0.0 으로
        ],
    }
    gaps, spreads = gap.parse_dataset(payload)
    assert gaps == {"BTC_USDT": 0.0012, "ETH_USDT": -0.0008, "SOL_USDT": 0.0}
    assert spreads == {"BTC_USDT": 0.00015, "ETH_USDT": 0.00025, "SOL_USDT": 0.0003}


def test_gap_parse_empty_dataset() -> None:
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "avg_mid_gap"},
            {"name": "avg_spread"},
        ],
        "dataset": [],
    }
    gaps, spreads = gap.parse_dataset(payload)
    assert gaps == {}
    assert spreads == {}


def test_gap_parse_skips_blank_symbol() -> None:
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "avg_mid_gap"},
            {"name": "avg_spread"},
        ],
        "dataset": [["", 0.001, 0.0001], ["BTC_USDT", 0.002, 0.0002]],
    }
    gaps, _ = gap.parse_dataset(payload)
    assert list(gaps.keys()) == ["BTC_USDT"]


def test_gap_parse_missing_avg_mid_gap_raises() -> None:
    payload = {
        "columns": [{"name": "symbol"}, {"name": "avg_spread"}],
        "dataset": [["BTC_USDT", 0.0001]],
    }
    with pytest.raises(ValueError):
        gap.parse_dataset(payload)


# ---- spread_pair.parse_dataset ----


def test_spread_pair_parse_full_payload() -> None:
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "avg_spread_gate_bookticker"},
            {"name": "avg_spread_gate_webbookticker"},
        ],
        "dataset": [
            ["BTC_USDT", 0.0001, 0.00012],
            ["ETH_USDT", 0.00015, None],  # None → 0.0
        ],
    }
    g, w = spread_pair.parse_dataset(payload)
    assert g == {"BTC_USDT": 0.0001, "ETH_USDT": 0.00015}
    assert w == {"BTC_USDT": 0.00012, "ETH_USDT": 0.0}


def test_spread_pair_parse_missing_columns_raises() -> None:
    payload = {
        "columns": [{"name": "symbol"}, {"name": "avg_spread_gate_bookticker"}],
        "dataset": [],
    }
    with pytest.raises(ValueError):
        spread_pair.parse_dataset(payload)


# ---- gate_web_gap.parse_dataset ----


def test_gate_web_gap_parse_full_payload() -> None:
    payload = {
        "columns": [{"name": "symbol"}, {"name": "avg_mid_gap"}],
        "dataset": [
            ["BTC_USDT", 0.0001],
            ["ETH_USDT", -0.0002],
            ["SOL_USDT", None],  # None 은 skip
        ],
    }
    out = gate_web_gap.parse_dataset(payload)
    assert out == {"BTC_USDT": 0.0001, "ETH_USDT": -0.0002}


def test_gate_web_gap_parse_no_avg_mid_gap_returns_empty() -> None:
    """gate_web_gap 은 컬럼 없으면 ValueError 가 아니라 빈 dict (원본 동작 유지)."""
    payload = {"columns": [{"name": "symbol"}], "dataset": [["BTC_USDT"]]}
    assert gate_web_gap.parse_dataset(payload) == {}


# ---- flipster_gap.parse_dataset ----


def test_flipster_gap_parse_full_payload() -> None:
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "avg_mid_gap"},
            {"name": "avg_spread"},
            {"name": "avg_spread_flipster"},
        ],
        "dataset": [
            ["BTC_USDT", 0.0012, 0.00015, 0.00010],
            ["ETH_USDT", -0.0008, 0.00025, None],  # None → 0.0
            ["SOL_USDT", None, 0.0003, 0.0002],    # gap None → 0.0
        ],
    }
    gaps, b_sp, f_sp = flipster_gap.parse_dataset(payload)
    assert gaps == {"BTC_USDT": 0.0012, "ETH_USDT": -0.0008, "SOL_USDT": 0.0}
    assert b_sp == {"BTC_USDT": 0.00015, "ETH_USDT": 0.00025, "SOL_USDT": 0.0003}
    assert f_sp == {"BTC_USDT": 0.00010, "ETH_USDT": 0.0, "SOL_USDT": 0.0002}


def test_flipster_gap_parse_missing_avg_mid_gap_raises() -> None:
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "avg_spread"},
            {"name": "avg_spread_flipster"},
        ],
        "dataset": [["BTC_USDT", 0.0001, 0.0001]],
    }
    with pytest.raises(ValueError):
        flipster_gap.parse_dataset(payload)


def test_flipster_gap_parse_empty_dataset() -> None:
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "avg_mid_gap"},
            {"name": "avg_spread"},
            {"name": "avg_spread_flipster"},
        ],
        "dataset": [],
    }
    gaps, b_sp, f_sp = flipster_gap.parse_dataset(payload)
    assert gaps == {}
    assert b_sp == {}
    assert f_sp == {}

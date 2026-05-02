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

from market_state_updater.jobs import (
    flipster_gap,
    gap,
    gate_web_gap,
    spread_pair,
    trade_outcomes,
)


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
    """parse 반환 순서: (gaps, flipster_spreads, binance_spreads). base=flipster."""
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "avg_mid_gap"},
            {"name": "avg_spread"},          # base = flipster
            {"name": "avg_spread_binance"},  # quote = binance
        ],
        "dataset": [
            ["BTC_USDT", 0.0012, 0.00010, 0.00015],
            ["ETH_USDT", -0.0008, None, 0.00025],  # None → 0.0
            ["SOL_USDT", None, 0.0002, 0.0003],    # gap None → 0.0
        ],
    }
    gaps, f_sp, b_sp = flipster_gap.parse_dataset(payload)
    assert gaps == {"BTC_USDT": 0.0012, "ETH_USDT": -0.0008, "SOL_USDT": 0.0}
    assert f_sp == {"BTC_USDT": 0.00010, "ETH_USDT": 0.0, "SOL_USDT": 0.0002}
    assert b_sp == {"BTC_USDT": 0.00015, "ETH_USDT": 0.00025, "SOL_USDT": 0.0003}


def test_flipster_gap_parse_missing_avg_mid_gap_raises() -> None:
    payload = {
        "columns": [
            {"name": "symbol"},
            {"name": "avg_spread"},
            {"name": "avg_spread_binance"},
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
            {"name": "avg_spread_binance"},
        ],
        "dataset": [],
    }
    gaps, f_sp, b_sp = flipster_gap.parse_dataset(payload)
    assert gaps == {}
    assert f_sp == {}
    assert b_sp == {}


# ---- trade_outcomes.parse_dataset ----


def _trade_outcomes_columns() -> list[dict[str, str]]:
    return [
        {"name": "symbol"},
        {"name": "long_win_count"},
        {"name": "long_loss_count"},
        {"name": "long_win_volume"},
        {"name": "long_loss_volume"},
        {"name": "short_win_count"},
        {"name": "short_loss_count"},
        {"name": "short_win_volume"},
        {"name": "short_loss_volume"},
    ]


def test_trade_outcomes_parse_full_payload() -> None:
    payload = {
        "columns": _trade_outcomes_columns(),
        "dataset": [
            ["BTC_USDT", 3, 2, 412.3, 280.1, 1, 4, 80.0, 320.5],
            ["ETH_USDT", 0, 0, 0.0, 0.0, 5, 1, 1500.0, 200.0],
        ],
    }
    out = trade_outcomes.parse_dataset(payload)
    assert set(out.keys()) == {"BTC_USDT", "ETH_USDT"}
    btc = out["BTC_USDT"]
    # count 류는 int
    assert btc["long_win_count"] == 3 and isinstance(btc["long_win_count"], int)
    assert btc["short_loss_count"] == 4 and isinstance(btc["short_loss_count"], int)
    # volume 류는 float
    assert btc["long_win_volume"] == 412.3
    assert btc["short_loss_volume"] == 320.5


def test_trade_outcomes_parse_skip_blank_symbol() -> None:
    payload = {
        "columns": _trade_outcomes_columns(),
        "dataset": [
            ["", 1, 0, 100.0, 0.0, 0, 0, 0.0, 0.0],
            ["BTC_USDT", 2, 1, 200.0, 50.0, 0, 0, 0.0, 0.0],
        ],
    }
    out = trade_outcomes.parse_dataset(payload)
    assert list(out.keys()) == ["BTC_USDT"]


def test_trade_outcomes_parse_none_treated_as_zero() -> None:
    payload = {
        "columns": _trade_outcomes_columns(),
        "dataset": [
            ["FOO_USDT", None, 1, None, 50.0, 0, 0, 0.0, 0.0],
        ],
    }
    out = trade_outcomes.parse_dataset(payload)
    foo = out["FOO_USDT"]
    assert foo["long_win_count"] == 0
    assert foo["long_win_volume"] == 0.0
    assert foo["long_loss_count"] == 1
    assert foo["long_loss_volume"] == 50.0


def test_trade_outcomes_parse_missing_column_raises() -> None:
    cols = _trade_outcomes_columns()
    cols.pop()  # short_loss_volume drop
    payload = {"columns": cols, "dataset": []}
    with pytest.raises(ValueError):
        trade_outcomes.parse_dataset(payload)


def test_trade_outcomes_parse_empty_dataset() -> None:
    payload = {"columns": _trade_outcomes_columns(), "dataset": []}
    assert trade_outcomes.parse_dataset(payload) == {}

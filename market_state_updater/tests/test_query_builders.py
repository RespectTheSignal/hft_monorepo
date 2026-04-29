"""Query builder snapshot-ish 테스트.

SQL string 그대로 비교하면 brittle 하니까 핵심 키워드/테이블/sample 만 검증.
"""

from __future__ import annotations

from market_state_updater.jobs import (
    flipster_gap,
    gap,
    gate_web_gap,
    price_change,
    spread_pair,
)


def test_gap_query_short_window_uses_fill_prev() -> None:
    sql = gap.build_query("gate", "binance", 1)
    assert "gate_bookticker" in sql
    assert "binance_bookticker" in sql
    assert "FILL(PREV)" in sql
    assert "SAMPLE BY 100T" in sql
    assert "avg_mid_gap" in sql
    assert "avg_spread" in sql


def test_gap_query_long_window_no_fill_prev() -> None:
    sql = gap.build_query("gate", "bybit", 60)
    assert "FILL(PREV)" not in sql
    # 60m 디폴트 SAMPLE BY = 1s
    assert "SAMPLE BY 1s" in sql
    assert "bybit_bookticker" in sql


def test_gap_query_gate_web_base_uses_webbookticker() -> None:
    sql = gap.build_query("gate_web", "binance", 10)
    assert "gate_webbookticker" in sql
    assert "binance_bookticker" in sql


def test_gap_query_very_long_window_uses_default_interval() -> None:
    # 720m 디폴트 = 10s
    sql = gap.build_query("gate", "binance", 720)
    assert "SAMPLE BY 10s" in sql


def test_spread_pair_query_has_both_sides() -> None:
    sql = spread_pair.build_query(10)
    assert "gate_bookticker" in sql
    assert "gate_webbookticker" in sql
    assert "avg_spread_gate_bookticker" in sql
    assert "avg_spread_gate_webbookticker" in sql


def test_spread_pair_query_1m_fill_prev() -> None:
    sql = spread_pair.build_query(1)
    assert "FILL(PREV)" in sql
    assert "SAMPLE BY 100T" in sql


def test_gate_web_gap_query_join_two_gate_tables() -> None:
    sql = gate_web_gap.build_query(5)
    assert "gate_bookticker" in sql
    assert "gate_webbookticker" in sql
    assert "FILL(PREV)" in sql  # 5m 이하는 fill prev
    assert "avg_mid_gap" in sql


def test_gate_web_gap_query_30m_no_fill() -> None:
    sql = gate_web_gap.build_query(30)
    assert "FILL(PREV)" not in sql
    # 30m 디폴트 = 500T (500ms)
    assert "SAMPLE BY 500T" in sql


def test_flipster_gap_query_short_window() -> None:
    sql = flipster_gap.build_query(1)
    assert "binance_bookticker" in sql
    assert "flipster_bookticker" in sql
    assert "FILL(PREV)" in sql
    # flipster 는 sample interval 최소 5s clamp — 100T/200T 가 5s 로 bump
    assert "SAMPLE BY 5s" in sql
    assert "SAMPLE BY 100T" not in sql
    # flipster 심볼 정규화 ('.PERP' 제거 + USDT → _USDT)
    assert "replace(replace(symbol, '.PERP', ''), 'USDT', '_USDT')" in sql
    assert "avg_mid_gap" in sql
    assert "avg_spread_flipster" in sql


def test_flipster_gap_query_long_window_no_fill() -> None:
    sql = flipster_gap.build_query(60)
    assert "FILL(PREV)" not in sql
    # 60m default = 1s → clamp → 5s
    assert "SAMPLE BY 5s" in sql
    assert "SAMPLE BY 1s" not in sql
    assert "replace(replace(symbol, '.PERP', ''), 'USDT', '_USDT')" in sql


def test_flipster_gap_query_720m_keeps_default() -> None:
    """default 가 이미 5s 이상이면 그대로 (720m=10s)."""
    sql = flipster_gap.build_query(720)
    assert "SAMPLE BY 10s" in sql


def test_price_change_query_basic() -> None:
    sql = price_change.build_query("gate_bookticker", 5)
    assert "gate_bookticker" in sql
    assert "first(" in sql
    assert "last(" in sql
    assert "n_ticks >= 2" in sql
    assert "price_change" in sql


def test_price_change_table_for_source() -> None:
    assert price_change.table_for_source("gate") == "gate_bookticker"
    assert price_change.table_for_source("binance") == "binance_bookticker"
    assert price_change.table_for_source("gate_web") == "gate_webbookticker"

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
    trade_outcomes,
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
    # base = flipster, quote = binance — 기준 spread 컬럼 + binance 추가 컬럼
    assert "avg_mid_gap" in sql
    assert "avg_spread_binance" in sql
    # JOIN 방향: flipster b JOIN binance q
    assert "FROM flipster b" in sql
    assert "JOIN binance q" in sql


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


# ---- trade_outcomes.build_query ----


def test_trade_outcomes_query_basic() -> None:
    sql = trade_outcomes.build_query(window_minutes=10, lookahead_seconds=30)
    # 두 source 다 들어가야
    assert "gate_sub_account_trades" in sql
    assert "gate_webbookticker" in sql
    # mid sample 1s + FILL(PREV)
    assert "SAMPLE BY 1s FILL(PREV)" in sql
    # lookahead shift via dateadd + date_trunc('second', ...)
    assert "dateadd('s', 30, t.timestamp)" in sql
    assert "date_trunc('second'" in sql
    # window filter
    assert "dateadd('m', -10, now())" in sql
    # tie-as-loss: long uses '<=', short uses '>='
    assert "m.mid <= t.price" in sql
    assert "m.mid >= t.price" in sql
    # 8 aggregate columns
    for col in (
        "long_win_count", "long_loss_count",
        "long_win_volume", "long_loss_volume",
        "short_win_count", "short_loss_count",
        "short_win_volume", "short_loss_volume",
    ):
        assert col in sql
    # dust filter + negative-fee skip
    assert "abs(t.usdt_size) > 10" in sql
    assert "t.fee >= 0" in sql


def test_trade_outcomes_query_lookahead_60s() -> None:
    sql = trade_outcomes.build_query(window_minutes=5, lookahead_seconds=60)
    assert "dateadd('s', 60, t.timestamp)" in sql
    assert "dateadd('s', -60, now())" in sql
    assert "dateadd('m', -5, now())" in sql


def test_trade_outcomes_query_volume_uses_abs_usdt_size() -> None:
    sql = trade_outcomes.build_query(window_minutes=10, lookahead_seconds=30)
    # volume 컬럼은 |usdt_size| 합
    assert "abs(t.usdt_size)" in sql

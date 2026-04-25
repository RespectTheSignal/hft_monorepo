from __future__ import annotations

from collections.abc import Iterator

import pytest

from market_state_updater.config import load_config

_KEYS = [
    "QUESTDB_URL",
    "REDIS_URL",
    "UPDATE_INTERVAL_SECS",
    "WINDOW_MODE",
    "MARKET_GAP_REDIS_PREFIX",
    "MARKET_GAP_BASE",
    "MARKET_GAP_QUOTE_EXCHANGES",
    "MARKET_GAP_INCLUDE_GATE_WEB",
    "MARKET_GAP_INCLUDE_GATE_SPREAD_PAIR",
    "MARKET_GAP_INCLUDE_GATE_GATE_WEB_GAP",
    "MARKET_GAP_INCLUDE_PRICE_CHANGE",
    "MARKET_GAP_INCLUDE_PRICE_CHANGE_GAP_CORR",
    "CORR_QUOTE_EXCHANGES",
    "CORR_RETURN_SECONDS_OVERRIDES",
    "PRICE_CHANGE_REDIS_PREFIX",
    "PRICE_CHANGE_SOURCES",
    "HEARTBEAT_REDIS_PREFIX",
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_CHAT_ID",
    "ALERT_AFTER_CONSECUTIVE_FAILURES",
]


@pytest.fixture
def clean_env(monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
    for k in _KEYS:
        monkeypatch.delenv(k, raising=False)
    yield


def test_load_config_defaults(clean_env: None) -> None:
    cfg = load_config([])
    assert cfg.questdb_url == "http://localhost:9000"
    assert cfg.redis_url == "redis://localhost:6379"
    assert cfg.window_mode == "all"
    assert cfg.base_exchange == "gate"
    assert cfg.quote_exchanges == ("binance",)
    assert cfg.include_gate_web is True
    assert cfg.gap_bases == ("gate", "gate_web")
    assert cfg.include_spread_pair is True
    assert cfg.include_price_change is True
    assert cfg.price_change_sources == ("gate", "binance")
    assert cfg.heartbeat_prefix == "gate_hft:_meta:market_state_updater"
    assert cfg.telegram_bot_token is None
    assert cfg.telegram_chat_id is None
    assert cfg.alert_after_consecutive_failures == 5


def test_load_config_disable_gate_web(
    clean_env: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("MARKET_GAP_INCLUDE_GATE_WEB", "0")
    cfg = load_config([])
    assert cfg.include_gate_web is False
    assert cfg.gap_bases == ("gate",)


def test_load_config_csv_exchanges(clean_env: None) -> None:
    cfg = load_config(["--exchanges", "binance,bybit,bitget"])
    assert cfg.quote_exchanges == ("binance", "bybit", "bitget")


def test_load_config_once_zeros_interval(clean_env: None) -> None:
    cfg = load_config(["--once"])
    assert cfg.once is True
    assert cfg.interval_secs == 0


def test_load_config_empty_exchanges_exits(
    clean_env: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("MARKET_GAP_QUOTE_EXCHANGES", "")
    with pytest.raises(SystemExit):
        load_config(["--exchanges", ""])


def test_gap_bases_when_base_is_gate_web(
    clean_env: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    """base 가 이미 gate_web 이면 중복 추가 안 함."""
    monkeypatch.setenv("MARKET_GAP_BASE", "gate_web")
    cfg = load_config([])
    assert cfg.gap_bases == ("gate_web",)


def test_load_config_window_mode_cli(clean_env: None) -> None:
    assert load_config(["--windows", "fast"]).window_mode == "fast"
    assert load_config(["--windows", "slow"]).window_mode == "slow"
    assert load_config(["--windows", "all"]).window_mode == "all"


def test_load_config_window_mode_env(
    clean_env: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("WINDOW_MODE", "fast")
    assert load_config([]).window_mode == "fast"


def test_load_config_window_mode_invalid_exits(clean_env: None) -> None:
    with pytest.raises(SystemExit):
        load_config(["--windows", "nope"])


def test_load_config_corr_defaults(clean_env: None) -> None:
    cfg = load_config([])
    assert cfg.include_price_change_gap_corr is True
    assert cfg.corr_quote_exchanges == ("binance",)
    assert cfg.corr_return_seconds_overrides == {}


def test_load_config_corr_disabled(
    clean_env: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("MARKET_GAP_INCLUDE_PRICE_CHANGE_GAP_CORR", "0")
    assert load_config([]).include_price_change_gap_corr is False


def test_load_config_corr_overrides_parsed(
    clean_env: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("CORR_RETURN_SECONDS_OVERRIDES", "60:60,240:120")
    cfg = load_config([])
    assert cfg.corr_return_seconds_overrides == {60: 60, 240: 120}


def test_load_config_telegram_envs(
    clean_env: None, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("TELEGRAM_BOT_TOKEN", "abc")
    monkeypatch.setenv("TELEGRAM_CHAT_ID", "12345")
    monkeypatch.setenv("ALERT_AFTER_CONSECUTIVE_FAILURES", "3")
    cfg = load_config([])
    assert cfg.telegram_bot_token == "abc"
    assert cfg.telegram_chat_id == "12345"
    assert cfg.alert_after_consecutive_failures == 3

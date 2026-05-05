from __future__ import annotations

import json
from collections.abc import Iterator
from pathlib import Path

import pytest

from market_state_updater.config import load_config

# config.json / env 둘 다 영향 주는 키 — fixture 에서 다 비움.
_KEYS = [
    "QUESTDB_URL",
    "QUESTDB_BACKUP_URL",
    "REDIS_URL",
    "REDIS_BACKUP_URL",
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
    "MARKET_GAP_INCLUDE_MID_CORR",
    "MARKET_GAP_FILL_PREV_LOOKBACK_MINUTES",
    "CORR_QUOTE_EXCHANGES",
    "CORR_RETURN_SECONDS_OVERRIDES",
    "MID_CORR_REDIS_PREFIX",
    "MID_CORR_QUOTE_EXCHANGES",
    "MID_CORR_MIN_SAMPLES",
    "PRICE_CHANGE_REDIS_PREFIX",
    "PRICE_CHANGE_SOURCES",
    "HEARTBEAT_REDIS_PREFIX",
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_CHAT_ID",
    "ALERT_AFTER_CONSECUTIVE_FAILURES",
]


@pytest.fixture
def clean_env(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> Iterator[Path]:
    """env 를 비우고 cwd 를 tmp_path 로 이동.

    `_resolve_config_path` 가 cwd → 패키지 루트 순으로 폴백하니, tmp 에 빈 config 를
    두면 패키지 루트의 실제 config.json 가 잡히지 않음 (테스트 격리).
    개별 테스트가 `_write_cfg` 로 다시 쓰면 덮어씀.
    """
    for k in _KEYS:
        monkeypatch.delenv(k, raising=False)
    monkeypatch.chdir(tmp_path)
    (tmp_path / "config.json").write_text("{}")
    yield tmp_path


def _write_cfg(dir_: Path, cfg: dict) -> Path:
    p = dir_ / "config.json"
    p.write_text(json.dumps(cfg))
    return p


def test_load_config_pure_defaults(clean_env: Path) -> None:
    cfg = load_config([])
    assert cfg.questdb_url == "http://localhost:9000"
    assert cfg.questdb_backup_url is None
    assert cfg.questdb_urls == ("http://localhost:9000",)
    assert cfg.redis_url == "redis://localhost:6379"
    assert cfg.redis_backup_url is None
    assert cfg.redis_urls == ("redis://localhost:6379",)
    assert cfg.window_mode == "all"
    assert cfg.base_exchange == "gate"
    assert cfg.quote_exchanges == ("binance",)
    assert cfg.include_gate_web is True
    assert cfg.gap_bases == ("gate", "gate_web")
    assert cfg.include_spread_pair is True
    assert cfg.include_price_change is True
    assert cfg.include_price_change_gap_corr is True
    assert cfg.price_change_sources == ("gate", "binance")
    assert cfg.corr_quote_exchanges == ("binance",)
    assert cfg.corr_return_seconds_overrides == {}
    assert cfg.heartbeat_prefix == "gate_hft:_meta:market_state_updater"
    assert cfg.telegram_bot_token is None
    assert cfg.alert_after_consecutive_failures == 5


def test_config_json_loaded_from_cwd(clean_env: Path) -> None:
    _write_cfg(
        clean_env,
        {
            "questdb_url": "http://primary:9000",
            "questdb_backup_url": "http://backup:9000",
            "redis_url": "redis://primary:6379",
            "redis_backup_url": "redis://backup:6379",
            "interval_secs": 33,
            "window_mode": "fast",
            "market_gap": {
                "redis_prefix": "test:mg",
                "base_exchange": "binance",
                "quote_exchanges": ["bybit", "bitget"],
                "fill_prev_lookback_minutes": 7,
            },
            "include": {"gate_web": False, "price_change_gap_corr": False},
            "price_change": {"sources": ["gate"]},
            "corr": {"return_seconds_overrides": {"60": 60, "240": 120}},
            "heartbeat": {"redis_prefix": "test:hb"},
            "alert_after_consecutive_failures": 9,
        },
    )
    cfg = load_config([])
    assert cfg.questdb_url == "http://primary:9000"
    assert cfg.questdb_backup_url == "http://backup:9000"
    assert cfg.questdb_urls == ("http://primary:9000", "http://backup:9000")
    assert cfg.redis_url == "redis://primary:6379"
    assert cfg.redis_backup_url == "redis://backup:6379"
    assert cfg.redis_urls == ("redis://primary:6379", "redis://backup:6379")
    assert cfg.interval_secs == 33
    assert cfg.window_mode == "fast"
    assert cfg.market_gap_prefix == "test:mg"
    assert cfg.base_exchange == "binance"
    assert cfg.quote_exchanges == ("bybit", "bitget")
    assert cfg.include_gate_web is False
    assert cfg.gap_bases == ("binance",)  # gate_web disabled
    assert cfg.include_price_change_gap_corr is False
    assert cfg.price_change_sources == ("gate",)
    assert cfg.corr_return_seconds_overrides == {60: 60, 240: 120}
    assert cfg.heartbeat_prefix == "test:hb"
    assert cfg.alert_after_consecutive_failures == 9


def test_config_json_explicit_path(clean_env: Path) -> None:
    """--config <path> 으로 임의 경로의 JSON 로드."""
    p = clean_env / "subdir"
    p.mkdir()
    cfg_path = _write_cfg(p, {"interval_secs": 99})
    cfg = load_config(["--config", str(cfg_path)])
    assert cfg.interval_secs == 99


def test_env_overrides_config_json(
    clean_env: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    _write_cfg(clean_env, {"window_mode": "fast", "interval_secs": 30})
    monkeypatch.setenv("WINDOW_MODE", "slow")
    monkeypatch.setenv("UPDATE_INTERVAL_SECS", "120")
    cfg = load_config([])
    assert cfg.window_mode == "slow"
    assert cfg.interval_secs == 120


def test_cli_overrides_env(clean_env: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _write_cfg(clean_env, {"window_mode": "fast"})
    monkeypatch.setenv("WINDOW_MODE", "slow")
    cfg = load_config(["--windows", "all"])
    assert cfg.window_mode == "all"


def test_cli_exchanges_overrides_config(clean_env: Path) -> None:
    _write_cfg(clean_env, {"market_gap": {"quote_exchanges": ["binance"]}})
    cfg = load_config(["--exchanges", "bybit,bitget"])
    assert cfg.quote_exchanges == ("bybit", "bitget")


def test_once_zeros_interval(clean_env: Path) -> None:
    _write_cfg(clean_env, {"interval_secs": 30})
    cfg = load_config(["--once"])
    assert cfg.once is True
    assert cfg.interval_secs == 0


def test_empty_quote_exchanges_exits(clean_env: Path) -> None:
    _write_cfg(clean_env, {"market_gap": {"quote_exchanges": []}})
    with pytest.raises(SystemExit):
        load_config([])


def test_invalid_window_mode_exits(clean_env: Path) -> None:
    _write_cfg(clean_env, {"window_mode": "nope"})
    with pytest.raises(SystemExit):
        load_config([])


def test_telegram_envs_picked_up(
    clean_env: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("TELEGRAM_BOT_TOKEN", "abc")
    monkeypatch.setenv("TELEGRAM_CHAT_ID", "12345")
    cfg = load_config([])
    assert cfg.telegram_bot_token == "abc"
    assert cfg.telegram_chat_id == "12345"


def test_dollar_keys_in_config_ignored(clean_env: Path) -> None:
    """`$comment` 같은 키는 schema 가 아니라 메타 → ignore."""
    _write_cfg(
        clean_env,
        {"$comment": "blah", "$schema_version": 1, "interval_secs": 7},
    )
    cfg = load_config([])
    assert cfg.interval_secs == 7


def test_fill_prev_lookback_propagated_to_env(clean_env: Path) -> None:
    """jobs/common.py 가 import 시 env 만 보니, file 값을 env 로 주입해서 통일."""
    import os

    _write_cfg(clean_env, {"market_gap": {"fill_prev_lookback_minutes": 13}})
    load_config([])
    assert os.environ["MARKET_GAP_FILL_PREV_LOOKBACK_MINUTES"] == "13"

"""실데이터 통합 테스트.

REDIS_URL / QUESTDB_URL 가 .env 또는 환경에 있으면 실행, 없으면 skip.
실제 QuestDB 1m 윈도우 쿼리 → Redis 에 blob 적재 → 다시 읽어서 shape 검증.
"""

from __future__ import annotations

import json
import os
import time

import pytest
import redis as redis_mod
from dotenv import load_dotenv

load_dotenv()

QUESTDB_URL = os.getenv("QUESTDB_URL")
REDIS_URL = os.getenv("REDIS_URL")
PREFIX = os.getenv("MARKET_GAP_REDIS_PREFIX", "gate_hft:market_gap")


def _redis_alive() -> bool:
    if not REDIS_URL:
        return False
    try:
        r = redis_mod.from_url(REDIS_URL, socket_connect_timeout=2)
        return bool(r.ping())
    except Exception:  # noqa: BLE001
        return False


def _questdb_alive() -> bool:
    if not QUESTDB_URL:
        return False
    try:
        from market_state_updater.questdb import questdb_exec

        questdb_exec(QUESTDB_URL, "SELECT 1")
        return True
    except Exception:  # noqa: BLE001
        return False


pytestmark = pytest.mark.skipif(
    not (_redis_alive() and _questdb_alive()),
    reason="QUESTDB_URL/REDIS_URL not configured or unreachable",
)


@pytest.fixture(scope="module")
def redis_client() -> redis_mod.Redis:
    assert REDIS_URL is not None
    return redis_mod.from_url(REDIS_URL)


def _scratch_prefix() -> str:
    """이 테스트가 자기 키만 건드리도록 별도 prefix."""
    return f"{PREFIX}:_test_{int(time.time())}"


def test_gap_job_writes_expected_shape(redis_client: redis_mod.Redis) -> None:
    from market_state_updater.jobs import gap

    assert QUESTDB_URL is not None
    prefix = _scratch_prefix()
    try:
        ok = gap.run(QUESTDB_URL, redis_client, prefix, "gate", "binance", 1)
        assert ok is True
        raw = redis_client.get(f"{prefix}:gate:binance:1")
        assert raw is not None
        blob = json.loads(raw)
        assert blob["base_exchange"] == "gate"
        assert blob["quote_exchange"] == "binance"
        assert blob["window_minutes"] == 1
        assert isinstance(blob["updated_at_ms"], int)
        assert isinstance(blob["avg_mid_gap_by_symbol"], dict)
        assert isinstance(blob["avg_spread_by_symbol"], dict)
        # 1m 윈도우라도 정상이면 심볼 1개 이상은 있어야
        assert len(blob["avg_mid_gap_by_symbol"]) > 0
        # filter_valid_gaps 가 동작했는지: 모든 값이 |x| < 0.1
        for v in blob["avg_mid_gap_by_symbol"].values():
            assert abs(v) < 0.1
    finally:
        for key in redis_client.scan_iter(f"{prefix}:*"):
            redis_client.delete(key)


def test_spread_pair_job_writes_expected_shape(redis_client: redis_mod.Redis) -> None:
    from market_state_updater.jobs import spread_pair

    assert QUESTDB_URL is not None
    prefix = _scratch_prefix()
    try:
        ok = spread_pair.run(QUESTDB_URL, redis_client, prefix, 1)
        assert ok is True
        raw = redis_client.get(f"{prefix}:gate_gate_web_spreads:1")
        assert raw is not None
        blob = json.loads(raw)
        assert blob["window_minutes"] == 1
        gate_map = blob["avg_spread_gate_bookticker_by_symbol"]
        web_map = blob["avg_spread_gate_webbookticker_by_symbol"]
        assert isinstance(gate_map, dict)
        assert isinstance(web_map, dict)
        # 두 map 의 심볼 집합이 동일해야 (run() 에서 intersection 한 후 적재)
        assert set(gate_map.keys()) == set(web_map.keys())
    finally:
        for key in redis_client.scan_iter(f"{prefix}:*"):
            redis_client.delete(key)


def test_price_change_job_writes_expected_shape(redis_client: redis_mod.Redis) -> None:
    from market_state_updater.jobs import price_change

    assert QUESTDB_URL is not None
    prefix = f"gate_hft:price_change:_test_{int(time.time())}"
    try:
        ok = price_change.run(QUESTDB_URL, redis_client, prefix, "gate", 5)
        assert ok is True
        raw = redis_client.get(f"{prefix}:gate:5")
        assert raw is not None
        blob = json.loads(raw)
        assert blob["source"] == "gate"
        assert blob["window_minutes"] == 5
        assert isinstance(blob["price_change_by_symbol"], dict)
        assert len(blob["price_change_by_symbol"]) > 0
    finally:
        for key in redis_client.scan_iter(f"{prefix}:*"):
            redis_client.delete(key)

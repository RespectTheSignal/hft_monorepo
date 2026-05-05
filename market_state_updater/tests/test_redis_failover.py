from __future__ import annotations

import pytest

from market_state_updater.redis_failover import RedisFailover


class FakeRedis:
    def __init__(self, *, fail_set: bool = False, fail_get: bool = False) -> None:
        self.fail_set = fail_set
        self.fail_get = fail_get
        self.set_calls: list[tuple[str, str]] = []
        self.get_calls: list[str] = []

    def set(self, key: str, value: str) -> bool:
        self.set_calls.append((key, value))
        if self.fail_set:
            raise RuntimeError("set failed")
        return True

    def get(self, key: str) -> str:
        self.get_calls.append(key)
        if self.fail_get:
            raise RuntimeError("get failed")
        return "value"


def test_set_is_mirrored_to_all_redis_clients() -> None:
    primary = FakeRedis()
    backup = FakeRedis()
    client = RedisFailover([primary, backup], ["redis://primary", "redis://backup"])

    assert client.set("k", "v") is True

    assert primary.set_calls == [("k", "v")]
    assert backup.set_calls == [("k", "v")]


def test_set_succeeds_when_primary_fails_but_backup_updates() -> None:
    primary = FakeRedis(fail_set=True)
    backup = FakeRedis()
    client = RedisFailover([primary, backup], ["redis://primary", "redis://backup"])

    assert client.set("k", "v") is True

    assert primary.set_calls == [("k", "v")]
    assert backup.set_calls == [("k", "v")]


def test_read_falls_back_to_backup() -> None:
    primary = FakeRedis(fail_get=True)
    backup = FakeRedis()
    client = RedisFailover([primary, backup], ["redis://primary", "redis://backup"])

    assert client.get("k") == "value"

    assert primary.get_calls == ["k"]
    assert backup.get_calls == ["k"]


def test_write_raises_when_all_redis_clients_fail() -> None:
    client = RedisFailover(
        [FakeRedis(fail_set=True), FakeRedis(fail_set=True)],
        ["redis://primary", "redis://backup"],
    )

    with pytest.raises(RuntimeError, match="set failed"):
        client.set("k", "v")

from __future__ import annotations

from typing import Any

import pytest

from market_state_updater import questdb
from market_state_updater.questdb import parse_questdb_url


def test_parse_url_no_auth() -> None:
    base, u, p = parse_questdb_url("http://localhost:9000")
    assert base == "http://localhost:9000"
    assert u is None
    assert p is None


def test_parse_url_with_auth() -> None:
    base, u, p = parse_questdb_url("http://admin:secret@db.example.com:9000")
    assert base == "http://db.example.com:9000"
    assert u == "admin"
    assert p == "secret"
    # 자격증명이 base_url 에서 제거됐는지
    assert "admin" not in base
    assert "secret" not in base


def test_parse_url_with_path_strips_trailing_slash() -> None:
    base, _u, _p = parse_questdb_url("http://localhost:9000/api/")
    assert base == "http://localhost:9000/api"


def test_exec_tries_backup_after_primary_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[str] = []

    def fake_exec_json(
        base_url: str,
        query: str,
        *,
        auth: Any,
        timeout: int | float,
    ) -> dict[str, Any]:
        calls.append(base_url)
        if base_url == "http://primary:9000":
            raise RuntimeError("primary down")
        return {"query": query, "dataset": [[1]]}

    monkeypatch.setattr(questdb, "questdb_exec_json", fake_exec_json)

    result = questdb.questdb_exec(
        ("http://primary:9000", "http://backup:9000"), "SELECT 1"
    )

    assert result == {"query": "SELECT 1", "dataset": [[1]]}
    assert calls == ["http://primary:9000", "http://backup:9000"]


def test_exec_raises_last_error_when_all_urls_fail(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def fake_exec_json(
        base_url: str,
        query: str,
        *,
        auth: Any,
        timeout: int | float,
    ) -> dict[str, Any]:
        raise RuntimeError(f"{base_url} down")

    monkeypatch.setattr(questdb, "questdb_exec_json", fake_exec_json)

    with pytest.raises(RuntimeError, match="backup:9000 down"):
        questdb.questdb_exec(("http://primary:9000", "http://backup:9000"), "SELECT 1")

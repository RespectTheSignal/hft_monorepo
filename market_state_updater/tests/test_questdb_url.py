from __future__ import annotations

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

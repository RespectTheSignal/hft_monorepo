"""High-level QuestDB exec wrapper: URL의 user:pass 분리 + timeout 적용."""

from __future__ import annotations

import os
from typing import Any
from urllib.parse import urlparse, urlunparse

import requests

from market_state_updater.questdb_http import questdb_exec_json

QUESTDB_QUERY_TIMEOUT_SECS = int(os.getenv("QUESTDB_QUERY_TIMEOUT_SECS", "120"))


def parse_questdb_url(questdb_url: str) -> tuple[str, str | None, str | None]:
    """QUESTDB_URL 파싱; (base_url_without_auth, username, password) 반환.

    http://user:password@host:port 형태 지원 (password URL-encoded 가능).
    base_url 은 로그에 찍어도 자격증명 노출 안 되도록 userinfo 제거.
    """
    parsed = urlparse(questdb_url)
    username = parsed.username if parsed.username is not None else None
    password = parsed.password if parsed.password is not None else None
    netloc = parsed.hostname or ""
    if parsed.port is not None:
        netloc = f"{netloc}:{parsed.port}"
    clean = urlunparse(
        (parsed.scheme, netloc, parsed.path.rstrip("/") or "", "", "", "")
    )
    return clean, username, password


def questdb_exec(questdb_url: str, query: str) -> dict[str, Any]:
    base_url, username, password = parse_questdb_url(questdb_url)
    auth = None
    if username is not None and password is not None:
        auth = requests.auth.HTTPBasicAuth(username, password)
    return questdb_exec_json(
        base_url, query, auth=auth, timeout=QUESTDB_QUERY_TIMEOUT_SECS
    )

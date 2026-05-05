"""High-level QuestDB exec wrapper: URL의 user:pass 분리 + timeout 적용."""

from __future__ import annotations

import os
from collections.abc import Sequence
from typing import Any
from urllib.parse import urlparse, urlunparse

import requests
import structlog

from market_state_updater.questdb_http import questdb_exec_json

QUESTDB_QUERY_TIMEOUT_SECS = int(os.getenv("QUESTDB_QUERY_TIMEOUT_SECS", "120"))
QuestDBUrlArg = str | Sequence[str]

logger = structlog.get_logger(__name__)


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


def _questdb_urls(questdb_url: QuestDBUrlArg) -> tuple[str, ...]:
    if isinstance(questdb_url, str):
        urls = (questdb_url,)
    else:
        urls = tuple(questdb_url)
    return tuple(url for url in urls if url.strip())


def _questdb_exec_one(questdb_url: str, query: str) -> dict[str, Any]:
    base_url, username, password = parse_questdb_url(questdb_url)
    auth = None
    if username is not None and password is not None:
        auth = requests.auth.HTTPBasicAuth(username, password)
    return questdb_exec_json(
        base_url, query, auth=auth, timeout=QUESTDB_QUERY_TIMEOUT_SECS
    )


def questdb_exec(questdb_url: QuestDBUrlArg, query: str) -> dict[str, Any]:
    urls = _questdb_urls(questdb_url)
    if not urls:
        raise ValueError("QuestDB URL is empty")

    last_error: Exception | None = None
    for idx, url in enumerate(urls):
        try:
            return _questdb_exec_one(url, query)
        except Exception as e:  # noqa: BLE001
            last_error = e
            if idx == len(urls) - 1:
                break
            base_url, _, _ = parse_questdb_url(url)
            next_base_url, _, _ = parse_questdb_url(urls[idx + 1])
            logger.warning(
                "questdb_primary_failed_trying_backup",
                failed_url=base_url,
                backup_url=next_base_url,
                error=str(e),
            )

    assert last_error is not None
    raise last_error

"""QuestDB HTTP /exec helper (gate_hft/questdb_http.py 에서 vendored).

GET ?query=... 만 지원하는 배포본이 많아서 기본은 GET. 긴 SQL/프록시 환경에서는 POST 폴백.

Env:
  QUESTDB_MAX_GET_QUERY_LEN - GET 사용 가능한 query 최대 길이 (default 7500).
  QUESTDB_EXEC_USE_POST     - 1/true 면 POST 우선 시도 (POST 선호 서버용).
"""

from __future__ import annotations

import os
from typing import Any

import requests


def questdb_exec_request(
    base_url: str,
    query: str,
    *,
    auth: requests.auth.HTTPBasicAuth | None,
    timeout: int | float,
    extra_headers: dict[str, str] | None = None,
) -> requests.Response:
    """Execute SQL against QuestDB /exec (GET 우선, 필요 시 POST)."""
    exec_url = f"{base_url.rstrip('/')}/exec"
    max_get = int(os.getenv("QUESTDB_MAX_GET_QUERY_LEN", "7500"))
    prefer_post = os.getenv("QUESTDB_EXEC_USE_POST", "").lower() in ("1", "true", "yes")
    headers: dict[str, str] = {"Accept": "application/json"}
    if extra_headers:
        headers.update(extra_headers)

    def _post() -> requests.Response:
        return requests.post(
            exec_url, data={"query": query}, auth=auth, timeout=timeout, headers=headers
        )

    def _get() -> requests.Response:
        return requests.get(
            exec_url, params={"query": query}, auth=auth, timeout=timeout, headers=headers
        )

    if prefer_post:
        r = _post()
        if r.ok:
            return r
        if r.status_code == 405 and len(query) <= max_get:
            r = _get()
        return r

    if len(query) <= max_get:
        r = _get()
        if r.ok:
            return r
        if r.status_code in (400, 414, 431):
            r2 = _post()
            if r2.ok:
                return r2
        return r

    r = _post()
    if r.status_code == 405:
        raise RuntimeError(
            "QuestDB: POST /exec not supported (405) but SQL exceeds GET URL length limit. "
            "Increase nginx/proxy large_client_header_buffers, raise QUESTDB_MAX_GET_QUERY_LEN "
            "after server tuning, shorten SQL, or set QUESTDB_EXEC_USE_POST=1 if your server "
            "actually supports POST."
        )
    return r


def questdb_exec_json(
    base_url: str,
    query: str,
    *,
    auth: requests.auth.HTTPBasicAuth | None,
    timeout: int | float,
    extra_headers: dict[str, str] | None = None,
) -> dict[str, Any]:
    """questdb_exec_request → JSON 파싱; HTTP/QuestDB 에러는 RuntimeError."""
    resp = questdb_exec_request(
        base_url, query, auth=auth, timeout=timeout, extra_headers=extra_headers
    )
    if not resp.ok:
        raise RuntimeError(f"QuestDB HTTP {resp.status_code}: {resp.text[:4000]}")
    data = resp.json()
    if isinstance(data, dict) and data.get("error"):
        raise RuntimeError(f"QuestDB error: {data.get('error')}")
    return data

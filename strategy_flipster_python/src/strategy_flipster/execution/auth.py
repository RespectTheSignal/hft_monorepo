"""Flipster HMAC-SHA256 서명 — Rust auth.rs 대응"""

from __future__ import annotations

import hashlib
import hmac
import time


def make_expires(offset_secs: int = 60) -> int:
    """현재 시각 + offset_secs (초) → expires timestamp"""
    return int(time.time()) + offset_secs


def sign(
    secret: str,
    method: str,
    path: str,
    expires: int,
    body: str | None = None,
) -> str:
    """HMAC-SHA256 서명 생성.

    message = METHOD + path + expires [+ body]
    Flipster REST/WS 공통 사용.
    """
    message = method.upper() + path + str(expires)
    if body:
        message += body
    return hmac.new(
        secret.encode("utf-8"),
        message.encode("utf-8"),
        hashlib.sha256,
    ).hexdigest()


def make_auth_headers(
    api_key: str,
    api_secret: str,
    method: str,
    path: str,
    body: str | None = None,
    expires_offset: int = 60,
) -> dict[str, str]:
    """Flipster REST 요청용 인증 헤더 생성"""
    expires = make_expires(expires_offset)
    signature = sign(api_secret, method, path, expires, body)
    return {
        "api-key": api_key,
        "api-expires": str(expires),
        "api-signature": signature,
    }


def make_ws_auth_headers(
    api_key: str,
    api_secret: str,
    expires_offset: int = 60,
) -> dict[str, str]:
    """Flipster WS 연결용 인증 헤더 생성"""
    expires = make_expires(expires_offset)
    signature = sign(api_secret, "GET", "/api/v1/stream", expires)
    return {
        "api-key": api_key,
        "api-expires": str(expires),
        "api-signature": signature,
    }

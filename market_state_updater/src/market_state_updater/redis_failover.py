"""Redis client wrapper with write mirroring and read failover."""

from __future__ import annotations

from collections.abc import Callable, Sequence
from typing import Any
from urllib.parse import urlparse, urlunparse

import redis
import structlog

logger = structlog.get_logger(__name__)

MIRRORED_COMMANDS = {
    "set",
    "setex",
    "psetex",
    "mset",
    "hset",
    "hmset",
    "delete",
    "expire",
    "pexpire",
}


def _safe_redis_url(redis_url: str) -> str:
    parsed = urlparse(redis_url)
    netloc = parsed.hostname or ""
    if parsed.port is not None:
        netloc = f"{netloc}:{parsed.port}"
    return urlunparse((parsed.scheme, netloc, parsed.path, "", "", ""))


class RedisFailover:
    """Mirror write commands to all Redis nodes; fail over other commands."""

    def __init__(self, clients: Sequence[redis.Redis], labels: Sequence[str]) -> None:
        if not clients:
            raise ValueError("at least one Redis client is required")
        self._clients = tuple(clients)
        self._labels = tuple(labels)

    @classmethod
    def from_urls(cls, redis_urls: Sequence[str]) -> "RedisFailover":
        urls = tuple(url for url in redis_urls if url.strip())
        if not urls:
            raise ValueError("Redis URL is empty")
        return cls(
            tuple(redis.from_url(url) for url in urls),
            tuple(_safe_redis_url(url) for url in urls),
        )

    def ping_any(self) -> bool:
        last_error: Exception | None = None
        for idx, client in enumerate(self._clients):
            try:
                client.ping()
                return True
            except Exception as e:  # noqa: BLE001
                last_error = e
                logger.warning(
                    "redis_ping_failed",
                    redis_url=self._labels[idx],
                    error=str(e),
                )
        if last_error is not None:
            raise last_error
        return False

    def __getattr__(self, name: str) -> Any:
        primary_attr = getattr(self._clients[0], name)
        if not callable(primary_attr):
            return primary_attr

        def call_with_failover(*args: Any, **kwargs: Any) -> Any:
            return self._call(name, *args, **kwargs)

        return call_with_failover

    def _call(self, name: str, *args: Any, **kwargs: Any) -> Any:
        if name.lower() in MIRRORED_COMMANDS:
            return self._call_all(name, *args, **kwargs)
        return self._call_first_success(name, *args, **kwargs)

    def _call_all(self, name: str, *args: Any, **kwargs: Any) -> Any:
        first_result: Any = None
        has_success = False
        last_error: Exception | None = None
        for idx, client in enumerate(self._clients):
            fn: Callable[..., Any] = getattr(client, name)
            try:
                result = fn(*args, **kwargs)
                if not has_success:
                    first_result = result
                    has_success = True
            except Exception as e:  # noqa: BLE001
                last_error = e
                logger.warning(
                    "redis_mirror_update_failed",
                    command=name,
                    redis_url=self._labels[idx],
                    error=str(e),
                )

        if has_success:
            return first_result
        assert last_error is not None
        raise last_error

    def _call_first_success(self, name: str, *args: Any, **kwargs: Any) -> Any:
        last_error: Exception | None = None
        for idx, client in enumerate(self._clients):
            fn: Callable[..., Any] = getattr(client, name)
            try:
                return fn(*args, **kwargs)
            except Exception as e:  # noqa: BLE001
                last_error = e
                if idx == len(self._clients) - 1:
                    break
                logger.warning(
                    "redis_primary_failed_trying_backup",
                    command=name,
                    failed_url=self._labels[idx],
                    backup_url=self._labels[idx + 1],
                    error=str(e),
                )

        assert last_error is not None
        raise last_error

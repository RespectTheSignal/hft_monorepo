"""에러 타입 — Rust Result<T, E> 대응"""

from __future__ import annotations

import enum
from dataclasses import dataclass


class ErrorKind(enum.Enum):
    CONFIG = "config"
    NETWORK = "network"
    AUTH = "auth"
    API = "api"
    PROTOCOL = "protocol"
    TIMEOUT = "timeout"
    ORDER = "order"


@dataclass(frozen=True, slots=True)
class AppError:
    """구조화된 에러 — Rust AppError 대응"""

    kind: ErrorKind
    message: str
    status_code: int | None = None


class AppException(Exception):
    """에러 경계에서 사용하는 예외 — Rust의 ? 연산자 대응"""

    def __init__(self, error: AppError) -> None:
        self.error: AppError = error
        super().__init__(f"[{error.kind.value}] {error.message}")

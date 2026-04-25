"""Telegram Bot 알림 (fire-and-forget, single chat).

gate_hft 는 봇 폴링 + 다구독자 패턴이지만 여기는 단순 operator alert 용도라
TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID 둘 다 있을 때만 활성. 하나라도 없으면 no-op.

전송 실패해도 main loop 를 죽이지 않음 (모든 예외 swallow + structlog 에러 로그).
"""

from __future__ import annotations

import requests
import structlog

logger = structlog.get_logger(__name__)


class TelegramNotifier:
    def __init__(
        self,
        token: str | None,
        chat_id: str | None,
        timeout_secs: float = 10.0,
    ) -> None:
        self._token = token or None
        self._chat_id = chat_id or None
        self._timeout = timeout_secs

    @property
    def enabled(self) -> bool:
        return bool(self._token and self._chat_id)

    def send(self, text: str) -> bool:
        """Telegram 으로 메시지 전송. 비활성/실패 시 False 반환 (예외 안 던짐)."""
        if not self.enabled:
            return False
        url = f"https://api.telegram.org/bot{self._token}/sendMessage"
        try:
            r = requests.post(
                url,
                data={
                    "chat_id": self._chat_id,
                    "text": text,
                    "parse_mode": "HTML",
                    "disable_web_page_preview": "true",
                },
                timeout=self._timeout,
            )
            if not r.ok:
                logger.error(
                    "telegram_send_failed",
                    status=r.status_code,
                    body=r.text[:200],
                )
                return False
            return True
        except Exception as e:  # noqa: BLE001
            logger.error("telegram_send_exception", error=str(e))
            return False

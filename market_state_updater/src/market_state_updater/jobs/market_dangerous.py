"""전체 마켓 message rate 기반 dangerous detector → Redis (single boolean blob).

Rules (둘 중 하나라도 trigger 면 dangerous):
  - gate_exceeds_compare:  primary count > compare count (보통 gate > binance 면 비정상)
  - high_message_rate:     primary count > absolute_threshold

Sliding sticky: 재트리거마다 sticky_until_ms 갱신.
False → True 전환 시 Telegram 알림 (sticky 안에선 dedup).

Redis key: 단일 JSON string (per-symbol 아님).

Blob:
  {
    "value": true,
    "updated_at_ms": ...,
    "sticky_until_ms": ...,
    "tags": ["gate_exceeds_compare", "high_message_rate"],
    "primary_count": 200000,
    "compare_count": 50000,
    "window_secs": 60,
    "absolute_threshold": 150000,
    "primary_table": "gate_bookticker",
    "compare_table": "binance_bookticker"
  }

State: process-wide (in-memory). 재시작 시 sticky 잃음 (의도적 단순화).
"""

from __future__ import annotations

import json
import time
from typing import TYPE_CHECKING

import redis
import structlog

from market_state_updater.questdb import questdb_exec

if TYPE_CHECKING:
    from market_state_updater.notifier import TelegramNotifier

logger = structlog.get_logger(__name__)

TAG_GATE_EXCEEDS = "gate_exceeds_compare"
TAG_HIGH_RATE = "high_message_rate"


def build_count_query(table: str, window_secs: int) -> str:
    window_secs = max(int(window_secs), 1)
    return (
        f"SELECT count() AS n FROM {table} "
        f"WHERE timestamp > dateadd('s', -{window_secs}, now())"
    )


def parse_count(payload: dict) -> int:
    ds = payload.get("dataset") or []
    if not ds or not isinstance(ds[0], list) or not ds[0]:
        return 0
    v = ds[0][0]
    return int(v) if v is not None else 0


class MarketDangerousJob:
    def __init__(
        self,
        *,
        primary_table: str,
        compare_table: str,
        absolute_threshold: int,
        window_secs: int,
        sticky_secs: int,
        redis_key: str,
        notifier: "TelegramNotifier | None",
    ) -> None:
        self.primary_table = primary_table
        self.compare_table = compare_table
        self.absolute_threshold = int(absolute_threshold)
        self.window_secs = int(window_secs)
        self.sticky_secs = int(sticky_secs)
        self.redis_key = redis_key
        self.notifier = notifier

        # in-memory state
        self.sticky_until_ms: int = 0
        self.last_dangerous: bool = False
        self.last_tags: list[str] = []

    def _evaluate_rules(self, primary: int, compare: int) -> list[str]:
        tags: list[str] = []
        if primary > compare:
            tags.append(TAG_GATE_EXCEEDS)
        if primary > self.absolute_threshold:
            tags.append(TAG_HIGH_RATE)
        return tags

    def run(self, questdb_url: str, redis_client: redis.Redis) -> bool:
        log = logger.bind(job="market_dangerous", redis_key=self.redis_key)
        t0 = time.monotonic()
        try:
            p_payload = questdb_exec(
                questdb_url, build_count_query(self.primary_table, self.window_secs)
            )
            c_payload = questdb_exec(
                questdb_url, build_count_query(self.compare_table, self.window_secs)
            )
            primary_count = parse_count(p_payload)
            compare_count = parse_count(c_payload)
        except Exception as e:  # noqa: BLE001
            log.error(
                "questdb_failed",
                error=str(e),
                query_ms=int((time.monotonic() - t0) * 1000),
            )
            return False
        query_ms = int((time.monotonic() - t0) * 1000)
        now_ms = int(time.time() * 1000)

        triggered_tags = self._evaluate_rules(primary_count, compare_count)
        triggered_now = bool(triggered_tags)
        if triggered_now:
            # sliding sticky — 재트리거마다 timer 연장
            self.sticky_until_ms = now_ms + self.sticky_secs * 1000

        in_sticky = now_ms < self.sticky_until_ms
        dangerous = triggered_now or in_sticky

        # blob 의 tags: 현재 trigger 된 것만 (sticky 일 땐 빈 list 가능 — sticky 라는 사실은
        # sticky_until_ms 로 표현)
        blob = {
            "value": dangerous,
            "updated_at_ms": now_ms,
            "sticky_until_ms": self.sticky_until_ms,
            "tags": triggered_tags,
            "primary_count": primary_count,
            "compare_count": compare_count,
            "window_secs": self.window_secs,
            "absolute_threshold": self.absolute_threshold,
            "primary_table": self.primary_table,
            "compare_table": self.compare_table,
        }
        try:
            redis_client.set(self.redis_key, json.dumps(blob))
        except Exception as e:  # noqa: BLE001
            log.error("redis_set_failed", error=str(e), query_ms=query_ms)
            return False

        # entry 알림: False → True 전환 (sticky 안에선 last_dangerous=True 라 dedup)
        if dangerous and not self.last_dangerous:
            log.warning(
                "dangerous_entry",
                tags=triggered_tags,
                primary_count=primary_count,
                compare_count=compare_count,
                window_secs=self.window_secs,
                sticky_until_ms=self.sticky_until_ms,
                query_ms=query_ms,
            )
            if self.notifier and self.notifier.enabled:
                sticky_until_human = time.strftime(
                    "%H:%M:%S", time.gmtime(self.sticky_until_ms / 1000)
                )
                tag_str = ", ".join(triggered_tags) if triggered_tags else "—"
                self.notifier.send(
                    f"🔴 <b>market dangerous</b>\n"
                    f"tags: <code>{tag_str}</code>\n"
                    f"{self.primary_table}: <code>{primary_count:,}</code> / {self.window_secs}s\n"
                    f"{self.compare_table}: <code>{compare_count:,}</code> / {self.window_secs}s\n"
                    f"abs threshold: <code>{self.absolute_threshold:,}</code>\n"
                    f"sticky until: <code>{sticky_until_human}Z</code>"
                )
        elif not dangerous and self.last_dangerous:
            log.info(
                "dangerous_exit",
                primary_count=primary_count,
                compare_count=compare_count,
                query_ms=query_ms,
            )
        else:
            log.info(
                "checked",
                dangerous=dangerous,
                primary_count=primary_count,
                compare_count=compare_count,
                query_ms=query_ms,
            )

        self.last_dangerous = dangerous
        self.last_tags = triggered_tags
        return True

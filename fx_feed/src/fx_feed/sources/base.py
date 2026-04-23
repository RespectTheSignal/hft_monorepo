"""FxSource Protocol — 모든 소스 구현체가 따라야 하는 인터페이스."""

from __future__ import annotations

from typing import Protocol

from fx_feed.questdb_sink import QuestDbSink


class FxSource(Protocol):
    """한 소스를 한 asyncio task로 돌린다.

    구현체는 내부에서 재연결/백오프를 알아서 하고,
    FxTick을 만들어 sink.put_nowait(tick) 으로 밀어넣는다.
    """

    name: str

    async def run(self, sink: QuestDbSink) -> None: ...

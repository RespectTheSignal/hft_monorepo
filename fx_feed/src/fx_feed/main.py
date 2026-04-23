"""fx_feed 엔트리포인트.

사용:
  PYTHONPATH=src python -m fx_feed.main --source binance config.toml
"""

from __future__ import annotations

import argparse
import asyncio
import signal
import sys
from pathlib import Path
from typing import Any

import structlog

try:
    import tomllib  # py311+
except ModuleNotFoundError:
    import tomli as tomllib  # type: ignore[no-redef]

from fx_feed.questdb_sink import QuestDbSink
from fx_feed.sources.base import FxSource
from fx_feed.symbol_map import SymbolMap

logger = structlog.get_logger(__name__)


def _load_config(path: Path) -> dict[str, Any]:
    with path.open("rb") as f:
        return tomllib.load(f)


def _build_source(source: str, cfg: dict[str, Any]) -> FxSource:
    sym_cfg = cfg.get("symbols", {}).get(source, {})
    smap = SymbolMap(sym_cfg)

    if source == "binance":
        from fx_feed.sources.binance import BinanceSource

        b = cfg["binance"]
        return BinanceSource(
            symbols=b["symbols"],
            symbol_map=smap,
            ws_url=b.get("ws_url", "wss://stream.binance.com:9443/stream"),
        )

    if source == "dxfeed":
        from fx_feed.sources.dxfeed import DxFeedSource

        d = cfg["dxfeed"]
        return DxFeedSource(
            symbols=d["symbols"],
            symbol_map=smap,
            ws_url=d["ws_url"],
        )

    if source == "databento":
        from fx_feed.sources.databento import DatabentoSource

        d = cfg["databento"]
        return DatabentoSource(
            symbols=d["symbols"],
            symbol_map=smap,
            dataset=d["dataset"],
            schema=d.get("schema", "mbp-1"),
        )

    raise ValueError(f"unknown source: {source}")


async def _run(source: str, cfg_path: Path) -> None:
    cfg = _load_config(cfg_path)
    qdb = cfg["questdb"]
    sink = QuestDbSink(
        conf=qdb["endpoint"],
        table=qdb.get("table", "fx_bookticker"),
        flush_interval_ms=int(qdb.get("flush_interval_ms", 200)),
    )
    await sink.start()

    src = _build_source(source, cfg)
    logger.info("fx_feed_starting", source=source)

    sink_task = asyncio.create_task(sink.run(), name="sink")
    src_task = asyncio.create_task(src.run(sink), name=f"src-{source}")

    stop = asyncio.Event()

    def _on_signal() -> None:
        logger.info("signal_received")
        stop.set()

    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, _on_signal)
        except NotImplementedError:
            pass

    # 둘 중 하나라도 죽거나 시그널 받으면 전체 종료
    done, pending = await asyncio.wait(
        [sink_task, src_task, asyncio.create_task(stop.wait())],
        return_when=asyncio.FIRST_COMPLETED,
    )

    await sink.stop()
    for t in pending:
        t.cancel()
    for t in done | pending:
        try:
            await t
        except (asyncio.CancelledError, Exception):  # noqa: BLE001
            pass
    logger.info("fx_feed_stopped")


def main() -> None:
    # .env 자동 로드 (Rust의 dotenvy와 동일 관습)
    try:
        from dotenv import load_dotenv  # type: ignore[import-not-found]

        load_dotenv()
    except ImportError:
        pass

    parser = argparse.ArgumentParser(description="fx_feed — FX bookticker → QuestDB")
    parser.add_argument(
        "--source",
        required=True,
        choices=("binance", "dxfeed", "databento"),
        help="데이터 소스 선택",
    )
    parser.add_argument("config", type=Path, help="config.toml 경로")
    args = parser.parse_args()

    if not args.config.exists():
        print(f"config file not found: {args.config}", file=sys.stderr)
        sys.exit(2)

    structlog.configure(
        processors=[
            structlog.processors.add_log_level,
            structlog.processors.TimeStamper(fmt="iso"),
            structlog.dev.ConsoleRenderer(),
        ]
    )

    try:
        import uvloop  # type: ignore[import-not-found]

        uvloop.install()
    except ImportError:
        pass

    asyncio.run(_run(args.source, args.config))


if __name__ == "__main__":
    main()

"""진입점 — 모든 모듈 조립 및 이벤트 루프 실행"""

from __future__ import annotations

import asyncio
import signal
import sys
from pathlib import Path
from typing import Any

import structlog

from strategy_flipster.config import AppConfig, load_config
from strategy_flipster.execution.order_manager import OrderManager
from strategy_flipster.execution.rest_client import FlipsterExecutionClient
from strategy_flipster.market_data.aggregator import MarketDataAggregator
from strategy_flipster.market_data.flipster_ipc import FlipsterIpcFeed
from strategy_flipster.market_data.flipster_zmq import FlipsterZmqFeed
from strategy_flipster.market_data.history import SnapshotHistory, SnapshotSampler
from strategy_flipster.market_data.latest_cache import LatestTickerCache
from strategy_flipster.market_data.stats_cache import MarketStatsCache
from strategy_flipster.market_data.stats_poller import FlipsterMarketStatsPoller
from strategy_flipster.market_data.zmq_feed import ExchangeZmqFeed
from strategy_flipster.strategy.sample import SampleStrategy
from strategy_flipster.user_data.rest_client import FlipsterUserRestClient
from strategy_flipster.user_data.state import UserState
from strategy_flipster.user_data.ws_client import FlipsterUserWsClient
from strategy_flipster.types import BookTicker

logger = structlog.get_logger(__name__)


def _default_accept(ticker: BookTicker) -> bool:
    """USDT-margin perpetual만 수용.

    - Flipster: symbol이 'USDT.PERP'로 끝남 (예: BTCUSDT.PERP)
    - Binance : symbol이 '_USDT'로 끝남 (Binance publisher의 USDT perp 포맷)
    - 기타 거래소: 통과 (필요 시 후속 확장)
    """
    sym = ticker.symbol
    exch = ticker.exchange
    if exch == "flipster":
        return sym.endswith("USDT.PERP")
    if exch == "binance":
        return sym.endswith("_USDT")
    return True


def _build_feeds(config: AppConfig) -> list[Any]:
    """설정에서 피드 목록 생성"""
    feeds: list[Any] = []

    # Flipster 피드 (ZMQ 또는 IPC)
    if config.flipster_feed.mode == "zmq":
        feeds.append(FlipsterZmqFeed(
            zmq_address=config.flipster_feed.zmq_address,
            topics=config.flipster_feed.symbols,
        ))
        logger.info(
            "feed_added",
            exchange="flipster",
            mode="zmq",
            address=config.flipster_feed.zmq_address,
        )
    else:
        feeds.append(FlipsterIpcFeed(
            socket_path=config.flipster_feed.ipc_socket_path,
            process_id=config.flipster_feed.process_id,
            symbols=config.flipster_feed.symbols,
        ))
        logger.info(
            "feed_added",
            exchange="flipster",
            mode="ipc",
            path=config.flipster_feed.ipc_socket_path,
        )

    # 외부 거래소 피드 (동일 wire format)
    for feed_config in config.exchange_feeds:
        if not feed_config.enabled:
            continue
        feeds.append(ExchangeZmqFeed(
            zmq_address=feed_config.zmq_address,
            exchange_name=feed_config.exchange,
            topics=feed_config.topics,
        ))
        logger.info(
            "feed_added",
            exchange=feed_config.exchange,
            mode="zmq",
            address=feed_config.zmq_address,
        )

    return feeds


async def run(config: AppConfig) -> None:
    """메인 이벤트 루프"""
    shutdown_event = asyncio.Event()

    # ── Graceful shutdown 핸들러 ──
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, shutdown_event.set)

    # ── 모듈 생성 ──
    feeds = _build_feeds(config)
    latest_cache = LatestTickerCache()
    aggregator = MarketDataAggregator(
        feeds,
        latest_cache=latest_cache,
        accept=_default_accept,
    )

    # Snapshot History + Sampler
    history = SnapshotHistory(
        interval_sec=config.snapshot_history.interval_ms / 1000.0,
        max_age_sec=config.snapshot_history.max_age_sec,
    )
    snapshot_sampler = SnapshotSampler(
        cache=latest_cache,
        history=history,
        interval_sec=config.snapshot_history.interval_ms / 1000.0,
    )

    # User Data
    user_state = UserState()
    user_rest = FlipsterUserRestClient(config.flipster_api)

    async def _load_rest_snapshot() -> None:
        """초기 로딩 + WS 재연결 후 재동기화 공용 루틴"""
        account = await user_rest.get_account()
        user_state.update_account(account)

        positions = await user_rest.get_positions()
        # 전체 교체를 위해 기존 포지션 키셋 비교
        new_symbols = {p.symbol for p in positions}
        for stale in list(user_state.positions.keys()):
            if stale not in new_symbols:
                user_state.remove_position(stale)
        for pos in positions:
            user_state.update_position(pos)

        balances = await user_rest.get_balances()
        for bal in balances:
            user_state.update_balance(bal)
        logger.info(
            "rest_snapshot_loaded",
            positions=len(positions),
            balances=len(balances),
        )

    user_ws = FlipsterUserWsClient(
        config.flipster_api,
        user_state,
        on_reconnect=_load_rest_snapshot,
    )

    # Execution
    exec_client = FlipsterExecutionClient(config.flipster_api)
    order_manager = OrderManager(exec_client, user_state, dry_run=False)

    # Market Stats (주기 풀링 캐시)
    market_stats = MarketStatsCache()
    stats_poller = FlipsterMarketStatsPoller(
        client=exec_client,
        cache=market_stats,
        interval_sec=config.market_stats.interval_ms / 1000.0,
    )

    # Strategy
    strategy = SampleStrategy()

    # ── 초기화 ──
    logger.info("initializing")

    await user_rest.start()
    await exec_client.start()

    # REST로 초기 상태 로딩
    try:
        await _load_rest_snapshot()
    except Exception:
        logger.exception("initial_state_load_failed")

    # ── 태스크 시작 ──
    logger.info("starting_tasks", feed_count=len(feeds))

    await aggregator.start()
    await strategy.on_start(user_state, latest_cache, market_stats, history)

    # 초기 stats 1회 동기 로딩 (전략이 빈 캐시로 시작하지 않도록)
    if config.market_stats.enabled:
        try:
            initial_count = await stats_poller.poll_once()
            logger.info("market_stats_initial_loaded", count=initial_count)
        except Exception:
            logger.exception("market_stats_initial_load_failed")

    # Task 1: Market data → Strategy
    async def market_data_loop() -> None:
        while not shutdown_event.is_set():
            try:
                ticker = await asyncio.wait_for(
                    aggregator.recv(),
                    timeout=config.strategy.tick_interval_ms / 1000.0,
                )
                orders = await strategy.on_book_ticker(
                    ticker, user_state, latest_cache, market_stats, history,
                )
                if orders:
                    await order_manager.submit_orders(orders)
            except asyncio.TimeoutError:
                orders = await strategy.on_timer(
                    user_state, latest_cache, market_stats, history,
                )
                if orders:
                    await order_manager.submit_orders(orders)
            except asyncio.CancelledError:
                break
            except Exception:
                logger.exception("market_data_loop_error")

    # Task 2: User data WS
    async def user_data_loop() -> None:
        try:
            await user_ws.start()
        except asyncio.CancelledError:
            await user_ws.stop()

    # Task 3: Market stats poller
    async def stats_poll_loop() -> None:
        try:
            await stats_poller.start()
        except asyncio.CancelledError:
            await stats_poller.stop()

    # Task 4: Snapshot history sampler
    async def snapshot_sample_loop() -> None:
        try:
            await snapshot_sampler.start()
        except asyncio.CancelledError:
            await snapshot_sampler.stop()

    # Task 5: Shutdown 대기
    async def shutdown_watcher() -> None:
        await shutdown_event.wait()
        logger.info("shutdown_signal_received")

    tasks: list[asyncio.Task[None]] = [
        asyncio.create_task(market_data_loop(), name="market_data"),
        asyncio.create_task(user_data_loop(), name="user_data"),
        asyncio.create_task(shutdown_watcher(), name="shutdown"),
    ]
    if config.market_stats.enabled:
        tasks.append(asyncio.create_task(stats_poll_loop(), name="stats_poller"))
    if config.snapshot_history.enabled:
        tasks.append(asyncio.create_task(snapshot_sample_loop(), name="snapshot_sampler"))

    # shutdown_watcher가 완료되면 나머지 태스크 취소
    done, pending = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
    for task in pending:
        task.cancel()
    await asyncio.gather(*pending, return_exceptions=True)

    # ── 정리 ──
    logger.info("shutting_down")
    await strategy.on_stop()
    await aggregator.stop()
    await user_ws.stop()
    await stats_poller.stop()
    await snapshot_sampler.stop()
    await exec_client.stop()
    await user_rest.stop()

    logger.info(
        "shutdown_complete",
        orders_submitted=order_manager.total_submitted,
        order_errors=order_manager.total_errors,
    )


def main() -> None:
    # uvloop 설치 시 사용
    try:
        import uvloop
        uvloop.install()
        print("uvloop 활성화")
    except ImportError:
        pass

    # structlog 설정
    structlog.configure(
        processors=[
            structlog.processors.TimeStamper(fmt="iso"),
            structlog.processors.add_log_level,
            structlog.dev.ConsoleRenderer(),
        ],
    )

    # 설정 로딩
    config_path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("config.toml")
    config = load_config(config_path)

    logger.info("config_loaded", config_path=str(config_path))

    asyncio.run(run(config))


if __name__ == "__main__":
    main()

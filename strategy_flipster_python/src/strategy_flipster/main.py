"""진입점 — 모든 모듈 조립 및 이벤트 루프 실행"""

from __future__ import annotations

import asyncio
import signal
import sys
from pathlib import Path

import structlog

from strategy_flipster.config import AppConfig, load_config
from strategy_flipster.execution.order_manager import OrderManager
from strategy_flipster.execution.rest_client import FlipsterExecutionClient
from strategy_flipster.market_data.aggregator import MarketDataAggregator
from strategy_flipster.market_data.binance_zmq import BinanceZmqFeed
from strategy_flipster.market_data.flipster_ipc import FlipsterIpcFeed
from strategy_flipster.strategy.sample import SampleStrategy
from strategy_flipster.user_data.rest_client import FlipsterUserRestClient
from strategy_flipster.user_data.state import UserState
from strategy_flipster.user_data.ws_client import FlipsterUserWsClient

logger = structlog.get_logger(__name__)


async def run(config: AppConfig) -> None:
    """메인 이벤트 루프"""
    shutdown_event = asyncio.Event()

    # ── Graceful shutdown 핸들러 ──
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, shutdown_event.set)

    # ── 모듈 생성 ──

    # Market Data 피드
    feeds: list = []

    binance_feed = BinanceZmqFeed(
        zmq_address=config.binance_feed.zmq_address,
        topics=config.binance_feed.topics,
    )
    feeds.append(binance_feed)

    flipster_feed = FlipsterIpcFeed(
        socket_path=config.flipster_feed.ipc_socket_path,
        process_id=config.flipster_feed.process_id,
        symbols=config.flipster_feed.symbols,
    )
    feeds.append(flipster_feed)

    aggregator = MarketDataAggregator(feeds)

    # User Data
    user_state = UserState()
    user_rest = FlipsterUserRestClient(config.flipster_api)
    user_ws = FlipsterUserWsClient(config.flipster_api, user_state)

    # Execution
    exec_client = FlipsterExecutionClient(config.flipster_api)
    order_manager = OrderManager(exec_client, user_state, dry_run=False)

    # Strategy
    strategy = SampleStrategy()

    # ── 초기화 ──
    logger.info("initializing")

    await user_rest.start()
    await exec_client.start()

    # REST로 초기 상태 로딩
    try:
        account = await user_rest.get_account()
        user_state.update_account(account)
        logger.info("account_loaded", available=str(account.available_balance))

        positions = await user_rest.get_positions()
        for pos in positions:
            user_state.update_position(pos)
        logger.info("positions_loaded", count=len(positions))

        balances = await user_rest.get_balances()
        for bal in balances:
            user_state.update_balance(bal)
        logger.info("balances_loaded", count=len(balances))
    except Exception:
        logger.exception("initial_state_load_failed")

    # ── 태스크 시작 ──
    logger.info("starting_tasks")

    await aggregator.start()
    await strategy.on_start(user_state)

    # Task 1: Market data → Strategy
    async def market_data_loop() -> None:
        while not shutdown_event.is_set():
            try:
                ticker = await asyncio.wait_for(
                    aggregator.recv(),
                    timeout=config.strategy.tick_interval_ms / 1000.0,
                )
                orders = await strategy.on_book_ticker(ticker, user_state)
                if orders:
                    await order_manager.submit_orders(orders)
            except asyncio.TimeoutError:
                # 타이머 트리거 — bookticker 없어도 주기적 실행
                orders = await strategy.on_timer(user_state)
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

    # Task 3: Shutdown 대기
    async def shutdown_watcher() -> None:
        await shutdown_event.wait()
        logger.info("shutdown_signal_received")

    tasks = [
        asyncio.create_task(market_data_loop(), name="market_data"),
        asyncio.create_task(user_data_loop(), name="user_data"),
        asyncio.create_task(shutdown_watcher(), name="shutdown"),
    ]

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

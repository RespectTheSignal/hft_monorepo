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
from strategy_flipster.backtest.pnl_tracker import PnlTracker
from strategy_flipster.market_data.zmq_feed import ExchangeZmqFeed
from strategy_flipster.strategy.basis_meanrev import (
    BasisMeanRevParams,
    BasisMeanRevStrategy,
)
from strategy_flipster.strategy.sample import SampleStrategy
from strategy_flipster.user_data.rest_client import FlipsterUserRestClient
from strategy_flipster.user_data.state import UserState
from strategy_flipster.market_data.symbol import is_supported_symbol
from strategy_flipster.types import BookTicker
from strategy_flipster.user_data.ws_client import FlipsterUserWsClient

import os

logger = structlog.get_logger(__name__)


def _default_accept(ticker: BookTicker) -> bool:
    """USDT-margin perpetual만 수용 (symbol 헬퍼로 판정)"""
    return is_supported_symbol(ticker.exchange, ticker.symbol)


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
    dry_run = os.environ.get("DRY_RUN", "1") != "0"
    exec_client = FlipsterExecutionClient(config.flipster_api)

    # dry_run 모드에서는 가상 체결 + PnL 추적
    dry_run_pnl: PnlTracker | None = None
    if dry_run:
        dry_run_pnl = PnlTracker()
        logger.warning("DRY_RUN_MODE_ENABLED — 가상 체결, 실제 주문 안 나감")

    order_manager = OrderManager(
        exec_client, user_state,
        dry_run=dry_run,
        dry_run_latest=latest_cache if dry_run else None,
        dry_run_pnl=dry_run_pnl,
        dry_run_fee_bps=float(os.environ.get("FEE_BPS", "0.45")),
    )

    # Market Stats (주기 풀링 캐시)
    market_stats = MarketStatsCache()
    stats_poller = FlipsterMarketStatsPoller(
        client=exec_client,
        cache=market_stats,
        interval_sec=config.market_stats.interval_ms / 1000.0,
    )

    # Strategy — 환경변수 STRATEGY=basis_meanrev 면 실전략, 아니면 sample
    strategy_name = os.environ.get("STRATEGY", "sample").lower()
    strategy: Any
    if strategy_name == "basis_meanrev":
        canonicals_str = os.environ.get("CANONICALS", "EPIC,ETH")
        canonicals = tuple(
            s.strip().upper() for s in canonicals_str.split(",") if s.strip()
        )
        params = BasisMeanRevParams(
            canonicals=canonicals,
            window_sec=float(os.environ.get("WINDOW_SEC", "30")),
            open_k=float(os.environ.get("OPEN_K", "2.0")),
            close_k=float(os.environ.get("CLOSE_K", "0.5")),
            max_position_size=float(os.environ.get("MAX_POSITION", "10")),
            open_order_size=float(os.environ.get("OPEN_ORDER_SIZE", "10")),
            close_order_size=float(os.environ.get("CLOSE_ORDER_SIZE", "10")),
            portfolio_max_size=float(os.environ.get("PORTFOLIO_MAX", "50")),
            min_open_dev_bps=float(os.environ.get("MIN_OPEN_DEV_BPS", "3.0")),
            min_open_std_bps=float(os.environ.get("MIN_OPEN_STD_BPS", "0.5")),
            min_close_dev_bps=float(os.environ.get("MIN_CLOSE_DEV_BPS", "1.0")),
            min_close_std_bps=float(os.environ.get("MIN_CLOSE_STD_BPS", "0")),
            spread_aware_filter=os.environ.get("SPREAD_FILTER", "1") != "0",
            beta_fl_assumption=float(os.environ.get("BETA_FL", "0.5")),
            fee_bps_cost=float(os.environ.get("FEE_BPS", "0.45")),
            spread_edge_safety=float(os.environ.get("SPREAD_EDGE_SAFETY", "1.0")),
            binance_open_cooldown_ms=int(os.environ.get("BN_OPEN_COOLDOWN_MS", os.environ.get("BN_COOLDOWN_MS", "200"))),
            binance_close_cooldown_ms=int(os.environ.get("BN_CLOSE_COOLDOWN_MS", "0")),
            cooldown_ms=int(os.environ.get("COOLDOWN_MS", "500")),
        )
        strategy = BasisMeanRevStrategy(params)
        logger.info(
            "using_basis_meanrev",
            canonicals=list(canonicals),
            max_position=params.max_position_size,
            portfolio_max=params.portfolio_max_size,
            dry_run=dry_run,
        )
    else:
        strategy = SampleStrategy()
        logger.info("using_sample_strategy")

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

    # Task 5: Dry-run 주기 stats (30초마다)
    async def dry_run_stats_loop() -> None:
        if dry_run_pnl is None:
            return
        try:
            while not shutdown_event.is_set():
                await asyncio.sleep(30.0)
                p = dry_run_pnl.stats
                net = p.total_realized - p.total_fees
                logger.info(
                    "dry_run_stats",
                    trades=p.total_trades,
                    wins=p.wins,
                    losses=p.losses,
                    realized=round(p.total_realized, 4),
                    fees=round(p.total_fees, 4),
                    net=round(net, 4),
                    volume=round(p.total_volume_usd, 2),
                    positions=len(user_state.positions),
                )
        except asyncio.CancelledError:
            pass

    # Task 6: Shutdown 대기
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
    if dry_run_pnl is not None:
        tasks.append(asyncio.create_task(dry_run_stats_loop(), name="dry_run_stats"))

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

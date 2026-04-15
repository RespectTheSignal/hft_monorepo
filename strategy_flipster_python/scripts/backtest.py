"""백테스트 CLI — basis_meanrev 전략을 QuestDB 데이터로 리플레이.

실행:
    uv run python scripts/backtest.py <canonical_csv> <start_iso> <end_iso> [options]

예:
    # EPIC 1개, 2시간 구간
    uv run python scripts/backtest.py EPIC 2026-04-14T00:00:00Z 2026-04-14T02:00:00Z

    # EPIC + ETH 동시
    uv run python scripts/backtest.py EPIC,ETH 2026-04-14T00:00:00Z 2026-04-14T02:00:00Z

옵션 (환경변수):
    K_IN=2.0 K_OUT=0.5 K_STOP=4.0 WINDOW_SEC=30 NOTIONAL=20 TIMEOUT_SEC=10 FEE_BPS=0.45
"""

from __future__ import annotations

import asyncio
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent / "src"))

import structlog

from strategy_flipster.backtest.fill_simulator import FillSimulator
from strategy_flipster.backtest.pnl_tracker import PnlTracker
from strategy_flipster.backtest.questdb_feed import (
    QdbConfig,
    make_binance_feed,
    make_flipster_feed,
)
from strategy_flipster.backtest.runner import BacktestConfig, BacktestRunner
from strategy_flipster.clock import SimClock
from strategy_flipster.market_data.history import SnapshotHistory
from strategy_flipster.market_data.latest_cache import LatestTickerCache
from strategy_flipster.market_data.stats_cache import MarketStatsCache
from strategy_flipster.market_data.symbol import (
    EXCHANGE_BINANCE,
    EXCHANGE_FLIPSTER,
    to_exchange_symbol,
)
from strategy_flipster.strategy.basis_meanrev import (
    BasisMeanRevParams,
    BasisMeanRevStrategy,
)
from strategy_flipster.user_data.state import UserState


def parse_iso_to_ns(s: str) -> int:
    dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return int(dt.timestamp() * 1_000_000_000)


async def main() -> None:
    if len(sys.argv) < 4:
        print(__doc__)
        sys.exit(1)

    canonicals = tuple(s.strip().upper() for s in sys.argv[1].split(",") if s.strip())
    start_ns = parse_iso_to_ns(sys.argv[2])
    end_ns = parse_iso_to_ns(sys.argv[3])

    if end_ns <= start_ns:
        print("end 가 start 이하"); sys.exit(1)

    # 파라미터 (env)
    default_notional = os.environ.get("NOTIONAL", "20")
    max_pos = float(os.environ.get("MAX_POSITION", default_notional))
    open_size = float(os.environ.get("OPEN_ORDER_SIZE", default_notional))
    close_size = float(os.environ.get("CLOSE_ORDER_SIZE", default_notional))
    portfolio_max = float(os.environ.get("PORTFOLIO_MAX", "0"))  # 0 = 무제한
    params = BasisMeanRevParams(
        canonicals=canonicals,
        window_sec=float(os.environ.get("WINDOW_SEC", "30")),
        open_k=float(os.environ.get("OPEN_K", os.environ.get("K_IN", "2.0"))),
        close_k=float(os.environ.get("CLOSE_K", os.environ.get("K_OUT", "0.5"))),
        max_position_size=max_pos,
        open_order_size=open_size,
        close_order_size=close_size,
        portfolio_max_size=portfolio_max,
        min_dev_bps=float(os.environ.get("MIN_DEV_BPS", "3.0")),
        min_std_bps=float(os.environ.get("MIN_STD_BPS", "0.5")),
        cooldown_ms=int(os.environ.get("COOLDOWN_MS", "500")),
    )
    fee_bps = float(os.environ.get("FEE_BPS", "0.45"))

    # 로깅
    structlog.configure(
        processors=[
            structlog.processors.TimeStamper(fmt="iso"),
            structlog.processors.add_log_level,
            structlog.dev.ConsoleRenderer(),
        ],
    )

    # 심볼 매핑
    fl_symbols = [to_exchange_symbol(EXCHANGE_FLIPSTER, c) for c in canonicals]
    bn_symbols = [to_exchange_symbol(EXCHANGE_BINANCE, c) for c in canonicals]

    print(f"backtest:")
    print(f"  canonicals : {canonicals}")
    print(f"  flipster   : {fl_symbols}")
    print(f"  binance    : {bn_symbols}")
    print(f"  range      : {sys.argv[2]} → {sys.argv[3]}")
    print(f"  duration   : {(end_ns - start_ns) / 1e9:.0f}s")
    print(f"  thresholds : open_k={params.open_k} close_k={params.close_k}")
    print(f"  window     : {params.window_sec}s")
    print(f"  sizes      : max_position=${params.max_position_size}")
    print(f"               open_order=${params.open_order_size} close_order=${params.close_order_size}")
    print(f"               portfolio_max=${params.portfolio_max_size} (0=무제한)")
    print(f"  filters    : min_dev={params.min_dev_bps}bp min_std={params.min_std_bps}bp")
    print(f"  cooldown   : {params.cooldown_ms}ms")
    print(f"  fee        : {fee_bps} bp\n")

    # QuestDB feed
    qdb = QdbConfig()
    bn_feed = make_binance_feed(qdb, bn_symbols, start_ns, end_ns)
    fl_feed = make_flipster_feed(qdb, fl_symbols, start_ns, end_ns)

    # 시뮬 컴포넌트
    sim_clock = SimClock(start_ns=start_ns)
    history = SnapshotHistory(
        interval_sec=0.05,
        max_age_sec=max(params.window_sec + 30.0, 60.0),
        clock=sim_clock,
    )
    latest = LatestTickerCache()
    user_state = UserState()
    pnl = PnlTracker()
    fill_sim = FillSimulator(user_state=user_state, pnl=pnl, fee_bps=fee_bps)
    market_stats = MarketStatsCache()
    strategy = BasisMeanRevStrategy(params, clock=sim_clock)

    runner = BacktestRunner(
        config=BacktestConfig(start_ns=start_ns, end_ns=end_ns),
        event_streams=[bn_feed.iter_events(), fl_feed.iter_events()],
        strategy=strategy,
        clock=sim_clock,
        history=history,
        latest=latest,
        user_state=user_state,
        fill_sim=fill_sim,
        pnl=pnl,
        market_stats=market_stats,
    )

    result = await runner.run()

    # 결과 출력
    print(f"\n{'=' * 6} 백테스트 결과 {'=' * 50}")
    print(f"  wall elapsed     : {result.wall_elapsed_sec:.1f}s")
    print(f"  sim duration     : {(result.end_ns - result.start_ns) / 1e9:.0f}s")
    print(f"  events processed : {result.events_processed:,}")
    print(f"  snapshots        : {result.snapshots_taken:,}")
    print(f"\n  orders submitted : {result.orders_submitted}")
    print(f"  orders filled    : {result.orders_filled}")
    print(f"  orders missed    : {result.orders_missed}")
    fill_rate = (result.orders_filled / result.orders_submitted * 100) if result.orders_submitted else 0.0
    print(f"  fill rate        : {fill_rate:.1f}%")
    print(f"\n  trades           : {result.trades}")
    print(f"  wins / losses    : {result.wins} / {result.losses}")
    win_rate = (result.wins / (result.wins + result.losses) * 100) if (result.wins + result.losses) else 0.0
    print(f"  win rate         : {win_rate:.1f}%")
    print(f"  total volume     : ${result.total_volume:,.2f}")
    print(f"\n  total realized   : ${result.total_realized:+.4f}")
    print(f"  total fees       : ${result.total_fees:.4f}")
    print(f"  NET PnL          : ${result.net_pnl:+.4f}")
    print(f"  peak equity      : ${result.peak_equity:+.4f}")
    print(f"  max drawdown     : ${result.max_drawdown:.4f}")

    # strategy 내부 통계
    s = strategy.stats
    print(f"\n  strategy stats:")
    print(f"    signals          : {s.signals_seen}")
    print(f"    intent_changes   : {s.intent_changes}  (L/S/F: {s.longs}/{s.shorts}/{s.flats})")
    print(f"    orders_emitted   : {s.orders_emitted}  (open/close: {s.open_orders}/{s.close_orders})")
    print(f"    builds (same-dir): {s.builds}")
    print(f"    reemits          : {s.reemits}")
    print(f"    hysteresis_hold  : {s.holds_hysteresis}")
    print(f"    skips_cooldown   : {s.skips_cooldown}")
    print(f"    skips_port_cap   : {s.skips_portfolio_cap}")
    print(f"    skips_no_data    : {s.skips_no_data}")


if __name__ == "__main__":
    asyncio.run(main())

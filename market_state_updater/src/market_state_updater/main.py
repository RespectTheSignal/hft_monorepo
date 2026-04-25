"""market_state_updater 엔트리포인트.

사용:
  uv run market-state-updater                                  # all + 직렬
  uv run market-state-updater --windows fast --interval 30     # 빠른 데몬
  uv run market-state-updater --windows slow --interval 600    # 느린 데몬
  uv run market-state-updater --once                           # CI / 검증
"""

from __future__ import annotations

import json
import socket
import sys
import time
from functools import partial

import redis
import structlog
from dotenv import load_dotenv

from market_state_updater.config import AppConfig, load_config
from market_state_updater.jobs import (
    gap,
    gate_web_gap,
    mid_corr,
    price_change,
    price_change_gap_corr,
    spread_pair,
)
from market_state_updater.jobs.common import (
    PRICE_CHANGE_PERIOD,
    WINDOW_PERIOD,
    corr_return_seconds,
    now_ms,
    windows_for_mode,
)
from market_state_updater.notifier import TelegramNotifier
from market_state_updater.scheduler import Schedule, run_cycle

logger = structlog.get_logger(__name__)


def _configure_logging() -> None:
    structlog.configure(
        processors=[
            structlog.processors.add_log_level,
            structlog.processors.TimeStamper(fmt="iso"),
            structlog.dev.ConsoleRenderer(),
        ]
    )


def build_schedules(cfg: AppConfig, redis_client: redis.Redis) -> list[Schedule]:
    """활성 job × 윈도우 × 거래소를 평면화해서 list[Schedule]."""
    gap_windows, pc_windows = windows_for_mode(cfg.window_mode)
    out: list[Schedule] = []

    # 1) gap : base × quote × window
    for base in cfg.gap_bases:
        for quote in cfg.quote_exchanges:
            for w in gap_windows:
                out.append(
                    Schedule(
                        name=f"gap:{base}:{quote}:{w}m",
                        period=WINDOW_PERIOD.get(w, 1),
                        run=partial(
                            gap.run,
                            cfg.questdb_url,
                            redis_client,
                            cfg.market_gap_prefix,
                            base,
                            quote,
                            w,
                        ),
                    )
                )

    # 2) gate_bookticker vs gate_webbookticker spreads
    if cfg.include_spread_pair:
        for w in gap_windows:
            out.append(
                Schedule(
                    name=f"spread_pair:{w}m",
                    period=WINDOW_PERIOD.get(w, 1),
                    run=partial(
                        spread_pair.run,
                        cfg.questdb_url,
                        redis_client,
                        cfg.market_gap_prefix,
                        w,
                    ),
                )
            )

    # 3) gate_bookticker vs gate_webbookticker mid-price gap
    if cfg.include_gate_gate_web_gap:
        for w in gap_windows:
            out.append(
                Schedule(
                    name=f"gate_web_gap:{w}m",
                    period=WINDOW_PERIOD.get(w, 1),
                    run=partial(
                        gate_web_gap.run,
                        cfg.questdb_url,
                        redis_client,
                        cfg.market_gap_prefix,
                        w,
                    ),
                )
            )

    # 4) price change : source × window
    if cfg.include_price_change:
        for src in cfg.price_change_sources:
            for w in pc_windows:
                out.append(
                    Schedule(
                        name=f"price_change:{src}:{w}m",
                        period=PRICE_CHANGE_PERIOD.get(w, 1),
                        run=partial(
                            price_change.run,
                            cfg.questdb_url,
                            redis_client,
                            cfg.price_change_prefix,
                            src,
                            w,
                        ),
                    )
                )

    # 5) mid level corr (gate, gate_web vs quote) : quote × window
    if cfg.include_mid_corr:
        for quote in cfg.mid_corr_quote_exchanges:
            for w in gap_windows:
                out.append(
                    Schedule(
                        name=f"mid_corr:{quote}:{w}m",
                        period=WINDOW_PERIOD.get(w, 1),
                        run=partial(
                            mid_corr.run,
                            cfg.questdb_url,
                            redis_client,
                            cfg.mid_corr_prefix,
                            quote,
                            w,
                            cfg.mid_corr_min_samples,
                        ),
                    )
                )

    # 6) gate_web ↔ quote step-return correlation : quote × window
    if cfg.include_price_change_gap_corr:
        for quote in cfg.corr_quote_exchanges:
            for w in gap_windows:
                ret = corr_return_seconds(w, cfg.corr_return_seconds_overrides)
                out.append(
                    Schedule(
                        name=f"corr:gate_web:{quote}:{w}m:{ret}s",
                        period=WINDOW_PERIOD.get(w, 1),
                        run=partial(
                            price_change_gap_corr.run,
                            cfg.questdb_url,
                            redis_client,
                            cfg.market_gap_prefix,
                            quote,
                            w,
                            ret,
                        ),
                    )
                )

    return out


def _write_heartbeat(
    redis_client: redis.Redis,
    cfg: AppConfig,
    *,
    run_count: int,
    due: int,
    ok: int,
    duration_ms: int,
) -> None:
    """{heartbeat_prefix}:{mode} 키에 cycle 결과 JSON SET. 외부 watchdog 용."""
    blob = {
        "mode": cfg.window_mode,
        "run_count": run_count,
        "last_cycle_ms": now_ms(),
        "due": due,
        "ok": ok,
        "duration_ms": duration_ms,
    }
    key = f"{cfg.heartbeat_prefix}:{cfg.window_mode}"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        logger.error("heartbeat_write_failed", error=str(e), key=key)


def _is_cycle_failure(due: int, ok: int) -> bool:
    """cycle 자체를 '실패' 로 칠지: due > 0 인데 ok == 0 일 때만.
    부분 실패는 여기서 카운트 안 함 (개별 schedule 의 structlog 에 이미 찍힘).
    """
    return due > 0 and ok == 0


def main() -> int:
    load_dotenv()
    _configure_logging()

    cfg = load_config()

    try:
        r = redis.from_url(cfg.redis_url)
        r.ping()
    except Exception as e:  # noqa: BLE001
        logger.error("redis_connection_failed", error=str(e))
        return 1

    notifier = TelegramNotifier(cfg.telegram_bot_token, cfg.telegram_chat_id)
    schedules = build_schedules(cfg, r)
    host = socket.gethostname()

    logger.info(
        "starting",
        mode=cfg.window_mode,
        schedules=len(schedules),
        gap_bases=list(cfg.gap_bases),
        quote_exchanges=list(cfg.quote_exchanges),
        include_spread_pair=cfg.include_spread_pair,
        include_gate_gate_web_gap=cfg.include_gate_gate_web_gap,
        include_price_change=cfg.include_price_change,
        interval_secs=cfg.interval_secs,
        telegram_enabled=notifier.enabled,
        host=host,
    )

    run_count = -1
    consecutive_failures = 0
    alert_sent = False  # streak 당 1회만 알림 → recovery 시 reset
    restart_delay_secs = 1

    while True:
        try:
            run_count += 1
            t0 = time.monotonic()
            result = run_cycle(schedules, run_count)
            duration_ms = int((time.monotonic() - t0) * 1000)

            _write_heartbeat(
                r,
                cfg,
                run_count=run_count,
                due=result.due,
                ok=result.ok,
                duration_ms=duration_ms,
            )

            if result.due > 0:
                logger.info(
                    "cycle_done",
                    run=run_count,
                    ok=result.ok,
                    due=result.due,
                    duration_ms=duration_ms,
                )

            if _is_cycle_failure(result.due, result.ok):
                consecutive_failures += 1
                logger.warning(
                    "cycle_failure",
                    consecutive=consecutive_failures,
                    threshold=cfg.alert_after_consecutive_failures,
                )
                if (
                    not alert_sent
                    and consecutive_failures >= cfg.alert_after_consecutive_failures
                ):
                    text = (
                        f"🔴 <b>market_state_updater[{cfg.window_mode}] 연속 실패</b>\n"
                        f"host: <code>{host}</code>\n"
                        f"consecutive: <code>{consecutive_failures}</code>\n"
                        f"last cycle due={result.due} ok={result.ok}\n"
                        f"run_count: <code>{run_count}</code>"
                    )
                    if notifier.send(text):
                        alert_sent = True
                        logger.info("alert_sent", consecutive=consecutive_failures)
            else:
                if alert_sent and result.due > 0:
                    notifier.send(
                        f"✅ <b>market_state_updater[{cfg.window_mode}] 복구</b>\n"
                        f"host: <code>{host}</code>\n"
                        f"after {consecutive_failures} consecutive failures"
                    )
                    logger.info("recovery_sent", after_failures=consecutive_failures)
                consecutive_failures = 0
                alert_sent = False

            if cfg.once or cfg.interval_secs <= 0:
                return 0
            time.sleep(cfg.interval_secs)
        except KeyboardInterrupt:
            logger.info("interrupted")
            return 0
        except Exception as e:  # noqa: BLE001
            consecutive_failures += 1
            logger.error(
                "cycle_exception",
                error=str(e),
                consecutive=consecutive_failures,
                restart_in_secs=restart_delay_secs,
            )
            if (
                not alert_sent
                and consecutive_failures >= cfg.alert_after_consecutive_failures
            ):
                if notifier.send(
                    f"🔴 <b>market_state_updater[{cfg.window_mode}] 예외 연속</b>\n"
                    f"host: <code>{host}</code>\n"
                    f"consecutive: <code>{consecutive_failures}</code>\n"
                    f"last error: <code>{str(e)[:200]}</code>"
                ):
                    alert_sent = True
            time.sleep(restart_delay_secs)


if __name__ == "__main__":
    sys.exit(main())

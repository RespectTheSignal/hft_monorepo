"""market_state_updater 엔트리포인트.

사용:
  uv run market-state-updater                                  # all + 직렬
  uv run market-state-updater --windows fast --interval 1      # 빠른 데몬 (1s tick)
  uv run market-state-updater --windows slow                   # 느린 데몬
  uv run market-state-updater --once                           # CI / 검증

Scheduler 모델: schedule 마다 절대 cadence_secs (jobs.common.DEFAULT_CADENCE_SECS).
main loop 가 짧게 (interval_secs, default 1s) tick 하면서 due 한 schedule 만 실행.
1m window 는 5s, 60m window 는 300s 마다 — 윈도우 길이에 비례.
"""

from __future__ import annotations

import json
import socket
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from functools import partial

import redis
import structlog
from dotenv import load_dotenv

from market_state_updater.config import AppConfig, load_config
from market_state_updater.jobs import (
    flipster_gap,
    gap,
    gate_web_gap,
    market_dangerous,
    mid_corr,
    price_change,
    price_change_gap_corr,
    return_autocorr,
    spread_pair,
    trade_outcomes,
    variance_ratio,
)
from market_state_updater.jobs.common import (
    cadence_for_window,
    corr_return_seconds,
    now_ms,
    windows_for_mode,
)
from market_state_updater.notifier import TelegramNotifier  # noqa: F401  (TYPE_CHECKING used in build_schedules)
from market_state_updater.scheduler import Schedule, stagger_initial_runs, tick

logger = structlog.get_logger(__name__)


def _configure_logging() -> None:
    structlog.configure(
        processors=[
            structlog.processors.add_log_level,
            structlog.processors.TimeStamper(fmt="iso"),
            structlog.dev.ConsoleRenderer(),
        ]
    )


def build_schedules(
    cfg: AppConfig,
    redis_client: redis.Redis,
    notifier: "TelegramNotifier | None" = None,
) -> list[Schedule]:
    """활성 job × 윈도우 × 거래소를 평면화해서 list[Schedule]."""
    gap_windows, pc_windows = windows_for_mode(cfg.window_mode)
    out: list[Schedule] = []

    def cad(w: int) -> float:
        return cadence_for_window(w, cfg.cadence_overrides)

    # 1) gap : base × quote × window
    for base in cfg.gap_bases:
        for quote in cfg.quote_exchanges:
            for w in gap_windows:
                out.append(
                    Schedule(
                        name=f"gap:{base}:{quote}:{w}m",
                        cadence_secs=cad(w),
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
                    cadence_secs=cad(w),
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
                    cadence_secs=cad(w),
                    run=partial(
                        gate_web_gap.run,
                        cfg.questdb_url,
                        redis_client,
                        cfg.market_gap_prefix,
                        w,
                    ),
                )
            )

    # 3b) flipster vs binance gap (flipster 심볼 정규화 포함)
    if cfg.include_flipster_gap:
        for w in gap_windows:
            out.append(
                Schedule(
                    name=f"flipster_gap:flipster:binance:{w}m",
                    cadence_secs=cad(w),
                    run=partial(
                        flipster_gap.run,
                        cfg.questdb_url,
                        redis_client,
                        cfg.market_gap_prefix,
                        w,
                    ),
                )
            )

    # 3c) gate_sub_account_trades win/loss outcomes : window × lookahead_seconds
    if cfg.include_trade_outcomes:
        for w in gap_windows:
            for la in cfg.trade_outcomes_lookahead_seconds:
                out.append(
                    Schedule(
                        name=f"trade_outcomes:gate:{w}m:{la}s",
                        cadence_secs=cad(w),
                        run=partial(
                            trade_outcomes.run,
                            cfg.questdb_url,
                            redis_client,
                            cfg.market_gap_prefix,
                            w,
                            la,
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
                        cadence_secs=cad(w),
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
                        cadence_secs=cad(w),
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

    # 6) per-exchange 1-lag return autocorr : exchange × window
    if cfg.include_return_autocorr:
        for ex in cfg.return_autocorr_exchanges:
            for w in gap_windows:
                ret = corr_return_seconds(w, cfg.corr_return_seconds_overrides)
                out.append(
                    Schedule(
                        name=f"return_autocorr:{ex}:{w}m:{ret}s",
                        cadence_secs=cad(w),
                        run=partial(
                            return_autocorr.run,
                            cfg.questdb_url,
                            redis_client,
                            cfg.return_autocorr_prefix,
                            ex,
                            w,
                            ret,
                            cfg.return_autocorr_min_samples,
                        ),
                    )
                )

    # 7) per-exchange Variance Ratio : exchange × window (multi-k in one schedule)
    if cfg.include_variance_ratio:
        for ex in cfg.variance_ratio_exchanges:
            for w in gap_windows:
                # base_seconds: variance_ratio 전용 override 우선, 없으면 corr_return_seconds 재사용
                if w in cfg.variance_ratio_base_seconds_overrides:
                    base_secs = cfg.variance_ratio_base_seconds_overrides[w]
                else:
                    base_secs = corr_return_seconds(
                        w, cfg.corr_return_seconds_overrides
                    )
                out.append(
                    Schedule(
                        name=f"variance_ratio:{ex}:{w}m:base{base_secs}s",
                        cadence_secs=cad(w),
                        run=partial(
                            variance_ratio.run,
                            cfg.questdb_url,
                            redis_client,
                            cfg.variance_ratio_prefix,
                            ex,
                            w,
                            base_secs,
                            cfg.variance_ratio_k_values,
                            cfg.variance_ratio_min_samples,
                        ),
                    )
                )

    # 8) market dangerous (전체 마켓 message rate 임계 + sticky)
    if cfg.include_market_dangerous:
        md_job = market_dangerous.MarketDangerousJob(
            primary_table=cfg.market_dangerous_primary_table,
            compare_table=cfg.market_dangerous_compare_table,
            absolute_threshold=cfg.market_dangerous_absolute_threshold,
            window_secs=cfg.market_dangerous_window_secs,
            sticky_secs=cfg.market_dangerous_sticky_secs,
            redis_key=cfg.market_dangerous_redis_key,
            notifier=notifier,
        )
        out.append(
            Schedule(
                name="market_dangerous",
                cadence_secs=cfg.market_dangerous_cadence_secs,
                run=partial(md_job.run, cfg.questdb_url, redis_client),
            )
        )

    # 9) gate_web ↔ quote step-return correlation : quote × window
    if cfg.include_price_change_gap_corr:
        for quote in cfg.corr_quote_exchanges:
            for w in gap_windows:
                ret = corr_return_seconds(w, cfg.corr_return_seconds_overrides)
                out.append(
                    Schedule(
                        name=f"corr:gate_web:{quote}:{w}m:{ret}s",
                        cadence_secs=cad(w),
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
    schedules_total: int,
    last_run_at: dict[str, float],
    last_tick_due: int,
    last_tick_ok: int,
) -> None:
    """{heartbeat_prefix}:{mode} 키에 tick 결과 + schedule 별 last_run JSON SET."""
    blob = {
        "mode": cfg.window_mode,
        "last_tick_ms": now_ms(),
        "schedules_total": schedules_total,
        "last_tick_due": last_tick_due,
        "last_tick_ok": last_tick_ok,
        # last_run_at 은 monotonic 기반이라 wall-clock 으로 직접 변환 불가 — 상대적 staleness 만.
        "schedules_with_last_run": len(last_run_at),
    }
    key = f"{cfg.heartbeat_prefix}:{cfg.window_mode}"
    try:
        redis_client.set(key, json.dumps(blob))
    except Exception as e:  # noqa: BLE001
        logger.error("heartbeat_write_failed", error=str(e), key=key)


def _is_failure(due: int, ok: int) -> bool:
    """due > 0 인데 ok == 0 일 때만 cycle failure 로 간주 (부분 실패는 X)."""
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
    schedules = build_schedules(cfg, r, notifier=notifier)
    host = socket.gethostname()

    cadence_summary = sorted({s.cadence_secs for s in schedules})
    logger.info(
        "starting",
        mode=cfg.window_mode,
        schedules=len(schedules),
        gap_bases=list(cfg.gap_bases),
        quote_exchanges=list(cfg.quote_exchanges),
        cadence_secs_distinct=cadence_summary,
        tick_interval_secs=cfg.interval_secs,
        stagger_step_secs=cfg.stagger_step_secs,
        max_workers=cfg.tick_max_workers,
        telegram_enabled=notifier.enabled,
        host=host,
    )

    last_run_at: dict[str, float] = {}
    if not cfg.once:
        stagger_initial_runs(
            schedules, last_run_at, time.monotonic(), cfg.stagger_step_secs
        )
    consecutive_failures = 0
    alert_sent = False
    restart_delay_secs = 1

    if cfg.once:
        # --once 는 첫 tick 에서 모든 schedule 강제 실행 (cadence 무시)
        forced_now = time.monotonic() + 1e9  # 매우 큰 값 → 모두 due
        with ThreadPoolExecutor(
            max_workers=cfg.tick_max_workers, thread_name_prefix="msu"
        ) as ex:
            result = tick(schedules, forced_now, last_run_at, executor=ex)
        logger.info("once_done", due=result.due, ok=result.ok)
        _write_heartbeat(
            r,
            cfg,
            schedules_total=len(schedules),
            last_run_at=last_run_at,
            last_tick_due=result.due,
            last_tick_ok=result.ok,
        )
        return 0

    executor = ThreadPoolExecutor(
        max_workers=cfg.tick_max_workers, thread_name_prefix="msu"
    )
    while True:
        try:
            t0 = time.monotonic()
            result = tick(schedules, t0, last_run_at, executor=executor)
            duration_ms = int((time.monotonic() - t0) * 1000)

            if result.due > 0:
                logger.info(
                    "tick_done",
                    due=result.due,
                    ok=result.ok,
                    duration_ms=duration_ms,
                )

            _write_heartbeat(
                r,
                cfg,
                schedules_total=len(schedules),
                last_run_at=last_run_at,
                last_tick_due=result.due,
                last_tick_ok=result.ok,
            )

            if _is_failure(result.due, result.ok):
                consecutive_failures += 1
                logger.warning(
                    "tick_failure",
                    consecutive=consecutive_failures,
                    threshold=cfg.alert_after_consecutive_failures,
                )
                if (
                    not alert_sent
                    and consecutive_failures
                    >= cfg.alert_after_consecutive_failures
                ):
                    text = (
                        f"🔴 <b>market_state_updater[{cfg.window_mode}] 연속 실패</b>\n"
                        f"host: <code>{host}</code>\n"
                        f"consecutive: <code>{consecutive_failures}</code>\n"
                        f"last tick due={result.due} ok={result.ok}"
                    )
                    if notifier.send(text):
                        alert_sent = True
                        logger.info("alert_sent", consecutive=consecutive_failures)
            elif result.due > 0:  # 성공 (부분 또는 전체)
                if alert_sent:
                    notifier.send(
                        f"✅ <b>market_state_updater[{cfg.window_mode}] 복구</b>\n"
                        f"host: <code>{host}</code>\n"
                        f"after {consecutive_failures} consecutive failures"
                    )
                    logger.info("recovery_sent", after_failures=consecutive_failures)
                consecutive_failures = 0
                alert_sent = False

            time.sleep(cfg.interval_secs)
        except KeyboardInterrupt:
            logger.info("interrupted")
            executor.shutdown(wait=False, cancel_futures=True)
            return 0
        except Exception as e:  # noqa: BLE001
            consecutive_failures += 1
            logger.error(
                "tick_exception",
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

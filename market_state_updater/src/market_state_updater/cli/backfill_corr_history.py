"""price_change_gap_corr 의 historical backfill.

지난 N 시간 동안 매 cadence 분마다 corr 을 계산해서 새 QuestDB 테이블
(`price_change_gap_corr`, partition by day) 에 row-per-(ts × window) 로 적재.

사용:
  uv run python -m market_state_updater.cli.backfill_corr_history
  # 또는 옵션 명시:
  uv run python -m market_state_updater.cli.backfill_corr_history \\
      --symbol IO_USDT --hours 24 --windows 30,60,240

Cadence 디폴트 (window 별, --cadence-overrides 로 변경):
  30m → 5m, 60m → 10m, 240m → 30m

Idempotency: 그냥 INSERT — 같은 backfill 두 번 실행하면 row 중복.
재실행 전 `DELETE` 또는 `DROP TABLE price_change_gap_corr` 필요 시 직접.
"""

from __future__ import annotations

import argparse
import os
import sys
import time as _time
from datetime import datetime, timedelta, timezone

import structlog
from dotenv import load_dotenv

from market_state_updater.jobs.common import (
    corr_return_seconds,
    parse_corr_return_seconds_overrides,
)
from market_state_updater.jobs.price_change_gap_corr import build_query, parse_dataset
from market_state_updater.questdb import questdb_exec

logger = structlog.get_logger(__name__)

DEFAULT_TABLE = "price_change_gap_corr"
DEFAULT_SYMBOL = "IO_USDT"
DEFAULT_HOURS = 24
DEFAULT_WINDOWS = (30, 60, 240)
DEFAULT_CADENCE_MINUTES: dict[int, int] = {30: 5, 60: 10, 240: 30}
DEFAULT_BASE_EXCHANGE = "gate_web"
DEFAULT_QUOTE_EXCHANGE = "binance"
# QuestDB /exec 의 GET URL 길이 한계 (~7500 chars). 한 row ≈ 90 chars.
# 30 rows × 91 + INSERT prefix ≈ 2900 chars 로 안전 마진.
INSERT_BATCH_SIZE = 30


def _configure_logging() -> None:
    structlog.configure(
        processors=[
            structlog.processors.add_log_level,
            structlog.processors.TimeStamper(fmt="iso"),
            structlog.dev.ConsoleRenderer(),
        ]
    )


def create_table(questdb_url: str, table: str) -> None:
    sql = f"""
CREATE TABLE IF NOT EXISTS {table} (
  ts              TIMESTAMP,
  symbol          SYMBOL,
  base_exchange   SYMBOL,
  quote_exchange  SYMBOL,
  window_minutes  INT,
  return_seconds  INT,
  corr            DOUBLE,
  n_samples       LONG
) timestamp(ts) PARTITION BY DAY;
""".strip()
    questdb_exec(questdb_url, sql)


def _format_ts(dt: datetime) -> str:
    return dt.strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def _build_insert_sql(
    table: str,
    rows: list[tuple[datetime, str, str, str, int, int, float | None, int | None]],
) -> str:
    """multi-row INSERT. corr/n_samples 가 None 이면 NULL 로."""
    values = []
    for ts, sym, base, quote, w, ret, corr, n in rows:
        corr_str = "NULL" if corr is None else f"{corr:.10f}"
        n_str = "NULL" if n is None else str(n)
        values.append(
            f"('{_format_ts(ts)}','{sym}','{base}','{quote}',{w},{ret},{corr_str},{n_str})"
        )
    return (
        f"INSERT INTO {table} "
        f"(ts, symbol, base_exchange, quote_exchange, window_minutes, return_seconds, corr, n_samples) "
        f"VALUES " + ",".join(values) + ";"
    )


def compute_one(
    questdb_url: str,
    *,
    symbol: str,
    end_ts: datetime,
    window_minutes: int,
    return_seconds: int,
    quote_exchange: str,
) -> tuple[float | None, int | None]:
    """한 시점 (end_ts) 의 corr 계산. None 이면 데이터 부족 / numerical drop."""
    sql = build_query(
        quote_exchange, window_minutes, return_seconds, end_ts=end_ts, symbol=symbol
    )
    payload = questdb_exec(questdb_url, sql)
    corrs, counts = parse_dataset(payload)
    if symbol in corrs:
        return corrs[symbol], counts[symbol]
    return None, None


def _parse_csv_int(s: str) -> tuple[int, ...]:
    return tuple(int(x.strip()) for x in s.split(",") if x.strip())


def _parse_cadence_overrides(s: str) -> dict[int, int]:
    out: dict[int, int] = {}
    for part in s.split(","):
        part = part.strip()
        if not part:
            continue
        k, _, v = part.partition(":")
        if not v:
            raise ValueError(f"invalid cadence override token: {part!r}")
        out[int(k.strip())] = int(v.strip())
    return out


def main() -> int:
    load_dotenv()
    _configure_logging()

    parser = argparse.ArgumentParser(
        prog="backfill_corr_history",
        description="price_change_gap_corr 의 historical backfill → QuestDB price_change_gap_corr 테이블.",
    )
    parser.add_argument("--symbol", default=DEFAULT_SYMBOL)
    parser.add_argument("--hours", type=int, default=DEFAULT_HOURS)
    parser.add_argument(
        "--windows",
        default=",".join(str(w) for w in DEFAULT_WINDOWS),
        help="쉼표로 분리된 window 분 (default: 30,60,240).",
    )
    parser.add_argument(
        "--cadence-overrides",
        default="",
        help="window:cadence_minutes,... 형식. 디폴트: 30:5,60:10,240:30",
    )
    parser.add_argument("--base-exchange", default=DEFAULT_BASE_EXCHANGE)
    parser.add_argument("--quote-exchange", default=DEFAULT_QUOTE_EXCHANGE)
    parser.add_argument("--table", default=DEFAULT_TABLE)
    parser.add_argument(
        "--questdb-url",
        default=os.getenv("QUESTDB_URL", "http://localhost:9000"),
    )
    parser.add_argument(
        "--corr-return-seconds-overrides",
        default=os.getenv("CORR_RETURN_SECONDS_OVERRIDES", ""),
        help="window:return_seconds,... — corr X 의 step. 디폴트는 jobs/common.py.",
    )
    parser.add_argument(
        "--reset-table",
        action="store_true",
        help="실행 전 DROP TABLE IF EXISTS + CREATE — 재실행 시 중복 방지.",
    )
    args = parser.parse_args()

    if args.base_exchange != "gate_web":
        logger.error("only_gate_web_supported_as_base", got=args.base_exchange)
        return 1

    windows = _parse_csv_int(args.windows)
    cadence_table = dict(DEFAULT_CADENCE_MINUTES)
    cadence_table.update(_parse_cadence_overrides(args.cadence_overrides))
    corr_overrides = parse_corr_return_seconds_overrides(
        args.corr_return_seconds_overrides
    )

    if args.reset_table:
        logger.info("dropping_table", table=args.table)
        try:
            questdb_exec(args.questdb_url, f"DROP TABLE IF EXISTS {args.table};")
        except Exception as e:  # noqa: BLE001
            logger.error("drop_table_failed", error=str(e))
            return 1

    logger.info("creating_table_if_needed", table=args.table)
    try:
        create_table(args.questdb_url, args.table)
    except Exception as e:  # noqa: BLE001
        logger.error("create_table_failed", error=str(e))
        return 1

    end_now = datetime.now(timezone.utc).replace(microsecond=0)
    start_at = end_now - timedelta(hours=args.hours)
    logger.info(
        "backfill_start",
        symbol=args.symbol,
        windows=list(windows),
        cadence_minutes={w: cadence_table.get(w, 5) for w in windows},
        range=(start_at.isoformat(), end_now.isoformat()),
        table=args.table,
    )

    total_rows = 0
    t0 = _time.monotonic()

    for window in windows:
        cadence = cadence_table.get(window, 5)
        return_sec = corr_return_seconds(window, corr_overrides)
        n_steps = (args.hours * 60) // cadence
        log = logger.bind(
            window=window, cadence_min=cadence, return_sec=return_sec, n_steps=n_steps
        )
        log.info("window_start")

        rows: list[tuple] = []
        n_ok = 0
        n_null = 0
        n_fail = 0
        wt0 = _time.monotonic()

        for i in range(n_steps + 1):  # +1 to include endpoint
            t = start_at + timedelta(minutes=i * cadence)
            try:
                corr_val, n = compute_one(
                    args.questdb_url,
                    symbol=args.symbol,
                    end_ts=t,
                    window_minutes=window,
                    return_seconds=return_sec,
                    quote_exchange=args.quote_exchange,
                )
                if corr_val is None:
                    n_null += 1
                else:
                    n_ok += 1
                rows.append(
                    (
                        t,
                        args.symbol,
                        args.base_exchange,
                        args.quote_exchange,
                        window,
                        return_sec,
                        corr_val,
                        n,
                    )
                )
            except Exception as e:  # noqa: BLE001
                n_fail += 1
                log.warning("compute_failed", t=t.isoformat(), error=str(e))

            if len(rows) >= INSERT_BATCH_SIZE:
                try:
                    questdb_exec(args.questdb_url, _build_insert_sql(args.table, rows))
                    total_rows += len(rows)
                except Exception as e:  # noqa: BLE001
                    log.error("insert_failed", batch=len(rows), error=str(e))
                rows.clear()

        if rows:
            try:
                questdb_exec(args.questdb_url, _build_insert_sql(args.table, rows))
                total_rows += len(rows)
            except Exception as e:  # noqa: BLE001
                log.error("insert_failed", batch=len(rows), error=str(e))

        log.info(
            "window_done",
            n_ok=n_ok,
            n_null=n_null,
            n_failed=n_fail,
            duration_secs=round(_time.monotonic() - wt0, 1),
        )

    logger.info(
        "backfill_done",
        total_rows=total_rows,
        duration_secs=round(_time.monotonic() - t0, 1),
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

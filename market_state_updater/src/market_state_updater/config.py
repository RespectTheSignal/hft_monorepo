"""CLI + env + config.json 을 합쳐 AppConfig 를 만든다.

우선순위 (낮음 → 높음):
  defaults  <  config.json  <  env  <  CLI

config.json 은 커밋되는 팀 공유 설정. .env 는 비밀 (DB URL with auth, telegram token).
일회성/환경별 override 는 env 또는 CLI 로.
"""

from __future__ import annotations

import argparse
import json
import os
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal

from market_state_updater.jobs.common import (
    parse_cadence_overrides,
    parse_corr_return_seconds_overrides,
    parse_sample_interval_overrides,
    set_sample_interval_overrides,
)

DEFAULT_QUESTDB_URL = "http://localhost:9000"
DEFAULT_REDIS_URL = "redis://localhost:6379"
DEFAULT_HEARTBEAT_PREFIX = "gate_hft:_meta:market_state_updater"
DEFAULT_ALERT_AFTER_CONSECUTIVE_FAILURES = 5
# tick poll interval. cycle 자체가 아니라 schedule 별 cadence_secs 가 절대 시각.
# 1s 정도면 cadence 정밀도 충분 (cadence 가 1s 이하인 schedule 거의 없음).
DEFAULT_UPDATE_INTERVAL_SECS = 1

WindowMode = Literal["fast", "slow", "all"]


def _truthy(v: str | None) -> bool:
    return (v or "").strip().lower() not in ("0", "false", "no", "")


def _split_csv(s: str) -> tuple[str, ...]:
    return tuple(x.strip().lower() for x in s.split(",") if x.strip())


@dataclass(frozen=True, slots=True)
class AppConfig:
    questdb_url: str
    redis_url: str
    interval_secs: int   # tick poll interval (default 1s)
    once: bool

    window_mode: WindowMode
    cadence_overrides: dict[int, float]   # window → cadence_secs override
    stagger_step_secs: float              # 첫 tick burst 회피 (0 = off)
    tick_max_workers: int                 # ThreadPoolExecutor max_workers

    market_gap_prefix: str
    base_exchange: str
    quote_exchanges: tuple[str, ...]
    sample_interval_overrides: dict[int, str]

    include_gate_web: bool
    include_spread_pair: bool
    include_gate_gate_web_gap: bool

    include_price_change: bool
    price_change_prefix: str
    price_change_sources: tuple[str, ...]

    include_price_change_gap_corr: bool
    corr_quote_exchanges: tuple[str, ...]
    corr_return_seconds_overrides: dict[int, int]

    include_mid_corr: bool
    mid_corr_prefix: str
    mid_corr_quote_exchanges: tuple[str, ...]
    mid_corr_min_samples: int

    include_return_autocorr: bool
    return_autocorr_prefix: str
    return_autocorr_exchanges: tuple[str, ...]
    return_autocorr_min_samples: int

    include_variance_ratio: bool
    variance_ratio_prefix: str
    variance_ratio_exchanges: tuple[str, ...]
    variance_ratio_k_values: tuple[int, ...]
    variance_ratio_min_samples: int
    # vr 의 r_1 step. 비어있으면 corr_return_seconds_overrides 사용 (재사용).
    variance_ratio_base_seconds_overrides: dict[int, int]

    include_market_dangerous: bool
    market_dangerous_redis_key: str
    market_dangerous_primary_table: str
    market_dangerous_compare_table: str
    market_dangerous_absolute_threshold: int
    market_dangerous_window_secs: int
    market_dangerous_sticky_secs: int
    market_dangerous_cadence_secs: float

    heartbeat_prefix: str

    telegram_bot_token: str | None
    telegram_chat_id: str | None
    alert_after_consecutive_failures: int

    @property
    def gap_bases(self) -> tuple[str, ...]:
        if self.include_gate_web and self.base_exchange != "gate_web":
            return (self.base_exchange, "gate_web")
        return (self.base_exchange,)


# ---------- config.json 파일 로딩 ----------


def _resolve_config_path(explicit: str | None) -> Path | None:
    """--config <path> 우선, 없으면 cwd/config.json, 없으면 패키지 루트/config.json."""
    if explicit:
        p = Path(explicit)
        return p if p.exists() else None
    candidates = [
        Path.cwd() / "config.json",
        Path(__file__).resolve().parent.parent.parent / "config.json",
    ]
    for p in candidates:
        if p.exists():
            return p
    return None


def _load_json_file(path: Path) -> Mapping[str, Any]:
    """config.json 파싱. `$` 로 시작하는 키 ($comment 등) 는 무시."""
    with path.open() as f:
        data = json.load(f)
    if not isinstance(data, dict):
        raise ValueError(f"config file root must be object: {path}")
    return {k: v for k, v in data.items() if not k.startswith("$")}


def _section(file_cfg: Mapping[str, Any], name: str) -> Mapping[str, Any]:
    sec = file_cfg.get(name, {})
    return sec if isinstance(sec, dict) else {}


# ---------- 헬퍼: env > file > default 우선순위 ----------


def _str_from(env_key: str, file_val: Any, default: str) -> str:
    v = os.environ.get(env_key)
    if v is not None:
        return v
    if isinstance(file_val, str):
        return file_val
    return default


def _int_from(env_key: str, file_val: Any, default: int) -> int:
    v = os.environ.get(env_key)
    if v is not None:
        return int(v)
    if isinstance(file_val, int):
        return file_val
    return default


def _bool_from(env_key: str, file_val: Any, default: bool) -> bool:
    if env_key in os.environ:
        return _truthy(os.environ[env_key])
    if isinstance(file_val, bool):
        return file_val
    return default


def _csv_or_list_from(
    env_key: str, file_val: Any, default: tuple[str, ...]
) -> tuple[str, ...]:
    v = os.environ.get(env_key)
    if v is not None:
        return _split_csv(v)
    if isinstance(file_val, list):
        return tuple(str(x).strip().lower() for x in file_val if str(x).strip())
    if isinstance(file_val, str):
        return _split_csv(file_val)
    return default


def _corr_overrides_from(
    env_key: str, file_val: Any, default: dict[int, int]
) -> dict[int, int]:
    v = os.environ.get(env_key)
    if v is not None:
        return parse_corr_return_seconds_overrides(v)
    if isinstance(file_val, dict):
        return {int(k): int(v) for k, v in file_val.items()}
    return default


def _sample_interval_overrides_from(
    env_key: str, file_val: Any, default: dict[int, str]
) -> dict[int, str]:
    v = os.environ.get(env_key)
    if v is not None:
        return parse_sample_interval_overrides(v)
    if isinstance(file_val, dict):
        return {int(k): str(v) for k, v in file_val.items()}
    return default


def _cadence_overrides_from(
    env_key: str, file_val: Any, default: dict[int, float]
) -> dict[int, float]:
    v = os.environ.get(env_key)
    if v is not None:
        return parse_cadence_overrides(v)
    if isinstance(file_val, dict):
        return {int(k): float(v) for k, v in file_val.items()}
    return default


# ---------- argparse ----------


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="market-state-updater",
        description="QuestDB → Redis 시장 상태 업데이터.",
    )
    parser.add_argument(
        "--config",
        default=None,
        help="config.json 경로 (기본: cwd/config.json → 패키지 루트/config.json).",
    )
    parser.add_argument(
        "--once", action="store_true", help="Run once and exit (no loop)."
    )
    # 나머지 flag 들은 default=None — env 와 file 우선순위 처리는 load_config 안에서.
    parser.add_argument("--interval", type=int, default=None)
    parser.add_argument(
        "--windows", choices=("fast", "slow", "all"), default=None,
        help="윈도우 모드. fast=≤5m, slow=>5m, all=전부.",
    )
    parser.add_argument("--questdb-url", default=None)
    parser.add_argument("--redis-url", default=None)
    parser.add_argument("--prefix", default=None, help="market_gap redis key prefix.")
    parser.add_argument("--base-exchange", dest="base_exchange", default=None)
    parser.add_argument(
        "--exchanges", default=None,
        help="quote exchanges, comma-separated. e.g. binance,bybit,bitget",
    )
    return parser


# ---------- main entry ----------


def load_config(argv: list[str] | None = None) -> AppConfig:
    args = build_arg_parser().parse_args(argv)

    cfg_path = _resolve_config_path(args.config)
    file_cfg: Mapping[str, Any] = _load_json_file(cfg_path) if cfg_path else {}

    mg = _section(file_cfg, "market_gap")
    inc = _section(file_cfg, "include")
    pc = _section(file_cfg, "price_change")
    corr = _section(file_cfg, "corr")
    mc = _section(file_cfg, "mid_corr")
    ra = _section(file_cfg, "return_autocorr")
    vr = _section(file_cfg, "variance_ratio")
    md = _section(file_cfg, "market_dangerous")
    hb = _section(file_cfg, "heartbeat")

    questdb_url = (
        args.questdb_url
        or _str_from("QUESTDB_URL", file_cfg.get("questdb_url"), DEFAULT_QUESTDB_URL)
    )
    redis_url = (
        args.redis_url
        or _str_from("REDIS_URL", file_cfg.get("redis_url"), DEFAULT_REDIS_URL)
    )
    interval = (
        args.interval
        if args.interval is not None
        else _int_from(
            "UPDATE_INTERVAL_SECS",
            file_cfg.get("interval_secs"),
            DEFAULT_UPDATE_INTERVAL_SECS,
        )
    )
    interval_secs = 0 if args.once else max(1, interval)

    window_mode_str = args.windows or _str_from(
        "WINDOW_MODE", file_cfg.get("window_mode"), "all"
    )
    if window_mode_str not in ("fast", "slow", "all"):
        raise SystemExit(
            f"invalid window_mode: {window_mode_str!r} (expected fast|slow|all)"
        )
    window_mode: WindowMode = window_mode_str  # type: ignore[assignment]

    market_gap_prefix = (
        args.prefix
        or _str_from(
            "MARKET_GAP_REDIS_PREFIX",
            mg.get("redis_prefix"),
            "gate_hft:market_gap",
        )
    )
    base_exchange = (
        args.base_exchange
        or _str_from("MARKET_GAP_BASE", mg.get("base_exchange"), "gate")
    ).strip().lower()

    if args.exchanges:
        quote_exchanges = _split_csv(args.exchanges)
    else:
        quote_exchanges = _csv_or_list_from(
            "MARKET_GAP_QUOTE_EXCHANGES", mg.get("quote_exchanges"), ("binance",)
        )
    if not quote_exchanges:
        raise SystemExit(
            "At least one quote exchange required (config.json market_gap.quote_exchanges, "
            "MARKET_GAP_QUOTE_EXCHANGES, or --exchanges)"
        )

    # FILL_PREV_LOOKBACK_MINUTES 는 jobs/common.py 가 import 시점에 env 만 봄.
    # 여기서 file 값을 env 로 주입해서 통일 (env override 도 그대로 동작).
    fill_prev_file = mg.get("fill_prev_lookback_minutes")
    if fill_prev_file is not None and "MARKET_GAP_FILL_PREV_LOOKBACK_MINUTES" not in os.environ:
        os.environ["MARKET_GAP_FILL_PREV_LOOKBACK_MINUTES"] = str(int(fill_prev_file))

    sample_interval_overrides = _sample_interval_overrides_from(
        "MARKET_GAP_SAMPLE_INTERVAL_OVERRIDES",
        mg.get("sample_interval_overrides"),
        {},
    )
    # process-wide setter 즉시 호출 — gap/spread_pair/gate_web_gap 가 자동 반영.
    set_sample_interval_overrides(sample_interval_overrides)

    sched_section = _section(file_cfg, "scheduler")
    cadence_overrides = _cadence_overrides_from(
        "CADENCE_OVERRIDES_SECS", sched_section.get("cadence_overrides"), {}
    )
    stagger_step_secs = float(
        os.environ.get(
            "STAGGER_STEP_SECS",
            sched_section.get("stagger_step_secs", 0.5),
        )
    )
    tick_max_workers = int(
        os.environ.get(
            "TICK_MAX_WORKERS",
            sched_section.get("max_workers", 8),
        )
    )

    return AppConfig(
        questdb_url=questdb_url,
        redis_url=redis_url,
        interval_secs=interval_secs,
        once=args.once,
        window_mode=window_mode,
        cadence_overrides=cadence_overrides,
        stagger_step_secs=stagger_step_secs,
        tick_max_workers=tick_max_workers,
        market_gap_prefix=market_gap_prefix,
        base_exchange=base_exchange,
        quote_exchanges=quote_exchanges,
        sample_interval_overrides=sample_interval_overrides,
        include_gate_web=_bool_from(
            "MARKET_GAP_INCLUDE_GATE_WEB", inc.get("gate_web"), True
        ),
        include_spread_pair=_bool_from(
            "MARKET_GAP_INCLUDE_GATE_SPREAD_PAIR", inc.get("spread_pair"), True
        ),
        include_gate_gate_web_gap=_bool_from(
            "MARKET_GAP_INCLUDE_GATE_GATE_WEB_GAP", inc.get("gate_gate_web_gap"), True
        ),
        include_price_change=_bool_from(
            "MARKET_GAP_INCLUDE_PRICE_CHANGE", inc.get("price_change"), True
        ),
        price_change_prefix=_str_from(
            "PRICE_CHANGE_REDIS_PREFIX",
            pc.get("redis_prefix"),
            "gate_hft:price_change",
        ),
        price_change_sources=_csv_or_list_from(
            "PRICE_CHANGE_SOURCES", pc.get("sources"), ("gate", "binance")
        ),
        include_price_change_gap_corr=_bool_from(
            "MARKET_GAP_INCLUDE_PRICE_CHANGE_GAP_CORR",
            inc.get("price_change_gap_corr"),
            True,
        ),
        corr_quote_exchanges=_csv_or_list_from(
            "CORR_QUOTE_EXCHANGES", corr.get("quote_exchanges"), ("binance",)
        ),
        corr_return_seconds_overrides=_corr_overrides_from(
            "CORR_RETURN_SECONDS_OVERRIDES",
            corr.get("return_seconds_overrides"),
            {},
        ),
        include_mid_corr=_bool_from(
            "MARKET_GAP_INCLUDE_MID_CORR", inc.get("mid_corr"), True
        ),
        mid_corr_prefix=_str_from(
            "MID_CORR_REDIS_PREFIX",
            mc.get("redis_prefix"),
            "gate_hft:market_mid_corr",
        ),
        mid_corr_quote_exchanges=_csv_or_list_from(
            "MID_CORR_QUOTE_EXCHANGES", mc.get("quote_exchanges"), ("binance",)
        ),
        mid_corr_min_samples=_int_from(
            "MID_CORR_MIN_SAMPLES", mc.get("min_samples"), 30
        ),
        include_return_autocorr=_bool_from(
            "MARKET_GAP_INCLUDE_RETURN_AUTOCORR", inc.get("return_autocorr"), True
        ),
        return_autocorr_prefix=_str_from(
            "RETURN_AUTOCORR_REDIS_PREFIX",
            ra.get("redis_prefix"),
            "gate_hft:return_autocorr",
        ),
        return_autocorr_exchanges=_csv_or_list_from(
            "RETURN_AUTOCORR_EXCHANGES",
            ra.get("exchanges"),
            ("gate", "gate_web", "binance"),
        ),
        return_autocorr_min_samples=_int_from(
            "RETURN_AUTOCORR_MIN_SAMPLES", ra.get("min_samples"), 30
        ),
        include_variance_ratio=_bool_from(
            "MARKET_GAP_INCLUDE_VARIANCE_RATIO", inc.get("variance_ratio"), True
        ),
        variance_ratio_prefix=_str_from(
            "VARIANCE_RATIO_REDIS_PREFIX",
            vr.get("redis_prefix"),
            "gate_hft:variance_ratio",
        ),
        variance_ratio_exchanges=_csv_or_list_from(
            "VARIANCE_RATIO_EXCHANGES",
            vr.get("exchanges"),
            ("gate", "gate_web", "binance"),
        ),
        variance_ratio_k_values=tuple(
            int(x) for x in (vr.get("k_values") or [2, 5, 10])
        ),
        variance_ratio_min_samples=_int_from(
            "VARIANCE_RATIO_MIN_SAMPLES", vr.get("min_samples"), 30
        ),
        variance_ratio_base_seconds_overrides=_corr_overrides_from(
            "VARIANCE_RATIO_BASE_SECONDS_OVERRIDES",
            vr.get("base_seconds_overrides"),
            {},
        ),
        include_market_dangerous=_bool_from(
            "MARKET_GAP_INCLUDE_MARKET_DANGEROUS", inc.get("market_dangerous"), True
        ),
        market_dangerous_redis_key=_str_from(
            "MARKET_DANGEROUS_REDIS_KEY",
            md.get("redis_key"),
            "gate_hft:market_dangerous",
        ),
        market_dangerous_primary_table=_str_from(
            "MARKET_DANGEROUS_PRIMARY_TABLE",
            md.get("primary_table"),
            "gate_bookticker",
        ),
        market_dangerous_compare_table=_str_from(
            "MARKET_DANGEROUS_COMPARE_TABLE",
            md.get("compare_table"),
            "binance_bookticker",
        ),
        market_dangerous_absolute_threshold=_int_from(
            "MARKET_DANGEROUS_ABSOLUTE_THRESHOLD",
            md.get("absolute_threshold"),
            150000,
        ),
        market_dangerous_window_secs=_int_from(
            "MARKET_DANGEROUS_WINDOW_SECS", md.get("window_secs"), 60
        ),
        market_dangerous_sticky_secs=_int_from(
            "MARKET_DANGEROUS_STICKY_SECS", md.get("sticky_secs"), 600
        ),
        market_dangerous_cadence_secs=float(
            os.environ.get(
                "MARKET_DANGEROUS_CADENCE_SECS",
                md.get("cadence_secs", 5),
            )
        ),
        heartbeat_prefix=_str_from(
            "HEARTBEAT_REDIS_PREFIX",
            hb.get("redis_prefix"),
            DEFAULT_HEARTBEAT_PREFIX,
        ),
        telegram_bot_token=os.getenv("TELEGRAM_BOT_TOKEN") or None,
        telegram_chat_id=os.getenv("TELEGRAM_CHAT_ID") or None,
        alert_after_consecutive_failures=_int_from(
            "ALERT_AFTER_CONSECUTIVE_FAILURES",
            file_cfg.get("alert_after_consecutive_failures"),
            DEFAULT_ALERT_AFTER_CONSECUTIVE_FAILURES,
        ),
    )

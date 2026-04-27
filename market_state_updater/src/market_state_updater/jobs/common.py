"""Job 들이 공유하는 상수 / 헬퍼.

윈도우는 fast / slow 로 분리.
- fast (≤ 5분): 짧은 cycle 로 자주 갱신해야 의미 있음 (전략의 단기 시그널)
- slow (>  5분): 느린 cycle 로 충분 (트렌드/장기 컨텍스트)

운영은 두 데몬을 따로 띄우는 게 권장 (`--windows fast --interval 30`,
`--windows slow --interval 600`). `--windows all` 은 한 프로세스로 다 도는 모드 (개발/CI 용).
"""

from __future__ import annotations

import os
import time

# market_watcher.rs 의 filter_valid_gaps 와 동일 임계
MAX_ABS_GAP = 0.1

# FILL(PREV) lookback (분). window 보다 크게 잡아야 sparse symbol 도 forward-fill 됨.
FILL_PREV_LOOKBACK_MINUTES = int(os.getenv("MARKET_GAP_FILL_PREV_LOOKBACK_MINUTES", "10"))

# === gap / spread_pair / gate_web_gap 윈도우 (분) ===
FAST_WINDOWS: tuple[int, ...] = (1, 5)
SLOW_WINDOWS: tuple[int, ...] = (10, 15, 30, 60, 240, 720)
WINDOW_MINUTES: tuple[int, ...] = FAST_WINDOWS + SLOW_WINDOWS  # 모드 == "all" 일 때

# === price_change 윈도우 (분) ===
FAST_PRICE_CHANGE_WINDOWS: tuple[int, ...] = (1, 5)
SLOW_PRICE_CHANGE_WINDOWS: tuple[int, ...] = (15, 30, 60, 240, 1440)
PRICE_CHANGE_WINDOWS: tuple[int, ...] = FAST_PRICE_CHANGE_WINDOWS + SLOW_PRICE_CHANGE_WINDOWS

# === 윈도우별 갱신 cadence (초) ===
# scheduler 가 각 schedule 을 cadence_secs 마다 1번 실행. 모든 job 류 (gap/spread/corr/...)
# 가 같은 dict 사용. PRICE_CHANGE_WINDOWS 의 별도 값 (15, 1440) 도 포함.
# 디자인 원칙: window 길이의 1/12 ~ 1/20 정도 — 너무 dense 하면 cycle 비용 ↑,
# 너무 sparse 하면 metric 갱신이 윈도우보다 늦어짐.
DEFAULT_CADENCE_SECS: dict[int, float] = {
    1: 5,
    5: 25,
    10: 60,
    15: 90,
    30: 180,
    60: 300,
    240: 1200,
    720: 3600,
    1440: 7200,
}


def parse_cadence_overrides(s: str) -> dict[int, float]:
    """env CADENCE_OVERRIDES_SECS='1:5,5:25,60:120' → {1:5, 5:25, 60:120}.

    빈 문자열은 빈 dict. 잘못된 토큰은 ValueError. cadence_secs 는 float 허용.
    """
    out: dict[int, float] = {}
    for part in s.split(","):
        part = part.strip()
        if not part:
            continue
        k_str, _, v_str = part.partition(":")
        if not v_str:
            raise ValueError(f"invalid cadence override token: {part!r}")
        out[int(k_str.strip())] = float(v_str.strip())
    return out


def cadence_for_window(
    window_minutes: int, overrides: dict[int, float] | None = None
) -> float:
    """window → cadence (초). overrides 우선. 매핑 없으면 max(1, window_min*60/12) fallback."""
    table = dict(DEFAULT_CADENCE_SECS)
    if overrides:
        table.update(overrides)
    if window_minutes in table:
        return table[window_minutes]
    return max(1.0, window_minutes * 60 / 12)

# === price_change_gap_corr: window 별 X return 의 sample 간격 (초) ===
# X = gate_web mid 의 1-step return, Y = gate_web↔binance mid gap.
# step 단위 = SAMPLE BY {step}s. 각 윈도우당 sample 100~250 정도 되도록 디폴트 설정.
DEFAULT_CORR_RETURN_SECONDS: dict[int, int] = {
    1: 1,     # 60 samples
    5: 5,     # 60
    10: 5,    # 120
    15: 10,   # 90
    30: 10,   # 180
    60: 30,   # 120
    240: 60,  # 240
    720: 300, # 144
}


def parse_corr_return_seconds_overrides(s: str) -> dict[int, int]:
    """env CORR_RETURN_SECONDS_OVERRIDES='1:1,5:5,10:10' → {1:1, 5:5, 10:10}.

    부적절한 토큰은 ValueError. 빈 문자열은 빈 dict.
    """
    out: dict[int, int] = {}
    for part in s.split(","):
        part = part.strip()
        if not part:
            continue
        k_str, _, v_str = part.partition(":")
        if not v_str:
            raise ValueError(f"invalid CORR_RETURN_SECONDS override token: {part!r}")
        out[int(k_str.strip())] = int(v_str.strip())
    return out


def corr_return_seconds(
    window_minutes: int, overrides: dict[int, int] | None = None
) -> int:
    """window 에 매핑된 return interval (초). overrides 가 있으면 우선.
    매핑 없으면 max(1, window_minutes) 로 fallback.
    """
    table = dict(DEFAULT_CORR_RETURN_SECONDS)
    if overrides:
        table.update(overrides)
    return table.get(int(window_minutes), max(1, int(window_minutes)))


def windows_for_mode(mode: str) -> tuple[tuple[int, ...], tuple[int, ...]]:
    """mode → (gap-family windows, price_change windows)."""
    m = mode.lower()
    if m == "fast":
        return FAST_WINDOWS, FAST_PRICE_CHANGE_WINDOWS
    if m == "slow":
        return SLOW_WINDOWS, SLOW_PRICE_CHANGE_WINDOWS
    if m == "all":
        return WINDOW_MINUTES, PRICE_CHANGE_WINDOWS
    raise ValueError(f"unknown windows mode: {mode!r} (expected fast|slow|all)")


def bookticker_table(exchange: str) -> str:
    """exchange → QuestDB bookticker 테이블명 (예: gate → gate_bookticker)."""
    return f"{exchange.lower()}_bookticker"


def bookticker_table_for_gap_base(base_exchange: str) -> str:
    """gap base side 테이블: gate → gate_bookticker, gate_web → gate_webbookticker."""
    b = base_exchange.lower().strip()
    if b == "gate_web":
        return "gate_webbookticker"
    return f"{b}_bookticker"


# gap / spread_pair / gate_web_gap 의 SAMPLE BY 단위. QuestDB 문법 그대로
# ("100T" = 100ms, "1s", "5s", "1m" 등). 운영 중 QuestDB 부하 보고 조정용.
DEFAULT_SAMPLE_INTERVALS: dict[int, str] = {
    1: "100T",
    5: "200T",
    10: "200T",
    15: "200T",
    30: "500T",
    60: "1s",
    240: "5s",
    720: "10s",
}

# main 에서 set_sample_interval_overrides(cfg.sample_interval_overrides) 로 주입.
# 빈 dict 면 DEFAULT_SAMPLE_INTERVALS 만 사용.
_SAMPLE_INTERVAL_OVERRIDES: dict[int, str] = {}


def set_sample_interval_overrides(overrides: dict[int, str]) -> None:
    """gap 류 job 의 SAMPLE BY 단위 override (process-wide). main 에서 1회 호출."""
    global _SAMPLE_INTERVAL_OVERRIDES
    _SAMPLE_INTERVAL_OVERRIDES = dict(overrides)


def parse_sample_interval_overrides(s: str) -> dict[int, str]:
    """env MARKET_GAP_SAMPLE_INTERVAL_OVERRIDES='1:100T,5:200T,60:1s' 파싱.

    빈 문자열은 빈 dict. 잘못된 토큰은 ValueError.
    """
    out: dict[int, str] = {}
    for part in s.split(","):
        part = part.strip()
        if not part:
            continue
        k_str, _, v_str = part.partition(":")
        if not v_str:
            raise ValueError(f"invalid sample interval override token: {part!r}")
        out[int(k_str.strip())] = v_str.strip()
    return out


def sample_interval_for_window(window_minutes: int) -> str:
    """윈도우 길이에 따른 SAMPLE BY 간격.

    우선순위: process-wide overrides > DEFAULT_SAMPLE_INTERVALS > 코드 fallback.
    fallback: ≤1m → 100T, ≤120m → 1s, > → 5s.
    """
    w = int(window_minutes)
    if w in _SAMPLE_INTERVAL_OVERRIDES:
        return _SAMPLE_INTERVAL_OVERRIDES[w]
    if w in DEFAULT_SAMPLE_INTERVALS:
        return DEFAULT_SAMPLE_INTERVALS[w]
    if w <= 1:
        return "100T"
    if w <= 120:
        return "1s"
    return "5s"


def filter_valid_gaps(gaps: dict[str, float]) -> dict[str, float]:
    """|avg_mid_gap| < MAX_ABS_GAP 인 심볼만. market_watcher.rs 와 동일."""
    return {s: g for s, g in gaps.items() if abs(g) < MAX_ABS_GAP}


def now_ms() -> int:
    return int(time.time() * 1000)

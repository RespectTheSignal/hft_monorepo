"""Symbol 정규화 헬퍼 — canonical ↔ exchange-specific 매핑.

canonical: base asset 기호 (예: "SOL", "BTC"). USDT-margin perpetual 기준.
exchange-specific:
  - flipster: "{BASE}USDT.PERP"    예) "SOLUSDT.PERP"
  - binance : "{BASE}_USDT"         예) "SOL_USDT"

Rust 번역:
  - 함수 기반, 상태 없음 → 자유로운 fn 시그니처
  - &str in, String out 패턴
"""

from __future__ import annotations

# 지원 거래소 상수 — Rust enum Exchange로 대응
EXCHANGE_FLIPSTER: str = "flipster"
EXCHANGE_BINANCE: str = "binance"
EXCHANGE_GATE: str = "gate"

_FLIPSTER_PERP_SUFFIX: str = "USDT.PERP"
_BINANCE_USDT_SUFFIX: str = "_USDT"


def to_exchange_symbol(exchange: str, canonical: str) -> str:
    """canonical base("SOL") → exchange별 심볼.

    지원하지 않는 거래소는 canonical 그대로 반환.
    """
    base = canonical.upper()
    if exchange == EXCHANGE_FLIPSTER:
        return f"{base}{_FLIPSTER_PERP_SUFFIX}"
    if exchange == EXCHANGE_BINANCE:
        return f"{base}{_BINANCE_USDT_SUFFIX}"
    if exchange == EXCHANGE_GATE:
        return f"{base}{_BINANCE_USDT_SUFFIX}"
    return canonical


def to_canonical(exchange: str, symbol: str) -> str | None:
    """exchange별 심볼 → canonical base. 형식 불일치 시 None.

    - flipster "SOLUSDT.PERP" → "SOL"
    - binance  "SOL_USDT"     → "SOL"
    - 기타/spot/USDC-margin 은 None
    """
    if exchange == EXCHANGE_FLIPSTER:
        if symbol.endswith(_FLIPSTER_PERP_SUFFIX):
            return symbol[: -len(_FLIPSTER_PERP_SUFFIX)] or None
        return None
    if exchange == EXCHANGE_BINANCE:
        if symbol.endswith(_BINANCE_USDT_SUFFIX):
            return symbol[: -len(_BINANCE_USDT_SUFFIX)] or None
        return None
    if exchange == EXCHANGE_GATE:
        if symbol.endswith(_BINANCE_USDT_SUFFIX):
            return symbol[: -len(_BINANCE_USDT_SUFFIX)] or None
        return None
    return None


def is_supported_symbol(exchange: str, symbol: str) -> bool:
    """해당 거래소의 USDT-margin perpetual 포맷 여부.

    aggregator.accept predicate로 사용 가능.
    """
    return to_canonical(exchange, symbol) is not None


def symbols_for(canonical: str) -> dict[str, str]:
    """모든 지원 거래소에 대한 exchange-specific 심볼 맵"""
    return {
        EXCHANGE_FLIPSTER: to_exchange_symbol(EXCHANGE_FLIPSTER, canonical),
        EXCHANGE_BINANCE: to_exchange_symbol(EXCHANGE_BINANCE, canonical),
        EXCHANGE_GATE: to_exchange_symbol(EXCHANGE_GATE, canonical),
    }

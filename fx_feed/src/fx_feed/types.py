"""fx_feed 핵심 데이터 타입."""

from __future__ import annotations

import math
from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class FxTick:
    """FX bookticker 정규화 레코드.

    모든 소스는 이 타입으로 정규화된 뒤 QuestDbSink로 흘러간다.
    delay_ms = (recv_ns - source_ns) / 1e6 은 QuestDbSink가 계산한다.
    """

    source: str                # "binance" | "dxfeed" | "databento"
    symbol: str                # Gate FX 포맷 (예: "EUR_USD")
    raw_symbol: str            # 원본 포맷 (디버깅용)
    bid_price: float
    ask_price: float
    bid_size: float            # 없으면 math.nan
    ask_size: float            # 없으면 math.nan
    source_ns: int | None      # 거래소 event time. 소스가 제공 안 하면 None
    source_send_ns: int | None # 거래소 송신 시각 (Binance `E` vs `T` 같이 구분되는 경우). 없으면 None
    recv_ns: int               # WS 수신 직후 (JSON parse 전)
    parse_ns: int              # JSON parse 완료 후, enqueue 직전

    @property
    def wire_delay_ms(self) -> float:
        """거래소 event → WS 수신 (네트워크 + 거래소 내부 지연)."""
        if self.source_ns is None:
            return math.nan
        return (self.recv_ns - self.source_ns) / 1e6

    @property
    def exchange_delay_ms(self) -> float:
        """거래소 내부 지연: event 발생 → 송신 (Binance E-T 같은 경우)."""
        if self.source_ns is None or self.source_send_ns is None:
            return math.nan
        return (self.source_send_ns - self.source_ns) / 1e6

    @property
    def parse_delay_ms(self) -> float:
        """WS recv → JSON parse 완료."""
        return (self.parse_ns - self.recv_ns) / 1e6

    @property
    def delay_ms(self) -> float:
        """별칭: 일반적으로 '지연'이라 하면 wire_delay_ms."""
        return self.wire_delay_ms

    @property
    def spread_bps(self) -> float:
        mid = 0.5 * (self.bid_price + self.ask_price)
        if mid <= 0.0 or not math.isfinite(mid):
            return math.nan
        return (self.ask_price - self.bid_price) / mid * 10_000.0

"""PnL 트래커 — 심볼별 포지션 및 실현 PnL 기록.

단순 모델: 같은 방향 추가 시 평균 단가, 반대 방향이면 realized PnL 가산.
수수료는 fill 당 바로 차감.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from strategy_flipster.types import OrderSide


@dataclass
class Trade:
    ts_ns: int
    symbol: str
    side: OrderSide
    qty: float
    price: float
    fee: float
    realized: float          # 이 체결이 실현시킨 PnL (수수료 제외)
    position_after: float    # 체결 후 순 포지션
    avg_entry_after: float


@dataclass
class _SymbolBook:
    """심볼별 포지션 장부 (netted position)"""

    position: float = 0.0        # +long, -short
    avg_entry: float = 0.0       # 평균 진입가
    realized: float = 0.0        # 누적 realized PnL
    fees: float = 0.0            # 누적 fee

    def apply(self, side: OrderSide, qty: float, price: float) -> float:
        """체결 반영. 반환: 이 체결이 realize 한 PnL (수수료 제외)."""
        delta = qty if side == OrderSide.BUY else -qty
        prev = self.position
        new = prev + delta
        realized_this = 0.0

        # 포지션이 같은 방향으로 커지거나, 0 에서 생기면 평균 단가 재계산
        if prev == 0 or (prev > 0 and delta > 0) or (prev < 0 and delta < 0):
            # 평균 단가 업데이트
            total_cost = abs(prev) * self.avg_entry + qty * price
            total_qty = abs(prev) + qty
            self.avg_entry = total_cost / total_qty if total_qty > 0 else 0.0
        else:
            # 부분/전체 청산 (방향 반대)
            closing_qty = min(abs(delta), abs(prev))
            if prev > 0:
                # long 청산 — 높게 팔수록 이익
                realized_this = closing_qty * (price - self.avg_entry)
            else:
                # short 청산 — 낮게 살수록 이익
                realized_this = closing_qty * (self.avg_entry - price)
            self.realized += realized_this
            # 청산 후 뒤집히는 경우
            if abs(delta) > abs(prev):
                # 새 방향으로 잔여 수량 생김
                self.avg_entry = price
            elif new == 0:
                self.avg_entry = 0.0
            # new 는 계속 유지

        self.position = new
        return realized_this


@dataclass
class PnlTrackerStats:
    total_trades: int = 0
    total_realized: float = 0.0
    total_fees: float = 0.0
    total_volume_usd: float = 0.0
    wins: int = 0           # realized > 0 인 청산 횟수
    losses: int = 0         # realized < 0
    max_drawdown: float = 0.0
    peak_equity: float = 0.0


class PnlTracker:
    """체결 이벤트 수신 → 심볼별 장부 갱신 + 트레이드 로그"""

    def __init__(self) -> None:
        self._books: dict[str, _SymbolBook] = {}
        self._trades: list[Trade] = []
        self._stats: PnlTrackerStats = PnlTrackerStats()

    @property
    def stats(self) -> PnlTrackerStats:
        return self._stats

    @property
    def trades(self) -> list[Trade]:
        return self._trades

    def equity(self) -> float:
        """실현 PnL − 누적 수수료 (현재 미실현 제외)"""
        return sum(b.realized - b.fees for b in self._books.values())

    def on_fill(
        self,
        symbol: str,
        side: OrderSide,
        qty: float,
        price: float,
        fee: float,
        ts_ns: int,
    ) -> None:
        book = self._books.get(symbol)
        if book is None:
            book = _SymbolBook()
            self._books[symbol] = book
        realized_this = book.apply(side, qty, price)
        book.fees += fee

        self._stats.total_trades += 1
        self._stats.total_realized += realized_this
        self._stats.total_fees += fee
        self._stats.total_volume_usd += qty * price
        if realized_this > 1e-12:
            self._stats.wins += 1
        elif realized_this < -1e-12:
            self._stats.losses += 1

        eq = self.equity()
        if eq > self._stats.peak_equity:
            self._stats.peak_equity = eq
        dd = self._stats.peak_equity - eq
        if dd > self._stats.max_drawdown:
            self._stats.max_drawdown = dd

        self._trades.append(Trade(
            ts_ns=ts_ns,
            symbol=symbol,
            side=side,
            qty=qty,
            price=price,
            fee=fee,
            realized=realized_this,
            position_after=book.position,
            avg_entry_after=book.avg_entry,
        ))

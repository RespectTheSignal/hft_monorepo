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
    wins: int = 0                      # realized > 0 인 청산 횟수
    losses: int = 0                    # realized < 0

    # Realized 기준 peak / drawdown
    max_drawdown: float = 0.0          # realized - fees 기준
    peak_equity: float = 0.0

    # Mark-to-market 기준 (unrealized 포함)
    max_drawdown_mtm: float = 0.0      # realized - fees + unrealized
    peak_equity_mtm: float = 0.0
    last_unrealized: float = 0.0
    worst_unrealized: float = 0.0      # 가장 컸던 미실현 손실 (음수 → 절대값 큰 것)


@dataclass
class SymbolPnlRow:
    symbol: str
    trades: int = 0
    volume_usd: float = 0.0
    realized: float = 0.0
    fees: float = 0.0
    wins: int = 0
    losses: int = 0

    @property
    def net(self) -> float:
        return self.realized - self.fees

    @property
    def win_rate(self) -> float:
        total = self.wins + self.losses
        return (self.wins / total * 100.0) if total > 0 else 0.0


class PnlTracker:
    """체결 이벤트 수신 → 심볼별 장부 갱신 + 트레이드 로그"""

    def __init__(self) -> None:
        self._books: dict[str, _SymbolBook] = {}
        self._trades: list[Trade] = []
        self._stats: PnlTrackerStats = PnlTrackerStats()
        self._symbol_rows: dict[str, SymbolPnlRow] = {}

    @property
    def stats(self) -> PnlTrackerStats:
        return self._stats

    @property
    def trades(self) -> list[Trade]:
        return self._trades

    def per_symbol(self) -> list[SymbolPnlRow]:
        """심볼별 PnL 집계 (net 내림차순)"""
        rows = list(self._symbol_rows.values())
        rows.sort(key=lambda r: r.net, reverse=True)
        return rows

    def equity(self) -> float:
        """실현 PnL − 누적 수수료 (현재 미실현 제외)"""
        return sum(b.realized - b.fees for b in self._books.values())

    def unrealized(
        self,
        mark_prices: dict[str, float],
    ) -> float:
        """현재 포지션의 미실현 PnL (mark_prices 기준)

        mark_prices: {symbol: mid_price}. 없는 심볼은 0 으로 취급.
        """
        total = 0.0
        for sym, book in self._books.items():
            if abs(book.position) < 1e-12:
                continue
            mark = mark_prices.get(sym, 0.0)
            if mark <= 0:
                continue
            # long(+): mark - avg_entry, short(-): avg_entry - mark
            total += book.position * (mark - book.avg_entry)
        return total

    def mark_to_market(
        self,
        mark_prices: dict[str, float],
    ) -> None:
        """현재 mid 로 mtm equity 계산 후 peak/DD 갱신"""
        unrl = self.unrealized(mark_prices)
        realized_net = self.equity()  # realized - fees
        eq_mtm = realized_net + unrl
        self._stats.last_unrealized = unrl
        if unrl < self._stats.worst_unrealized:
            self._stats.worst_unrealized = unrl
        if eq_mtm > self._stats.peak_equity_mtm:
            self._stats.peak_equity_mtm = eq_mtm
        dd_mtm = self._stats.peak_equity_mtm - eq_mtm
        if dd_mtm > self._stats.max_drawdown_mtm:
            self._stats.max_drawdown_mtm = dd_mtm

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

        row = self._symbol_rows.get(symbol)
        if row is None:
            row = SymbolPnlRow(symbol=symbol)
            self._symbol_rows[symbol] = row
        row.trades += 1
        row.volume_usd += qty * price
        row.realized += realized_this
        row.fees += fee

        self._stats.total_trades += 1
        self._stats.total_realized += realized_this
        self._stats.total_fees += fee
        self._stats.total_volume_usd += qty * price
        if realized_this > 1e-12:
            self._stats.wins += 1
            row.wins += 1
        elif realized_this < -1e-12:
            self._stats.losses += 1
            row.losses += 1

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

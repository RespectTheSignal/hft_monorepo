"""QuestDB 리플레이 feed — binance_bookticker / flipster_bookticker 스트림 읽기.

psycopg3 서버-사이드 커서(fetchmany)로 메모리 안 터지게 청크 단위 iter.
타임스탬프 기준 오름차순. 심볼 필터 지원.

Rust: tokio-postgres + rust_decimal.
"""

from __future__ import annotations

from collections.abc import Iterator
from dataclasses import dataclass

import psycopg

from strategy_flipster.market_data.symbol import EXCHANGE_BINANCE, EXCHANGE_FLIPSTER
from strategy_flipster.types import BookTicker


@dataclass(frozen=True, slots=True)
class QdbConfig:
    """QuestDB PGWire 접속 설정"""

    host: str = "211.181.122.102"
    port: int = 8812
    user: str = "admin"
    password: str = "quest"
    dbname: str = "qdb"

    def conninfo(self) -> str:
        return (
            f"host={self.host} port={self.port} "
            f"user={self.user} password={self.password} dbname={self.dbname}"
        )


class QuestDBFeed:
    """단일 거래소 테이블의 bookticker 를 시간순으로 스트림.

    iterator 는 BookTicker 를 yield (time.time_ns 호출 없음 — ts_ns 는 DB 타임스탬프).
    """

    def __init__(
        self,
        config: QdbConfig,
        table: str,
        exchange: str,
        symbols: list[str],
        start_ns: int,
        end_ns: int,
        batch_size: int = 50_000,
    ) -> None:
        self._config: QdbConfig = config
        self._table: str = table
        self._exchange: str = exchange
        self._symbols: list[str] = symbols
        self._start_ns: int = start_ns
        self._end_ns: int = end_ns
        self._batch_size: int = batch_size

    def iter_events(self) -> Iterator[BookTicker]:
        """심볼 필터 + 시간 범위 적용하여 BookTicker iterator.

        QuestDB PGWire 는 named (server-side) cursor 미지원.
        대신 시간 범위를 청크 단위로 쪼개 순차 쿼리.
        """
        if not self._symbols:
            return
        symbol_list = ",".join(f"'{s}'" for s in self._symbols)

        # 청크 단위 (1h) — 하루치면 24회 쿼리. 더 큰 폭으로 조정 가능.
        chunk_ns = 3_600_000_000_000  # 1h

        with psycopg.connect(self._config.conninfo()) as conn:
            cur_start = self._start_ns
            while cur_start < self._end_ns:
                cur_end = min(cur_start + chunk_ns, self._end_ns)
                start_iso = _ns_to_iso(cur_start)
                end_iso = _ns_to_iso(cur_end)
                sql = (
                    f"SELECT symbol, bid_price, ask_price, timestamp "
                    f"FROM {self._table} "
                    f"WHERE symbol IN ({symbol_list}) "
                    f"  AND timestamp >= '{start_iso}' "
                    f"  AND timestamp <  '{end_iso}' "
                    f"ORDER BY timestamp"
                )
                with conn.cursor() as cur:
                    cur.execute(sql)
                    while True:
                        rows = cur.fetchmany(self._batch_size)
                        if not rows:
                            break
                        for row in rows:
                            symbol, bid, ask, ts = row
                            if ts is None:
                                continue
                            ts_ns = int(ts.timestamp() * 1_000_000_000) + ts.microsecond * 1000 - (int(ts.timestamp()) * 1_000_000_000 - int(ts.timestamp()) * 1_000_000_000)
                            # 간단: datetime aware → ns
                            ts_ns = int(ts.timestamp() * 1_000_000) * 1000 + (ts.microsecond - int(ts.timestamp() * 1_000_000) % 1_000_000) * 1000
                            # 아 복잡 — 재작성
                            ts_ns = _dt_to_ns(ts)
                            yield BookTicker(
                                exchange=self._exchange,
                                symbol=symbol,
                                bid_price=float(bid) if bid is not None else 0.0,
                                ask_price=float(ask) if ask is not None else 0.0,
                                bid_size=0.0,
                                ask_size=0.0,
                                last_price=0.0,
                                mark_price=0.0,
                                index_price=0.0,
                                event_time_ms=ts_ns // 1_000_000,
                                recv_ts_ns=ts_ns,
                            )
                cur_start = cur_end


def _ns_to_iso(ns: int) -> str:
    """ns → QuestDB-호환 ISO 문자열 (마이크로초까지)"""
    from datetime import datetime, timezone
    s = ns // 1_000_000_000
    us = (ns % 1_000_000_000) // 1000
    dt = datetime.fromtimestamp(s, tz=timezone.utc).replace(microsecond=us)
    return dt.strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def _dt_to_ns(dt) -> int:
    """datetime → epoch ns"""
    # dt.timestamp() 는 float 이라 μs 이하 정밀도 손실 가능. int 조합으로 변환.
    from datetime import datetime, timezone
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    epoch = datetime(1970, 1, 1, tzinfo=timezone.utc)
    delta = dt - epoch
    return delta.days * 86400 * 1_000_000_000 + delta.seconds * 1_000_000_000 + delta.microseconds * 1000


def make_binance_feed(
    config: QdbConfig,
    symbols: list[str],
    start_ns: int,
    end_ns: int,
) -> QuestDBFeed:
    return QuestDBFeed(
        config=config,
        table="binance_bookticker",
        exchange=EXCHANGE_BINANCE,
        symbols=symbols,
        start_ns=start_ns,
        end_ns=end_ns,
    )


def make_flipster_feed(
    config: QdbConfig,
    symbols: list[str],
    start_ns: int,
    end_ns: int,
) -> QuestDBFeed:
    return QuestDBFeed(
        config=config,
        table="flipster_bookticker",
        exchange=EXCHANGE_FLIPSTER,
        symbols=symbols,
        start_ns=start_ns,
        end_ns=end_ns,
    )

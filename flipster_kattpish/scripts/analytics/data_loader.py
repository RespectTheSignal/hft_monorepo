"""QuestDB connectivity. HTTP for small queries, pg-wire for bulk pulls."""
import os
import json
import urllib.parse
import urllib.request
from contextlib import contextmanager
from typing import Iterable

import pandas as pd
import psycopg2

QDB_HOST = os.getenv("QDB_HOST", "211.181.122.102")
QDB_HTTP = os.getenv("QUESTDB_HTTP_URL", f"http://{QDB_HOST}:9000")
QDB_PG_PORT = int(os.getenv("QDB_PG_PORT", "8812"))
QDB_USER = os.getenv("QDB_USER", "admin")
QDB_PASS = os.getenv("QDB_PASS", "quest")
QDB_DB = os.getenv("QDB_DB", "qdb")


def http_query(sql: str, timeout: int = 60) -> list:
    url = QDB_HTTP + "/exec?query=" + urllib.parse.quote(sql)
    with urllib.request.urlopen(url, timeout=timeout) as r:
        data = json.loads(r.read())
    if "error" in data:
        raise RuntimeError(f"QuestDB error: {data['error']} | sql={sql[:200]}")
    return data.get("dataset", [])


@contextmanager
def pg_conn():
    conn = psycopg2.connect(
        host=QDB_HOST, port=QDB_PG_PORT,
        user=QDB_USER, password=QDB_PASS, dbname=QDB_DB,
    )
    try:
        yield conn
    finally:
        conn.close()


def pg_df(sql: str, params: tuple | None = None) -> pd.DataFrame:
    with pg_conn() as c:
        return pd.read_sql(sql, c, params=params)


def list_tables() -> list[str]:
    return [row[0] for row in http_query("SHOW TABLES")]


# Exchanges actually in scope (have *_bookticker, perp, useful)
EXCHANGES = [
    "binance", "bybit", "bitget", "gate", "okx", "mexc",
    "kucoin", "kraken", "bingx", "backpack", "hyperliquid",
]
# Excluded: flipster (data gap), pyth (oracle), hashkey (sparse), fx (FX), ws (aggregate)


def time_range(exchange: str) -> tuple[str, str]:
    sql = f"SELECT min(timestamp), max(timestamp) FROM {exchange}_bookticker"
    return tuple(http_query(sql)[0])  # type: ignore


def common_window(exchanges: Iterable[str]) -> tuple[str, str]:
    """Return (latest start, earliest end) across given exchanges."""
    starts, ends = [], []
    for ex in exchanges:
        s, e = time_range(ex)
        starts.append(s); ends.append(e)
    return max(starts), min(ends)

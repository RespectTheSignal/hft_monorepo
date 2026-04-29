"""Tiny QuestDB wrapper over the PostgreSQL-wire protocol (port 8812)."""
from __future__ import annotations
import os
import pandas as pd
import psycopg


def connect():
    return psycopg.connect(
        host=os.environ.get("QDB_HOST", "127.0.0.1"),
        port=int(os.environ.get("QDB_PG_PORT", "8812")),
        user=os.environ.get("QDB_USER", "admin"),
        password=os.environ.get("QDB_PASSWORD", "quest"),
        dbname=os.environ.get("QDB_DB", "qdb"),
    )


def query(sql: str, params: tuple = ()) -> pd.DataFrame:
    with connect() as conn, conn.cursor() as cur:
        cur.execute(sql, params)
        cols = [d.name for d in cur.description] if cur.description else []
        rows = cur.fetchall() if cur.description else []
    return pd.DataFrame(rows, columns=cols)

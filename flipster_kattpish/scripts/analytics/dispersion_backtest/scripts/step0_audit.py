#!/usr/bin/env python3
"""
Step 0 — Symbol mapping verification for index dispersion backtest.

Reads from central QuestDB at 211.181.122.102:9000.
Read-only. No DDL, no writes.

Outputs (in ../results/):
  - step0_symbol_audit.md
  - step0_proposed_map_fix.json
  - step0_coverage_matrix.csv
  - step0_query_log.txt
"""

from __future__ import annotations

import csv
import json
import os
import sys
import time
import urllib.parse
import urllib.request
from typing import Any

QDB_HTTP = "http://211.181.122.102:9000/exec"

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
RESULTS_DIR = os.path.abspath(os.path.join(SCRIPT_DIR, "..", "results"))
os.makedirs(RESULTS_DIR, exist_ok=True)

QUERY_LOG_PATH = os.path.join(RESULTS_DIR, "step0_query_log.txt")
# Reset log
with open(QUERY_LOG_PATH, "w") as f:
    f.write("# Step 0 query log\n")


SYMBOL_MAP: dict[str, dict[str, str]] = {
    "binance":     {"BTC": "BTC_USDT", "ETH": "ETH_USDT", "SOL": "SOL_USDT", "BNB": "BNB_USDT", "XRP": "XRP_USDT", "DOGE": "DOGE_USDT"},
    "okx":         {"BTC": "BTC-USDT-SWAP", "ETH": "ETH-USDT-SWAP", "SOL": "SOL-USDT-SWAP", "BNB": "BNB-USDT-SWAP", "XRP": "XRP-USDT-SWAP", "DOGE": "DOGE-USDT-SWAP"},
    "bitget":      {"BTC": "BTC_USDT", "ETH": "ETH_USDT", "SOL": "SOL_USDT", "BNB": "BNB_USDT", "XRP": "XRP_USDT", "DOGE": "DOGE_USDT"},
    "kucoin":      {"BTC": "XBTUSDTM", "ETH": "ETHUSDTM", "SOL": "SOLUSDTM", "BNB": "BNBUSDTM", "XRP": "XRPUSDTM", "DOGE": "DOGEUSDTM"},
    "mexc":        {"BTC": "BTC_USDT", "ETH": "ETH_USDT", "SOL": "SOL_USDT", "BNB": "BNB_USDT", "XRP": "XRP_USDT", "DOGE": "DOGE_USDT"},
    "gate":        {"BTC": "BTC_USDT", "ETH": "ETH_USDT", "SOL": "SOL_USDT", "BNB": "BNB_USDT", "XRP": "XRP_USDT", "DOGE": "DOGE_USDT"},
    "bingx":       {"BTC": "BTC-USDT", "ETH": "ETH-USDT", "SOL": "SOL-USDT", "BNB": "BNB-USDT", "XRP": "XRP-USDT", "DOGE": "DOGE-USDT"},
    "kraken":      {"BTC": "PF_XBTUSD", "ETH": "PF_ETHUSD", "SOL": "PF_SOLUSD", "XRP": "PF_XRPUSD", "DOGE": "PF_DOGEUSD"},  # BNB not listed
    "hyperliquid": {"BTC": "BTC", "ETH": "ETH", "SOL": "SOL", "BNB": "BNB", "XRP": "XRP", "DOGE": "DOGE"},
    "aster":       {"BTC": "BTCUSDT", "ETH": "ETHUSDT", "SOL": "SOLUSDT", "BNB": "BNBUSDT", "XRP": "XRPUSDT", "DOGE": "DOGEUSDT"},
    "pancake":     {"BTC": "BTCUSDT", "ETH": "ETHUSDT", "SOL": "SOLUSDT"},
    "lighter":     {"BTC": "BTC", "ETH": "ETH", "SOL": "SOL", "BNB": "BNB", "XRP": "XRP", "DOGE": "DOGE"},
    "pyth":        {"BTC": "BTC/USD", "ETH": "ETH/USD", "SOL": "SOL/USD"},
}

EXCHANGE_TO_TABLE: dict[str, str] = {
    ex: f"{ex}_bookticker" for ex in SYMBOL_MAP
}

BASES = ["BTC", "ETH", "SOL", "BNB", "XRP", "DOGE"]


def log_query(sql: str) -> None:
    with open(QUERY_LOG_PATH, "a") as f:
        # one per line, escape newlines
        f.write(sql.replace("\n", " ").strip() + "\n")


def qdb_exec(sql: str, timeout: int = 25) -> dict[str, Any]:
    log_query(sql)
    url = QDB_HTTP + "?query=" + urllib.parse.quote(sql)
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            return json.loads(resp.read())
    except Exception as e:
        return {"error": str(e)}


def fmt_num(x: Any) -> str:
    if x is None:
        return "—"
    if isinstance(x, float):
        if abs(x) >= 1000:
            return f"{x:,.2f}"
        if abs(x) >= 1:
            return f"{x:.4f}"
        return f"{x:.6f}"
    return str(x)


def audit_symbol(exchange: str, base: str, mapped_symbol: str) -> dict[str, Any]:
    table = EXCHANGE_TO_TABLE[exchange]
    out: dict[str, Any] = {
        "exchange": exchange,
        "base": base,
        "mapped_symbol": mapped_symbol,
        "table": table,
        "exists": False,
        "ticks_24h": 0,
        "first_seen": None,
        "last_seen": None,
        "tick_rate_per_sec_24h": 0.0,
        "last_bid": None,
        "last_ask": None,
        "last_mid": None,
        "median_spread_bp_1h": None,
        "notes": [],
        "alt_candidates": [],
    }

    # Single combined query (24h count + min/max/last) — escape single quotes by doubling
    safe_sym = mapped_symbol.replace("'", "''")
    sql_main = (
        f"SELECT count() AS n_24h, "
        f"min(timestamp) AS first_seen, "
        f"max(timestamp) AS last_seen, "
        f"last(bid_price) AS last_bid, "
        f"last(ask_price) AS last_ask "
        f"FROM {table} "
        f"WHERE symbol = '{safe_sym}' "
        f"AND timestamp > dateadd('h', -24, now())"
    )
    res = qdb_exec(sql_main)
    if "error" in res:
        out["notes"].append(f"main_query_error: {res['error']}")
        return out
    ds = res.get("dataset") or []
    if ds:
        row = ds[0]
        n_24h = int(row[0] or 0)
        out["ticks_24h"] = n_24h
        out["first_seen"] = row[1]
        out["last_seen"] = row[2]
        out["last_bid"] = row[3]
        out["last_ask"] = row[4]
        if row[3] is not None and row[4] is not None and (row[3] + row[4]) > 0:
            out["last_mid"] = (row[3] + row[4]) / 2.0
        out["exists"] = n_24h > 0
        out["tick_rate_per_sec_24h"] = n_24h / 86400.0

    # If exists: get all-time first_seen for time coverage (used for long window check)
    if out["exists"]:
        sql_alltime = (
            f"SELECT min(timestamp) AS first_seen_all FROM {table} "
            f"WHERE symbol = '{safe_sym}'"
        )
        res2 = qdb_exec(sql_alltime)
        if "error" not in res2:
            ds2 = res2.get("dataset") or []
            if ds2 and ds2[0][0]:
                out["first_seen_all"] = ds2[0][0]

        # Median spread bp over last 1h
        sql_spread = (
            f"SELECT avg(spread_bp) AS avg_bp, "
            f"min(spread_bp) AS min_bp, "
            f"max(spread_bp) AS max_bp "
            f"FROM ("
            f"  SELECT (ask_price - bid_price) / ((ask_price + bid_price) / 2.0) * 1e4 AS spread_bp "
            f"  FROM {table} "
            f"  WHERE symbol = '{safe_sym}' "
            f"  AND timestamp > dateadd('h', -1, now()) "
            f"  AND bid_price > 0 AND ask_price > 0"
            f")"
        )
        # Note: QuestDB lacks median in std SQL. Use approx_percentile instead.
        sql_spread = (
            f"SELECT approx_percentile(spread_bp, 0.5, 4) AS median_bp, "
            f"approx_percentile(spread_bp, 0.05, 4) AS p5_bp, "
            f"approx_percentile(spread_bp, 0.95, 4) AS p95_bp, "
            f"count() AS n "
            f"FROM ("
            f"  SELECT (ask_price - bid_price) / ((ask_price + bid_price) / 2.0) * 1e4 AS spread_bp "
            f"  FROM {table} "
            f"  WHERE symbol = '{safe_sym}' "
            f"  AND timestamp > dateadd('h', -1, now()) "
            f"  AND bid_price > 0 AND ask_price > 0"
            f")"
        )
        res3 = qdb_exec(sql_spread)
        if "error" not in res3:
            ds3 = res3.get("dataset") or []
            if ds3 and ds3[0][0] is not None:
                out["median_spread_bp_1h"] = ds3[0][0]
                out["spread_bp_1h_p5"] = ds3[0][1]
                out["spread_bp_1h_p95"] = ds3[0][2]
                out["spread_bp_1h_n"] = ds3[0][3]
        else:
            # Fallback: simple avg if approx_percentile unavailable
            res3b = qdb_exec(sql_spread.replace("approx_percentile(spread_bp, 0.5, 4)", "avg(spread_bp)"))
            ds3b = res3b.get("dataset") or []
            if ds3b and ds3b[0][0] is not None:
                out["median_spread_bp_1h"] = ds3b[0][0]
                out["notes"].append("spread is avg, not median (approx_percentile unavailable)")
    else:
        # Not in 24h — check whether the symbol *ever* had data (stale feed) or never (mapping issue)
        sql_hist = (
            f"SELECT min(timestamp), max(timestamp), count() FROM {table} "
            f"WHERE symbol = '{safe_sym}'"
        )
        res_h = qdb_exec(sql_hist)
        if "error" not in res_h:
            ds_h = res_h.get("dataset") or []
            if ds_h and (ds_h[0][2] or 0) > 0:
                out["historical_first_seen"] = ds_h[0][0]
                out["historical_last_seen"] = ds_h[0][1]
                out["historical_count"] = ds_h[0][2]
                out["notes"].append(
                    f"symbol exists historically but no data in 24h "
                    f"(last_seen={ds_h[0][1]}, n_total={ds_h[0][2]:,}) — feed appears stale"
                )

        # Try ILIKE search for the base over a wider window (last 7d) for alt candidates
        sql_alt = (
            f"SELECT DISTINCT symbol FROM {table} "
            f"WHERE symbol ILIKE '%{base}%' "
            f"AND timestamp > dateadd('d', -7, now()) "
            f"LIMIT 50"
        )
        res4 = qdb_exec(sql_alt)
        if "error" not in res4:
            ds4 = res4.get("dataset") or []
            out["alt_candidates"] = [r[0] for r in ds4]
        else:
            out["notes"].append(f"alt_search_error: {res4['error']}")

    return out


def derive_status(audit: dict[str, Any]) -> dict[str, Any]:
    """Compute long_window_ok / short_window_ok flags."""
    long_ok = False
    short_ok = False
    if audit["exists"]:
        # short window: any data in last 24h is enough proxy; we'll mark short_ok
        # if first_seen < (now - 4d) i.e. covers full 4d window
        # We need first_seen_all (all-time min). Assume audit got it.
        first_seen_all = audit.get("first_seen_all") or audit.get("first_seen")
        # first_seen_all is ISO string like "2026-04-12T00:00:00.000000Z"
        if first_seen_all:
            # Crude epoch comparison
            try:
                from datetime import datetime, timezone
                ts = first_seen_all.replace("Z", "+00:00")
                # QuestDB returns "2026-04-12T00:00:00.000000Z" sometimes without TZ suffix.
                if "+" not in ts and "Z" not in first_seen_all:
                    ts = ts + "+00:00"
                first_dt = datetime.fromisoformat(ts.replace("Z", "+00:00")) if "Z" in first_seen_all else datetime.fromisoformat(ts)
                now = datetime.now(timezone.utc)
                age_days = (now - first_dt).total_seconds() / 86400.0
                short_ok = age_days >= 4.0 and audit["ticks_24h"] > 0
                long_ok = age_days >= 60.0 and audit["ticks_24h"] > 0
                audit["age_days"] = age_days
            except Exception as e:
                audit["notes"].append(f"date_parse_error: {e}")
    audit["long_window_ok"] = long_ok
    audit["short_window_ok"] = short_ok

    # Status string
    if not audit["exists"]:
        if audit.get("historical_count"):
            audit["status"] = "STALE"
        elif audit.get("alt_candidates"):
            audit["status"] = "MISSING_HAS_ALT"
        else:
            audit["status"] = "MISSING"
    else:
        if audit["last_mid"] is None or audit["last_mid"] <= 0:
            audit["status"] = "BAD_PRICE"
        elif audit["median_spread_bp_1h"] is not None and audit["median_spread_bp_1h"] > 50:
            audit["status"] = "WIDE_SPREAD"
        elif long_ok:
            audit["status"] = "OK_LONG"
        elif short_ok:
            audit["status"] = "OK_SHORT"
        else:
            audit["status"] = "OK_THIN"
    return audit


# Rough sanity bounds for "is this the right asset"
PRICE_RANGE = {
    "BTC":  (10_000, 250_000),
    "ETH":  (500, 15_000),
    "SOL":  (10, 1_000),
    "BNB":  (50, 3_000),
    "XRP":  (0.1, 20.0),
    "DOGE": (0.01, 5.0),
}


def main() -> None:
    audits: list[dict[str, Any]] = []
    proposed_fix: dict[str, dict[str, str]] = {}

    for ex, sym_map in SYMBOL_MAP.items():
        for base in BASES:
            if base not in sym_map:
                # not mapped (e.g. kraken BNB, pancake XRP/DOGE/BNB, pyth BNB/XRP/DOGE)
                audits.append({
                    "exchange": ex,
                    "base": base,
                    "mapped_symbol": "(not mapped)",
                    "table": EXCHANGE_TO_TABLE[ex],
                    "exists": False,
                    "ticks_24h": 0,
                    "tick_rate_per_sec_24h": 0.0,
                    "last_bid": None,
                    "last_ask": None,
                    "last_mid": None,
                    "median_spread_bp_1h": None,
                    "first_seen": None,
                    "last_seen": None,
                    "long_window_ok": False,
                    "short_window_ok": False,
                    "status": "NOT_MAPPED",
                    "notes": ["base not in SYMBOL_MAP for this exchange"],
                    "alt_candidates": [],
                })
                continue

            mapped = sym_map[base]
            print(f"  -> {ex} / {base} = {mapped}", flush=True)
            a = audit_symbol(ex, base, mapped)
            a = derive_status(a)

            # Sanity-check price band
            lo, hi = PRICE_RANGE[base]
            if a["exists"] and a["last_mid"] is not None:
                if not (lo <= a["last_mid"] <= hi):
                    a["notes"].append(f"price {a['last_mid']:.4g} outside expected range [{lo}, {hi}] for {base}")
                    if a["status"].startswith("OK"):
                        a["status"] = "BAD_PRICE"

            # Suggest fixes for missing mappings
            if not a["exists"] and a["alt_candidates"]:
                # heuristic: prefer USDT perp-like
                cands = a["alt_candidates"]
                # Filter to those that contain base AND USDT/USD (avoid pure base-only that is the same as mapped)
                scored: list[tuple[int, str]] = []
                for c in cands:
                    cu = c.upper()
                    score = 0
                    if base in cu:
                        score += 5
                    if "USDT" in cu:
                        score += 3
                    elif "USD" in cu:
                        score += 1
                    if cu == mapped.upper():
                        score = -1
                    scored.append((score, c))
                scored.sort(key=lambda x: -x[0])
                if scored and scored[0][0] > 0:
                    a["proposed"] = scored[0][1]
                    proposed_fix.setdefault(ex, {})[base] = scored[0][1]

            audits.append(a)
            time.sleep(0.05)  # be nice

    # Write outputs

    # 1. Markdown report
    md_path = os.path.join(RESULTS_DIR, "step0_symbol_audit.md")
    lines: list[str] = []
    lines.append("# Step 0 — Symbol Audit Report")
    lines.append("")
    lines.append(f"Generated by `scripts/step0_audit.py`. Source: central QuestDB at 211.181.122.102:9000.")
    lines.append("")
    lines.append("**Status legend**:")
    lines.append("- `OK_LONG`: exists, ≥60d coverage (long-window eligible)")
    lines.append("- `OK_SHORT`: exists, ≥4d coverage (short-window eligible)")
    lines.append("- `OK_THIN`: exists but <4d coverage")
    lines.append("- `WIDE_SPREAD`: median 1h spread > 50bp (suspicious)")
    lines.append("- `BAD_PRICE`: last_mid out of expected band for base")
    lines.append("- `STALE`: symbol mapping IS correct (historical data exists) but feed stopped >24h ago")
    lines.append("- `MISSING`: 0 rows in 24h, no historical data, no alt candidates found")
    lines.append("- `MISSING_HAS_ALT`: 0 rows but ILIKE search found candidates (see notes)")
    lines.append("- `NOT_MAPPED`: base not declared in SYMBOL_MAP for this exchange")
    lines.append("")

    by_base: dict[str, list[dict[str, Any]]] = {b: [] for b in BASES}
    for a in audits:
        by_base[a["base"]].append(a)

    for base in BASES:
        lines.append(f"## Base: {base}")
        lines.append("")
        lines.append("| exchange | mapped_symbol | exists | 24h_ticks | tick/s | first_seen | spread_bp_med | last_mid | status | notes |")
        lines.append("|---|---|---|---|---|---|---|---|---|---|")
        for a in by_base[base]:
            ticks = a["ticks_24h"]
            rate = a["tick_rate_per_sec_24h"]
            first_seen = a.get("first_seen_all") or a.get("first_seen") or "—"
            spr = a.get("median_spread_bp_1h")
            lm = a.get("last_mid")
            notes_parts = list(a.get("notes", []))
            if a.get("alt_candidates"):
                show = a["alt_candidates"][:5]
                notes_parts.append(f"alt: {', '.join(show)}" + (f" (+{len(a['alt_candidates'])-5} more)" if len(a['alt_candidates']) > 5 else ""))
            if a.get("proposed"):
                notes_parts.append(f"PROPOSED FIX: {a['proposed']}")
            notes = "; ".join(notes_parts) if notes_parts else ""
            lines.append(
                f"| {a['exchange']} | `{a['mapped_symbol']}` | {'yes' if a['exists'] else 'NO'} | "
                f"{ticks:,} | {rate:.2f} | {first_seen} | "
                f"{fmt_num(spr) if spr is not None else '—'} | "
                f"{fmt_num(lm) if lm is not None else '—'} | "
                f"`{a['status']}` | {notes} |"
            )
        lines.append("")

    # Summary table per base
    lines.append("## Per-base coverage summary")
    lines.append("")
    lines.append("| base | n_exchanges_ok | n_tier_a_ok | long_window_n | short_window_n |")
    lines.append("|---|---|---|---|---|")
    TIER_A = {"binance", "okx", "kucoin"}
    for base in BASES:
        rows = by_base[base]
        n_ok = sum(1 for r in rows if r["status"].startswith("OK"))
        n_a = sum(1 for r in rows if r["status"].startswith("OK") and r["exchange"] in TIER_A)
        n_long = sum(1 for r in rows if r.get("long_window_ok"))
        n_short = sum(1 for r in rows if r.get("short_window_ok"))
        lines.append(f"| {base} | {n_ok} | {n_a} | {n_long} | {n_short} |")
    lines.append("")

    with open(md_path, "w") as f:
        f.write("\n".join(lines))
    print(f"\nWrote {md_path}")

    # 2. Proposed fixes JSON
    fix_path = os.path.join(RESULTS_DIR, "step0_proposed_map_fix.json")
    with open(fix_path, "w") as f:
        json.dump(proposed_fix, f, indent=2)
    print(f"Wrote {fix_path}")

    # 3. Coverage matrix CSV
    csv_path = os.path.join(RESULTS_DIR, "step0_coverage_matrix.csv")
    with open(csv_path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow([
            "exchange", "base", "mapped_symbol",
            "long_window_ok", "short_window_ok",
            "tick_rate_per_sec", "ticks_24h",
            "median_spread_bp_1h", "last_mid",
            "first_seen", "status",
        ])
        for a in audits:
            w.writerow([
                a["exchange"], a["base"], a["mapped_symbol"],
                int(bool(a.get("long_window_ok"))), int(bool(a.get("short_window_ok"))),
                f"{a['tick_rate_per_sec_24h']:.4f}",
                a["ticks_24h"],
                fmt_num(a.get("median_spread_bp_1h")) if a.get("median_spread_bp_1h") is not None else "",
                fmt_num(a.get("last_mid")) if a.get("last_mid") is not None else "",
                a.get("first_seen_all") or a.get("first_seen") or "",
                a["status"],
            ])
    print(f"Wrote {csv_path}")

    print(f"Query log: {QUERY_LOG_PATH}")


if __name__ == "__main__":
    main()

#!/usr/bin/env bash
# Generate a PAIRS_BLACKLIST from recent position_log data.
# Picks symbols with avg_pnl_bp below threshold and at least N trades.
# Prints a PAIRS_BLACKLIST=... line you can paste into .env.
set -e
THRESH_BP="${1:--0.5}"   # blacklist symbols avg pnl below this
MIN_N="${2:-8}"          # require at least N trades

/home/clink/projects/flipster/backtest/.venv/bin/python - <<PY
import psycopg
c=psycopg.connect(host='127.0.0.1',port=8812,user='admin',password='quest',dbname='qdb',autocommit=True)
rows = c.execute("""SELECT symbol, count() n, avg(pnl_bp) avg_bp
                    FROM position_log WHERE mode='paper' AND strategy='pairs'
                    GROUP BY symbol""").fetchall()
losers = [r[0] for r in rows if r[1] >= $MIN_N and r[2] < $THRESH_BP]
print(f"# blacklisting {len(losers)} symbols with n>={$MIN_N} and avg<{$THRESH_BP}bp")
print("PAIRS_BLACKLIST=" + ",".join(sorted(losers)))
PY

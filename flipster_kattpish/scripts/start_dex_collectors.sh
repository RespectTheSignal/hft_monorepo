#!/usr/bin/env bash
# Long-running collector for new exchanges (PancakeSwap perp / AsterDex /
# Lighter). Writes bookticker rows to the central QuestDB so the Grafana
# lead-lag dashboard can query them alongside Binance.
#
# These are NOT served by gate1's central feed (it only knows
# Binance/Bybit/Bitget/Gate/MEXC/Hyperliquid/BingX), so we collect them
# ourselves and bypass DISABLE_TICK_WRITES via the per-exchange override
# in `ilp.rs::write_tick`.
#
# Run:
#   nohup ./scripts/start_dex_collectors.sh > /dev/null 2>&1 &
#
# Stop:
#   pkill -f 'release/collector.*PANCAKE_WS_DIRECT'

set -euo pipefail
PROJECT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT"

LOG="$PROJECT/logs/dex_collectors.log"
mkdir -p "$PROJECT/logs"

set -a
[ -f .env ] && . ./.env
set +a

# Off everything except the new exchanges.
export READ_FROM_QUESTDB=1
export DISABLE_TICK_WRITES=1   # bypassed for Pancake/Aster/Lighter only
export PAPER_BOT=0
export GATE_LEAD=0
export HL_LEAD=0
export MEXC_LEAD=0
export BINGX_LEAD=0
export SPREAD_REVERT=0
export HL_SPREAD_REVERT=0
export USE_ZMQ=0

# Don't double-subscribe to Binance/BingX (the live trading collector owns those).
export BINANCE_WS_DIRECT=0
export BINGX_WS_DIRECT=0

# New exchanges
export PANCAKE_WS_DIRECT=1
export ASTER_WS_DIRECT=1
export LIGHTER_WS_DIRECT=1
export VARIATIONAL_WS_DIRECT=1

# Distinct ZMQ socket so we don't clash with the trading collector.
export SIGNAL_PUB_ADDR="ipc:///tmp/flipster_kattpish_signal_dex.sock"
export FILL_SUB_ADDR="ipc:///tmp/flipster_kattpish_fill_dex.sock"

echo "[dex] starting Pancake/Aster/Lighter collectors, log=$LOG"
exec ./target/release/collector >> "$LOG" 2>&1

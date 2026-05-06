#!/usr/bin/env bash
# Run the bingx_lead 4-variant min_move_bp sweep on a date window of central
# QuestDB (full Binance∩BingX universe — whitelist disabled in sweep mode).
#
# Mirrors backtest_mexc_sweep.sh but with BingX as follower. Built to test
# whether BingX's lag profile (300ms-1.3s on majors per the lead-lag survey)
# generates positive edge under the same gate_lead detection.
#
# Usage:
#   ./scripts/backtest_bingx_sweep.sh <START_TS> <END_TS> [TAG]
set -euo pipefail

PROJECT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT"

START=${1:?need START_TS, e.g. 2026-05-04T00:00:00Z}
END=${2:?need END_TS, e.g. 2026-05-05T00:00:00Z}
TAG=${3:-bingx_sweep_$(date +%Y%m%d_%H%M)}
LOG="$PROJECT/logs/backtests/${TAG}.log"
mkdir -p "$PROJECT/logs/backtests"

set -a
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
  if [[ "$line" =~ ^[[:space:]]*([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
    export "${BASH_REMATCH[1]}=${BASH_REMATCH[2]}"
  fi
done < .env
set +a

export PAPER_BOT=1
export GATE_LEAD=0
export HL_LEAD=0
export MEXC_LEAD=0
export SPREAD_REVERT=0
export HL_SPREAD_REVERT=0
export BINGX_LEAD=1
export BACKTEST_BINGX_SWEEP=1
export BACKTEST_START_TS="$START"
export BACKTEST_END_TS="$END"
export BACKTEST_TAG="$TAG"
export USE_ZMQ=0
export READ_FROM_QUESTDB=0
export PAIRS_HEDGE_VENUE=binance

export SIGNAL_PUB_ADDR="ipc:///tmp/flipster_kattpish_signal_bt.sock"
export FILL_SUB_ADDR="ipc:///tmp/flipster_kattpish_fill_bt.sock"

echo "[backtest] tag=$TAG"
echo "[backtest] window=$START → $END"
echo "[backtest] follower=bingx, leader=binance"
echo "[backtest] log=$LOG"
exec ./target/release/collector 2>&1 | tee "$LOG"

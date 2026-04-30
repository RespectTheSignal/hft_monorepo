#!/usr/bin/env bash
# Sweep gate_lead anchor_s ∈ {2, 3, 5, 7, 10} with min_move_bp pinned at
# 30. Picks the best lookback window for the strategy without changing
# the move threshold.
#
# Usage:
#   ./scripts/backtest_anchor_sweep.sh <START_TS> <END_TS> [TAG]
# Example:
#   ./scripts/backtest_anchor_sweep.sh 2026-04-13T00:00:00Z 2026-04-29T00:00:00Z gl_anchor_v1
set -euo pipefail

PROJECT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT"

START=${1:?need START_TS}
END=${2:?need END_TS}
TAG=${3:-gl_anchor_$(date +%Y%m%d_%H%M)}
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
export SPREAD_REVERT=0
export BACKTEST_GL_ANCHOR_SWEEP=1
export BACKTEST_START_TS="$START"
export BACKTEST_END_TS="$END"
export BACKTEST_TAG="$TAG"
export USE_ZMQ=0
export READ_FROM_QUESTDB=0
export PAIRS_HEDGE_VENUE=binance

# Isolated IPC sockets so live can run in parallel
export SIGNAL_PUB_ADDR="ipc:///tmp/flipster_kattpish_signal_bt2.sock"
export FILL_SUB_ADDR="ipc:///tmp/flipster_kattpish_fill_bt2.sock"

echo "[bt anchor] tag=$TAG"
echo "[bt anchor] window=$START → $END"
echo "[bt anchor] log=$LOG"
exec ./target/release/collector 2>&1 | tee "$LOG"

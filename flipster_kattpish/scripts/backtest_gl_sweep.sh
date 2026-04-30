#!/usr/bin/env bash
# Run the gate_lead 4-variant min_move_bp sweep on a date window of central
# QuestDB, full Binance∩Flipster universe (whitelist disabled).
#
# Usage:
#   ./scripts/backtest_gl_sweep.sh <START_TS> <END_TS> [TAG]
# Example:
#   ./scripts/backtest_gl_sweep.sh 2026-04-13T00:00:00Z 2026-04-29T00:00:00Z gl_sweep_v1
#
# Output:
#   logs/backtests/<tag>.log    — full collector log
#   stdout                       — final BACKTEST SUMMARY table (per-variant totals)
#   QuestDB position_log         — rows tagged mode=<tag> for symbol-level analysis
set -euo pipefail

PROJECT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT"

START=${1:?need START_TS, e.g. 2026-04-13T00:00:00Z}
END=${2:?need END_TS, e.g. 2026-04-29T00:00:00Z}
TAG=${3:-gl_sweep_$(date +%Y%m%d_%H%M)}
LOG="$PROJECT/logs/backtests/${TAG}.log"
mkdir -p "$PROJECT/logs/backtests"

# Load .env so QDB_HOST etc. propagate. Skip secrets we don't need.
set -a
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
  if [[ "$line" =~ ^[[:space:]]*([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
    export "${BASH_REMATCH[1]}=${BASH_REMATCH[2]}"
  fi
done < .env
set +a

# Backtest-only env. PAPER_BOT=1 is the gate to spawn strategies; the
# replay branch fires when both BACKTEST_START_TS and BACKTEST_END_TS are
# set. SPREAD_REVERT=0 turns off SR (we only want gate_lead). Pairs are
# always spawned by paper_bot but they don't interfere with the gate_lead
# tag and add little overhead — kept on for reference.
export PAPER_BOT=1
export SPREAD_REVERT=0
export BACKTEST_GL_SWEEP=1
export BACKTEST_START_TS="$START"
export BACKTEST_END_TS="$END"
export BACKTEST_TAG="$TAG"
export USE_ZMQ=0          # replay reads from QuestDB, not live ZMQ
export READ_FROM_QUESTDB=0

# gate_lead reads from binance_bookticker as the lead. Hedge venue must be
# binance for the replay merger to schedule binance ticks alongside flipster.
export PAIRS_HEDGE_VENUE=binance

# Use isolated IPC socket paths so the backtest collector doesn't
# collide with a live collector that may be running on the same host.
export SIGNAL_PUB_ADDR="ipc:///tmp/flipster_kattpish_signal_bt.sock"
export FILL_SUB_ADDR="ipc:///tmp/flipster_kattpish_fill_bt.sock"

echo "[backtest] tag=$TAG"
echo "[backtest] window=$START → $END"
echo "[backtest] log=$LOG"
echo "[backtest] starting collector in replay mode…"
exec ./target/release/collector 2>&1 | tee "$LOG"

#!/usr/bin/env bash
# Run the mexc_lead 4-variant min_move_bp sweep on a date window of central
# QuestDB (full Binance∩MEXC universe — whitelist disabled in sweep mode).
#
# Mirror of backtest_gl_sweep.sh but with MEXC as the follower leg
# (Binance still the leader). Built to validate whether the latency-arb
# alpha identified in scripts/analytics (MEXC lags by ~300ms on alts) is
# captured by the same gate_lead detection logic.
#
# Usage:
#   ./scripts/backtest_mexc_sweep.sh <START_TS> <END_TS> [TAG]
# Example:
#   ./scripts/backtest_mexc_sweep.sh 2026-05-04T00:00:00Z 2026-05-05T00:00:00Z mexc_sweep_v1
#
# Output:
#   logs/backtests/<tag>.log    — full collector log
#   stdout                       — final BACKTEST SUMMARY table (per-variant totals)
#   QuestDB position_log         — rows tagged mode=<tag>
set -euo pipefail

PROJECT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT"

START=${1:?need START_TS, e.g. 2026-05-04T00:00:00Z}
END=${2:?need END_TS, e.g. 2026-05-05T00:00:00Z}
TAG=${3:-mexc_sweep_$(date +%Y%m%d_%H%M)}
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
# set. Disable everything except mexc_lead so we don't spend time on
# strategies that would just see no Flipster ticks (replay follower is
# mexc_bookticker, not flipster_bookticker).
export PAPER_BOT=1
export GATE_LEAD=0
export HL_LEAD=0
export SPREAD_REVERT=0
export HL_SPREAD_REVERT=0
export MEXC_LEAD=1
export BACKTEST_MEXC_SWEEP=1
export BACKTEST_START_TS="$START"
export BACKTEST_END_TS="$END"
export BACKTEST_TAG="$TAG"
export USE_ZMQ=0
export READ_FROM_QUESTDB=0

# mexc_lead reads from binance_bookticker as the leader (same as gate_lead).
# Hedge venue must be binance for the replay merger to schedule binance ticks
# alongside mexc ticks.
export PAIRS_HEDGE_VENUE=binance

# Use isolated IPC socket paths so the backtest collector doesn't
# collide with a live collector that may be running on the same host.
export SIGNAL_PUB_ADDR="ipc:///tmp/flipster_kattpish_signal_bt.sock"
export FILL_SUB_ADDR="ipc:///tmp/flipster_kattpish_fill_bt.sock"

echo "[backtest] tag=$TAG"
echo "[backtest] window=$START → $END"
echo "[backtest] follower=mexc, leader=binance"
echo "[backtest] log=$LOG"
echo "[backtest] starting collector in replay mode…"
exec ./target/release/collector 2>&1 | tee "$LOG"

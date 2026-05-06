#!/usr/bin/env bash
# Run BingX validation backtests sequentially: C → A → B
#
# C: 24h whitelist실측 backtest (same window as bingx_24h_v1, with whitelist applied)
# A: 24h OOS (different day)
# B: 24h grid sweep (anchor × exit at mm30)
#
# Each backtest uses isolated _bt IPC sockets. Won't collide with live paper.
# All logs in logs/backtests/. Position_log rows tagged with distinct mode.
set -euo pipefail
PROJECT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT"

mkdir -p "$PROJECT/logs/backtests"

run_backtest() {
  local TAG=$1
  local START=$2
  local END=$3
  shift 3
  local LOG="$PROJECT/logs/backtests/${TAG}.log"
  echo
  echo "============================================================"
  echo "[runner] starting $TAG ($START → $END)"
  echo "============================================================"

  # Clear previous backtest env
  unset BACKTEST_BINGX_SWEEP BACKTEST_BINGX_GRID GL_WHITELIST

  # Common backtest env
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
  export BINGX_LEAD=1
  export SPREAD_REVERT=0
  export HL_SPREAD_REVERT=0
  export BACKTEST_START_TS="$START"
  export BACKTEST_END_TS="$END"
  export BACKTEST_TAG="$TAG"
  export USE_ZMQ=0
  export READ_FROM_QUESTDB=0
  export PAIRS_HEDGE_VENUE=binance
  export SIGNAL_PUB_ADDR="ipc:///tmp/flipster_kattpish_signal_bt.sock"
  export FILL_SUB_ADDR="ipc:///tmp/flipster_kattpish_fill_bt.sock"

  # Variant-specific overrides via "$@"
  for ev in "$@"; do export "$ev"; done

  ./target/release/collector 2>&1 | tee "$LOG"
}

# C: whitelist 실측 backtest, same window as bingx_24h_v1
WHITELIST="LAB,UB,DASH,BIO,D,BR,GUA,KOMA,MERL,SIREN,REZ,RLC,ON,PENDLE,DUSK,H,PTB,BROCCOLIF3B,GENIUS,TAKE,GWEI,HEMI,EVAA,CATI,TRIA,SHELL,RAVE,CHR,TOWNS,FORM,LYN,SAPIEN,PIEVERSE,IRYS,ELSA,SQD,GRASS,ICNT,XNY,ORDI,HIGH,HANA,GRIFFAIN,BEAT,FLOCK,ONDO,KGEN,PENGU,JTO,BAS,BANANAS31,SPX,PNUT,FARTCOIN"
BLACKLIST="4,B,SKYAI,AIOT,GIGGLE,IDOL,MEGA,MUBARAK,STBL,Q,IN,MAV,MINA,SOLV,BAT"

run_backtest \
  bingx_C_wl_v1 \
  2026-05-03T14:00:00Z \
  2026-05-04T14:00:00Z \
  GL_MIN_MOVE_BP=30 \
  "GL_WHITELIST=$WHITELIST" \
  "GL_BLACKLIST=$BLACKLIST"

# A: OOS — different 24h window
run_backtest \
  bingx_A_oos_v1 \
  2026-05-02T07:30:00Z \
  2026-05-03T07:30:00Z \
  GL_MIN_MOVE_BP=30 \
  "GL_WHITELIST=$WHITELIST" \
  "GL_BLACKLIST=$BLACKLIST"

# B: grid sweep — anchor × exit (9 variants in parallel, no whitelist)
run_backtest \
  bingx_B_grid_v1 \
  2026-05-03T14:00:00Z \
  2026-05-04T14:00:00Z \
  BACKTEST_BINGX_GRID=1

echo
echo "[runner] all 3 validation backtests complete."

#!/usr/bin/env bash
# Helper used while migrating to teamreporter. Loads .env, launches the
# given binary (collector / executor) detached. Idempotent w.r.t. the
# pidfile arg. Usage:
#   ./_launch_remote.sh <pidfile> <binary> [args...]
set -euo pipefail

PIDFILE=$1; shift
BIN=$1; shift

PROJECT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT"
mkdir -p run logs

if [[ -f "$PIDFILE" ]] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
  echo "[launch] $BIN already running pid=$(cat "$PIDFILE")"
  exit 0
fi

set -a
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
  if [[ "$line" =~ ^[[:space:]]*([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
    export "${BASH_REMATCH[1]}=${BASH_REMATCH[2]}"
  fi
done < .env
set +a

LOG="logs/$(basename "$BIN").log"
[[ "$BIN" == *executor* ]] && LOG="logs/executor_v44.log"

setsid "$BIN" "$@" </dev/null >"$LOG" 2>&1 &
PID=$!
echo $PID > "$PIDFILE"
disown $PID 2>/dev/null || true
echo "[launch] $BIN pid=$PID log=$LOG"

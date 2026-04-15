#!/usr/bin/env bash
# Unified start/stop/status for the flipster research stack.
#
#   ./control.sh start      # bring up everything
#   ./control.sh stop       # stop everything
#   ./control.sh status     # list running processes + qdb tables
#   ./control.sh restart    # stop + start
#
# Services:
#   - QuestDB (if not already running)
#   - Rust collector (binance/bybit/bitget/gate, flipster if creds in env)
#   - Rust funding_poller
#   - Grafana on :3001
#
# Paper bot is NOT started by default; use: ./control.sh paper-start
set -euo pipefail

PROJECT="/home/gate1/projects/quant/hft_monorepo/flipster_kattpish"
RUN="$PROJECT/run"
LOGS="$PROJECT/logs"
QDB_HOME="$HOME/questdb/questdb-9.3.4-rt-linux-x86-64"
GRAFANA_HOME="$HOME/grafana/grafana-v12.4.2"

mkdir -p "$RUN" "$LOGS"

# Auto-load .env so children inherit it.
# Parse line-by-line instead of `source`ing — values may contain
# shell-special characters like `|`, `$`, backticks, etc.
if [[ -f "$PROJECT/.env" ]]; then
  while IFS= read -r _line || [[ -n "$_line" ]]; do
    # skip blanks and comments
    [[ -z "$_line" || "$_line" =~ ^[[:space:]]*# ]] && continue
    if [[ "$_line" =~ ^[[:space:]]*([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
      _k="${BASH_REMATCH[1]}"
      _v="${BASH_REMATCH[2]}"
      # strip optional surrounding quotes
      if   [[ "$_v" =~ ^\"(.*)\"$ ]]; then _v="${BASH_REMATCH[1]}"
      elif [[ "$_v" =~ ^\'(.*)\'$ ]]; then _v="${BASH_REMATCH[1]}"
      fi
      export "$_k=$_v"
    fi
  done < "$PROJECT/.env"
  unset _line _k _v
fi

is_running() { [[ -f "$1" ]] && kill -0 "$(cat "$1")" 2>/dev/null; }

start_qdb() {
  if ss -tln 2>/dev/null | grep -q ':9009\b'; then
    echo "[questdb] already listening on 9009"
    return 0
  fi
  "$QDB_HOME/bin/questdb.sh" start -d "$HOME/questdb/data" >/dev/null 2>&1 || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    ss -tln 2>/dev/null | grep -q ':9009\b' && { echo "[questdb] up"; return 0; }
    sleep 0.5
  done
  echo "[questdb] FAILED to start" >&2
  return 1
}

start_collector() {
  if is_running "$RUN/collector.pid"; then echo "[collector] already running"; return 0; fi
  RUST_LOG=info nohup "$PROJECT/target/release/collector" >"$LOGS/collector.log" 2>&1 &
  echo $! > "$RUN/collector.pid"
  echo "[collector] pid=$!"
}

start_funding() {
  if is_running "$RUN/funding.pid"; then echo "[funding_poller] already running"; return 0; fi
  RUST_LOG=info nohup "$PROJECT/target/release/funding_poller" >"$LOGS/funding.log" 2>&1 &
  echo $! > "$RUN/funding.pid"
  echo "[funding_poller] pid=$!"
}

start_paper() {
  # Strategies now live inside the collector process; toggle by restarting
  # the collector with PAPER_BOT=1. This avoids duplicate WS connections.
  if is_running "$RUN/collector.pid"; then
    kill "$(cat "$RUN/collector.pid")" 2>/dev/null || true
    while kill -0 "$(cat "$RUN/collector.pid")" 2>/dev/null; do :; done
  fi
  PAPER_BOT=1 RUST_LOG=info \
    nohup "$PROJECT/target/release/collector" >"$LOGS/collector.log" 2>&1 &
  echo $! > "$RUN/collector.pid"
  echo "[collector+strategies] pid=$! (PAPER_BOT=1)"
}

start_grafana() {
  if is_running "$RUN/grafana.pid"; then echo "[grafana] already running"; return 0; fi
  if [[ ! -x "$GRAFANA_HOME/bin/grafana" ]]; then
    echo "[grafana] binary missing at $GRAFANA_HOME — skip"; return 0
  fi
  GRAFANA_HOME="$GRAFANA_HOME" nohup "$PROJECT/scripts/start_grafana.sh" >"$LOGS/grafana.log" 2>&1 &
  echo $! > "$RUN/grafana.pid"
  echo "[grafana] pid=$! — http://127.0.0.1:3001 (admin/admin)"
}

stop_one() {
  local name=$1 pidfile=$2
  if is_running "$pidfile"; then
    kill "$(cat "$pidfile")" 2>/dev/null || true
    echo "[$name] stopped"
  else
    echo "[$name] not running"
  fi
  rm -f "$pidfile"
}

case "${1:-status}" in
  start)
    start_qdb
    start_collector
    start_funding
    start_grafana
    ;;
  paper-start)
    start_paper
    ;;
  stop)
    stop_one paper_bot       "$RUN/paper.pid"
    stop_one collector       "$RUN/collector.pid"
    stop_one funding_poller  "$RUN/funding.pid"
    stop_one grafana         "$RUN/grafana.pid"
    ;;
  restart)
    "$0" stop
    "$0" start
    ;;
  status)
    for f in "$RUN"/*.pid; do
      [[ -e "$f" ]] || continue
      n=$(basename "$f" .pid); p=$(cat "$f" 2>/dev/null || echo)
      if [[ -n "$p" ]] && kill -0 "$p" 2>/dev/null; then
        printf "  %-16s pid=%s %s\n" "$n" "$p" "$(ps -o etime= -p "$p" 2>/dev/null | xargs)"
      else
        printf "  %-16s DEAD\n" "$n"
      fi
    done
    ;;
  *)
    echo "usage: $0 {start|stop|restart|status|paper-start}"; exit 1 ;;
esac

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
#
# Live executor instances (subscribe to collector signals on IPC and
# place real Flipster orders):
#   ./control.sh gl-start  / gl-stop   — gate_lead (BINANCE_LEAD_v1)
#   ./control.sh sr-start  / sr-stop   — spread_revert (SR_LIVE_v1)
#
# Auth (one-time per machine):
#   ./control.sh vnc-start             — start Xvfb + x11vnc on :5901
#   ./control.sh chrome-start          — Flipster + Gate Chromes with CDP
#   then: VNC viewer → :5901 → log into both sites manually
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

# ---------------------------------------------------------------------------
# Live executor instances. Each one is a `target/release/executor` process
# subscribed to the collector via IPC and authorised to place real Flipster
# orders. Multiple executors can run side-by-side as long as their fill
# publisher socket paths don't collide.
# ---------------------------------------------------------------------------

start_gl() {
  if is_running "$RUN/executor_gl.pid"; then echo "[executor-gl] already running"; return 0; fi
  if [[ ! -x "$PROJECT/target/release/executor" ]]; then
    echo "[executor-gl] binary missing — run \`cargo build --release -p executor\`" >&2
    return 1
  fi
  # gate_lead live: BINANCE_LEAD_v1, $80, single-leg Flipster.
  # Default fill PUB on tcp://127.0.0.1:7501 (legacy) — collector's
  # fill_subscriber is on ipc:///tmp/flipster_kattpish_fill.sock, so set
  # FILL_PUB_ADDR to match unless overridden.
  : "${GL_SIZE_USD:=80}"
  RUST_LOG=info FILL_PUB_ADDR="${FILL_PUB_ADDR:-ipc:///tmp/flipster_kattpish_fill.sock}" \
    nohup "$PROJECT/target/release/executor" \
      --variant BINANCE_LEAD_v1 \
      --size-usd "$GL_SIZE_USD" \
      --flipster-only \
      --trade-log "$LOGS/binance_lead_live_v44.jsonl" \
      >"$LOGS/executor_v44.log" 2>&1 &
  echo $! > "$RUN/executor_gl.pid"
  echo "[executor-gl] pid=$! variant=BINANCE_LEAD_v1 size=\$$GL_SIZE_USD"
}

# ---------------------------------------------------------------------------
# Browser session (one per host). Runs an Xvfb display + x11vnc so the
# operator can connect a VNC viewer once, manually log into Flipster +
# Gate, and walk away. Sessions persist in user-data-dir across reboots.
# ---------------------------------------------------------------------------

VNC_DISPLAY=:1
VNC_PORT=5901

start_vnc() {
  if pgrep -f "Xvfb $VNC_DISPLAY" >/dev/null; then
    echo "[vnc] Xvfb $VNC_DISPLAY already running"
  else
    Xvfb "$VNC_DISPLAY" -screen 0 1920x1080x24 >/dev/null 2>&1 &
    echo "[vnc] Xvfb started on $VNC_DISPLAY"
  fi
  if pgrep -f "x11vnc.*-rfbport $VNC_PORT" >/dev/null; then
    echo "[vnc] x11vnc already on :$VNC_PORT"
  else
    if [[ ! -f "$HOME/.vnc/passwd" ]]; then
      echo "[vnc] no password yet — run: x11vnc -storepasswd"; exit 1
    fi
    x11vnc -display "$VNC_DISPLAY" -rfbauth "$HOME/.vnc/passwd" \
      -forever -bg -rfbport "$VNC_PORT" -quiet >/dev/null
    echo "[vnc] x11vnc bound to :$VNC_PORT (connect with VNC viewer)"
  fi
}

start_chrome() {
  start_vnc
  for spec in "flipster:9230:https://flipster.io/trade/perpetual/BTCUSDT.PERP" \
              "gate:9231:https://www.gate.com/futures/USDT/BTC_USDT"; do
    name="${spec%%:*}"; rest="${spec#*:}"
    port="${rest%%:*}"; url="${rest#*:}"
    if curl -fs "http://localhost:$port/json/version" >/dev/null 2>&1; then
      echo "[chrome-$name] CDP already alive on :$port"
      continue
    fi
    DISPLAY="$VNC_DISPLAY" google-chrome \
      --no-sandbox --disable-gpu \
      --remote-debugging-port="$port" \
      --remote-debugging-address=0.0.0.0 \
      --user-data-dir="/tmp/chrome-$name" \
      --window-size=1920,1080 "$url" \
      >/dev/null 2>&1 &
    echo "[chrome-$name] launched (CDP :$port, user-data-dir=/tmp/chrome-$name)"
  done
  echo
  echo "VNC: connect to <this-host>:$VNC_PORT, then log into Flipster + Gate."
  echo "After login, cookies refresh via the cron registered by setup.sh."
}

stop_chrome() {
  pkill -f "remote-debugging-port=9230" 2>/dev/null || true
  pkill -f "remote-debugging-port=9231" 2>/dev/null || true
  echo "[chrome] killed flipster + gate sessions (Xvfb left running)"
}

start_sr() {
  if is_running "$RUN/executor_sr.pid"; then echo "[executor-sr] already running"; return 0; fi
  if [[ ! -x "$PROJECT/target/release/executor" ]]; then
    echo "[executor-sr] binary missing — run \`cargo build --release -p executor\`" >&2
    return 1
  fi
  # spread_revert live: SR_LIVE_v1, $5, Isolated margin, MULTIPLE_POSITIONS.
  # Distinct fill_publisher socket so it doesn't bind-collide with gl.
  : "${SR_LIVE_SIZE_USD:=5}"
  RUST_LOG=info FILL_PUB_ADDR="ipc:///tmp/flipster_kattpish_fill_sr.sock" \
    nohup "$PROJECT/target/release/executor" \
      --variant SR_LIVE_v1 \
      --size-usd "$SR_LIVE_SIZE_USD" \
      --flipster-only \
      --margin Isolated \
      --trade-mode MULTIPLE_POSITIONS \
      --trade-log "$LOGS/spread_revert_live_v1.jsonl" \
      >"$LOGS/executor_sr_v1.log" 2>&1 &
  echo $! > "$RUN/executor_sr.pid"
  echo "[executor-sr] pid=$! variant=SR_LIVE_v1 size=\$$SR_LIVE_SIZE_USD margin=Isolated multi=on"
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
  gl-start)
    start_gl
    ;;
  gl-stop)
    stop_one executor-gl "$RUN/executor_gl.pid"
    ;;
  sr-start)
    start_sr
    ;;
  sr-stop)
    stop_one executor-sr "$RUN/executor_sr.pid"
    ;;
  vnc-start)
    start_vnc
    ;;
  chrome-start)
    start_chrome
    ;;
  chrome-stop)
    stop_chrome
    ;;
  cookies-now)
    python3 "$PROJECT/scripts/dump_cookies.py" || exit $?
    ;;
  stop)
    stop_one executor-sr     "$RUN/executor_sr.pid"
    stop_one executor-gl     "$RUN/executor_gl.pid"
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
    echo "usage: $0 {start|stop|restart|status|paper-start|gl-start|gl-stop|sr-start|sr-stop|vnc-start|chrome-start|chrome-stop|cookies-now}"; exit 1 ;;
esac

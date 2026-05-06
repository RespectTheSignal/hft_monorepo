#!/usr/bin/env bash
# Multi-exchange bookticker publisher control script.
#
# Each binary writes to <exchange>_bookticker on the central QuestDB
# (default 211.181.122.102:9009 ILP).  Override with QDB_HOST / QDB_ILP_PORT.
#
# Usage:
#   ./scripts/publishers.sh start [exchange ...]
#   ./scripts/publishers.sh stop  [exchange ...]
#   ./scripts/publishers.sh status
#   ./scripts/publishers.sh tail <exchange>
#
#   exchange ∈ {okx,backpack,kucoin,bingx,mexc,hashkey,kraken,all}

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
LOG_DIR="$ROOT/logs"
PID_DIR="$ROOT/run"
mkdir -p "$LOG_DIR" "$PID_DIR"

ALL=(okx backpack kucoin bingx mexc hashkey kraken pyth binance_trade binance_depth)

resolve_targets() {
    if [ "$#" -eq 0 ] || [ "$1" = "all" ]; then
        echo "${ALL[@]}"
    else
        echo "$@"
    fi
}

bin_path() {
    echo "$ROOT/target/release/${1}_publisher"
}

start_one() {
    local ex="$1"
    local bin pid_file log_file
    bin="$(bin_path "$ex")"
    pid_file="$PID_DIR/${ex}.pid"
    log_file="$LOG_DIR/${ex}.log"
    if [ ! -x "$bin" ]; then
        echo "missing binary: $bin (run: cargo build --release --bin ${ex}_publisher)"
        return 1
    fi
    if [ -f "$pid_file" ] && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
        echo "[$ex] already running (pid $(cat "$pid_file"))"
        return 0
    fi
    nohup "$bin" >> "$log_file" 2>&1 &
    echo $! > "$pid_file"
    echo "[$ex] started pid $(cat "$pid_file") → $log_file"
}

stop_one() {
    local ex="$1"
    local pid_file="$PID_DIR/${ex}.pid"
    if [ ! -f "$pid_file" ]; then
        echo "[$ex] not running"
        return 0
    fi
    local pid; pid="$(cat "$pid_file")"
    if kill -0 "$pid" 2>/dev/null; then
        kill "$pid"
        echo "[$ex] sent SIGTERM to pid $pid"
        for _ in 1 2 3 4 5; do
            kill -0 "$pid" 2>/dev/null || break
            sleep 1
        done
        kill -0 "$pid" 2>/dev/null && kill -9 "$pid" || true
    fi
    rm -f "$pid_file"
}

status() {
    for ex in "${ALL[@]}"; do
        local pid_file="$PID_DIR/${ex}.pid"
        if [ -f "$pid_file" ] && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
            printf "  %-10s running (pid %s)\n" "$ex" "$(cat "$pid_file")"
        else
            printf "  %-10s stopped\n" "$ex"
        fi
    done
}

cmd="${1:-status}"
shift || true

case "$cmd" in
    start)
        for ex in $(resolve_targets "$@"); do start_one "$ex"; done
        ;;
    stop)
        for ex in $(resolve_targets "$@"); do stop_one "$ex"; done
        ;;
    restart)
        for ex in $(resolve_targets "$@"); do stop_one "$ex"; start_one "$ex"; done
        ;;
    status)
        status
        ;;
    tail)
        ex="${1:?usage: tail <exchange>}"
        tail -n 200 -f "$LOG_DIR/${ex}.log"
        ;;
    *)
        echo "usage: $0 {start|stop|restart|status|tail} [exchange ...]"
        echo "exchanges: ${ALL[*]} all"
        exit 1
        ;;
esac

#!/usr/bin/env bash
# Watchdog: monitors executor log for CF 1015 (HTTP 429) bursts,
# automatically rotates teamreporter VM IP through a pool, restarts
# SSH reverse tunnel + executor.
set -uo pipefail

# IP pool. Watchdog skips recently-used IPs (last RECENT_HISTORY_LEN swaps)
# so it lands on a fresh one quickly even if multiple are in CF cooldown.
IPS=(
  "211.181.122.105"
  "211.181.122.125"
  "211.181.122.126"
  "211.181.122.127"
)
CURRENT_IP="${CURRENT_IP:-211.181.122.105}"
RECENT_HISTORY_LEN=2   # avoid last N IPs (with 4-IP pool, max 2 to keep options)
RECENT_IPS=()          # history of recently-used IPs

LOG_DIR="/home/teamreporter/projects/quant/flipster_kattpish/logs"

# Tunable — aggressive: swap immediately on first sign of CF block
ERR_THRESHOLD=3        # 429 errors in window — low so we react fast
WINDOW_S=10
COOLDOWN_AFTER_SWAP_S=10   # just long enough to verify swap landed
SETUP_WAIT_S=10        # wait for new IP to come up; retry loop covers slow case
EXEC_BASE_SIZE=80

# Hard-coded exec command — avoids the nested-bash-capture bug we hit
# when grabbing it from `ps -p ... -o args=`.
EXEC_CMD_TPL='./executor_bin --variant BINANCE_LEAD_v1 --size-usd %SIZE% --flipster-only --trade-log logs/binance_lead_live_v%TS%.jsonl'

trap 'echo "[watchdog] stop"; exit 0' SIGTERM SIGINT

log() { echo "[$(date -u +%FT%TZ)] $*"; }

count_429_recent() {
  ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no teamreporter@"$CURRENT_IP" \
    "L=\$(ls -t $LOG_DIR/executor*.log 2>/dev/null | head -1); \
     [ -z \"\$L\" ] && echo 0 && exit; \
     tail -2000 \$L | sed 's/\\x1b\\[[0-9;]*m//g' | \
     awk -v t=\$(date -u -d '${WINDOW_S} sec ago' +%FT%TZ) '\$1 >= t && /429/' | wc -l" \
    2>/dev/null || echo 0
}

next_ip() {
  # Round-robin from CURRENT_IP, skipping any in RECENT_IPS list. If all
  # are recent, take the oldest (whatever round-robin gives).
  local n=${#IPS[@]}
  local i=0
  local start=-1
  for ip in "${IPS[@]}"; do
    [[ "$ip" == "$CURRENT_IP" ]] && start=$i
    ((i++))
  done
  [[ $start -lt 0 ]] && start=0
  for ((k=1; k<=n; k++)); do
    local idx=$(( (start + k) % n ))
    local cand="${IPS[$idx]}"
    local skip=false
    for r in "${RECENT_IPS[@]:-}"; do
      [[ "$cand" == "$r" ]] && skip=true && break
    done
    [[ $skip == false ]] && echo "$cand" && return
  done
  # All recent — fall through to round-robin oldest
  echo "${IPS[$(( (start + 1) % n ))]}"
}

remember_recent() {
  RECENT_IPS=("$1" "${RECENT_IPS[@]}")
  while [[ ${#RECENT_IPS[@]} -gt $RECENT_HISTORY_LEN ]]; do
    unset 'RECENT_IPS[-1]'
  done
}

swap_ip() {
  local OLD_IP="$CURRENT_IP"
  local NEW_IP
  NEW_IP=$(next_ip)
  log "swap: $OLD_IP → $NEW_IP (recent: ${RECENT_IPS[*]:-})"

  # Stop executor
  ssh teamreporter@"$OLD_IP" 'pkill -TERM -f executor_bin 2>/dev/null; sleep 1; pkill -9 -f executor_bin 2>/dev/null' || true
  sleep 1

  # Schedule IP change (runs after SSH disconnects)
  ssh teamreporter@"$OLD_IP" \
    "nohup bash -c '(sleep 2 && sudo nmcli con mod \"Wired connection 1\" ipv4.addresses ${NEW_IP}/23 && sudo nmcli con down \"Wired connection 1\" && sleep 2 && sudo nmcli con up \"Wired connection 1\") &' > /tmp/ipswap.log 2>&1 || true"
  log "IP-change scheduled, sleep ${SETUP_WAIT_S}s..."
  sleep "$SETUP_WAIT_S"

  # Wait for NEW_IP to be reachable
  local reachable=false
  for i in $(seq 1 12); do
    if ssh -o ConnectTimeout=4 -o StrictHostKeyChecking=no teamreporter@"$NEW_IP" 'echo ok' >/dev/null 2>&1; then
      log "$NEW_IP up (attempt $i)"
      reachable=true
      break
    fi
    sleep 5
  done
  if [[ $reachable == false ]]; then
    log "ERROR: $NEW_IP never came up — keeping CURRENT_IP=$OLD_IP, will retry"
    return 1
  fi

  remember_recent "$OLD_IP"
  CURRENT_IP="$NEW_IP"

  # Kill old SSH tunnel + clean up port 7500 on new host
  pkill -f "ssh.*-R.*7500" 2>/dev/null || true
  sleep 1
  ssh teamreporter@"$NEW_IP" 'fuser -k 7500/tcp 2>/dev/null; sleep 1; ss -tln | grep ":7500" >/dev/null && (kill -9 $(pgrep -af "sshd: teamreporter" | head -3 | awk "{print \$1}") 2>/dev/null; sleep 1)' || true

  # New tunnel
  if ! ssh -f -N -R 7500:127.0.0.1:7500 -o ServerAliveInterval=30 -o ExitOnForwardFailure=yes teamreporter@"$NEW_IP"; then
    log "ERROR: tunnel setup failed"
    return 1
  fi
  log "tunnel up to $NEW_IP"

  # Restart executor
  local TS=$(date -u +%H%M%S)
  local CMD=${EXEC_CMD_TPL//%SIZE%/$EXEC_BASE_SIZE}
  CMD=${CMD//%TS%/swap_$TS}
  ssh teamreporter@"$NEW_IP" \
    "cd /home/teamreporter/projects/quant/flipster_kattpish && GL_MAKER_EXIT=0 nohup $CMD > logs/executor_swap_${TS}.log 2>&1 < /dev/null & disown" \
    && log "executor relaunched (log=executor_swap_${TS}.log)"

  sleep "$COOLDOWN_AFTER_SWAP_S"
}

log "watchdog start, current=$CURRENT_IP, threshold=$ERR_THRESHOLD/${WINDOW_S}s, pool=${#IPS[@]} IPs"

while true; do
  N=$(count_429_recent)
  if [[ "$N" =~ ^[0-9]+$ ]] && (( N >= ERR_THRESHOLD )); then
    log "429 burst: $N in last ${WINDOW_S}s — rotating"
    if ! swap_ip; then
      log "swap failed, retry in 15s"
      sleep 15
    fi
  fi
  sleep 3
done

#!/usr/bin/env bash
# Start the bingx_executor — connects to the SAME ZMQ signal socket as the
# live paper collector (BX_LIVE_v1 variant) and either dry-runs or fires
# real BingX market orders.
#
# Usage:
#   BINGX_DRY_RUN=1 ./scripts/start_bingx_executor.sh   # safe shadow
#   ./scripts/start_bingx_executor.sh                    # LIVE
#
# Required env (recommended: put in ~/.config/flipster_kattpish/bingx.env
# and source before invoking):
#   BINGX_USER_ID  — numeric account id, e.g. 1507939247354732548
#   BINGX_JWT      — bearer token (~7 day expiry)
#
# Optional:
#   BINGX_VARIANT       (default BX_LIVE_v1)
#   BINGX_SIZE_USD      (default 20)
#   BINGX_MAX_OPEN      (default 5)
#   BINGX_DAILY_LOSS_USD (default -50, kill switch)
#   BINGX_DRY_RUN=1     (no API calls, log only)
set -euo pipefail
PROJECT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT"

# Load .env (project) and a private bingx.env if present.
set -a
if [ -f .env ]; then
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
    if [[ "$line" =~ ^[[:space:]]*([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
      export "${BASH_REMATCH[1]}=${BASH_REMATCH[2]}"
    fi
  done < .env
fi
PRIV="${HOME}/.config/flipster_kattpish/bingx.env"
[ -f "$PRIV" ] && source "$PRIV"
set +a

# Live paper collector binds to the same SIGNAL socket addr by default.
export SIGNAL_PUB_ADDR="${SIGNAL_PUB_ADDR:-ipc:///tmp/flipster_kattpish_signal_live.sock}"
# Fill feedback PUB — collector's bingx_lead subscribes here (via FILL_SUB_ADDR
# in live_paper_bingx.sh) so it can retune ref_mid to the actual maker fill
# price. Without it the strategy's TP threshold is computed against the
# signal-time mid and ignores the half-spread head start of maker entry.
export FILL_PUB_ADDR="${FILL_PUB_ADDR:-ipc:///tmp/flipster_kattpish_fill_live.sock}"

LOG="$PROJECT/logs/bingx_executor.log"
mkdir -p "$PROJECT/logs"

if [ -z "${BINGX_USER_ID:-}" ] || [ -z "${BINGX_JWT:-}" ]; then
  echo "ERROR: BINGX_USER_ID and BINGX_JWT required (export or place in ~/.config/flipster_kattpish/bingx.env)" >&2
  exit 1
fi

DRY="${BINGX_DRY_RUN:-0}"
echo "[bingx-exec] variant=${BINGX_VARIANT:-BX_LIVE_v1} size=${BINGX_SIZE_USD:-20} dry=$DRY signal=$SIGNAL_PUB_ADDR"
echo "[bingx-exec] log=$LOG"
# Detach-friendly: when this script is invoked under nohup ... >> $LOG,
# the previous `tee -a "$LOG"` doubled every log line. Plain exec leaves
# the parent's redirection in charge — single-write.
exec ./target/release/bingx_executor 2>&1

#!/usr/bin/env bash
# Launch a dedicated Chrome instance for BingX with CDP enabled on :9232.
#
# Pattern mirrors the existing Flipster Chrome (:9230) and Gate Chrome (:9231)
# instances — separate user-data-dir so sessions don't collide. After the
# browser is up, log in to bingx.com via the VNC display, then run
# scripts/dump_cookies.py to extract session cookies for the executor.
#
# Usage:
#   ./scripts/start_bingx_chrome.sh        # foreground
#   nohup ./scripts/start_bingx_chrome.sh > /tmp/bingx-chrome.log 2>&1 &
#
# Stop with: pkill -f 'remote-debugging-port=9232'
set -euo pipefail

PORT=${BINGX_CDP_PORT:-9232}
DATA_DIR=${BINGX_CHROME_DIR:-/tmp/chrome-bingx}
DISPLAY_NUM=${DISPLAY:-:1}

# Reuse google-chrome if installed; fall back to chromium if not.
if command -v google-chrome >/dev/null 2>&1; then
  CHROME=google-chrome
elif command -v chromium >/dev/null 2>&1; then
  CHROME=chromium
elif [ -x /opt/google/chrome/chrome ]; then
  CHROME=/opt/google/chrome/chrome
else
  echo "no Chrome/Chromium binary found" >&2
  exit 1
fi

mkdir -p "$DATA_DIR"

echo "[bingx-chrome] port=$PORT data-dir=$DATA_DIR display=$DISPLAY_NUM"
echo "[bingx-chrome] after launch:"
echo "  1. VNC into $DISPLAY_NUM (e.g. via :6090 web vnc)"
echo "  2. log in to https://bingx.com"
echo "  3. run: python3 scripts/dump_cookies.py"
echo "  4. confirm cookies.json has 'bingx' key with session cookies"

# Run Chrome (foreground — the launcher script keeps it attached for easy
# restart). --no-sandbox required when running as the gate1 user without a
# user namespace.  --lang=ko mirrors the Flipster/Gate setups.  --start-maximized
# helps the VNC user see the trade UI without resizing.
DISPLAY=$DISPLAY_NUM exec "$CHROME" \
  --remote-debugging-port="$PORT" \
  --user-data-dir="$DATA_DIR" \
  --no-sandbox \
  --no-first-run \
  --disable-features=Translate \
  --start-maximized \
  --lang=ko \
  https://bingx.com

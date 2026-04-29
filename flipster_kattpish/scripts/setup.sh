#!/usr/bin/env bash
# One-shot setup for a fresh machine.
#
#   ./scripts/setup.sh
#
# Installs system deps, builds the Rust binaries, registers the cookie
# dumper cron, and prints what's left for you (manual VNC login).
#
# Idempotent — safe to re-run.
set -euo pipefail

PROJECT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT"

echo "==[1/4] system deps =="
SUDO=sudo
if ! command -v sudo >/dev/null; then SUDO=""; fi
$SUDO apt-get update -qq
# google-chrome-stable: install via google's apt repo if missing
if ! command -v google-chrome >/dev/null; then
  curl -fsSL https://dl.google.com/linux/linux_signing_key.pub \
    | $SUDO gpg --dearmor -o /etc/apt/trusted.gpg.d/google.gpg
  echo "deb [arch=amd64] http://dl.google.com/linux/chrome/deb/ stable main" \
    | $SUDO tee /etc/apt/sources.list.d/google-chrome.list >/dev/null
  $SUDO apt-get update -qq
fi
$SUDO apt-get install -y -qq \
  google-chrome-stable x11vnc xvfb tigervnc-standalone-server \
  python3 python3-pip jq curl
pip3 install --quiet --break-system-packages websockets python-dotenv 2>/dev/null \
  || pip3 install --quiet websockets python-dotenv

echo "==[2/4] rust toolchain =="
if ! command -v cargo >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
  source "$HOME/.cargo/env"
fi

echo "==[3/4] build =="
cargo build --release -p collector -p executor

echo "==[4/4] cron for cookie dump =="
CRON_LINE="*/20 * * * * cd $PROJECT && python3 scripts/dump_cookies.py >>/tmp/dump_cookies.log 2>&1"
( crontab -l 2>/dev/null | grep -v "scripts/dump_cookies.py" ; echo "$CRON_LINE" ) | crontab -
echo "  cron registered: every 20 min"

mkdir -p run logs ~/.config/flipster_kattpish
[ -f .env ] || { echo "  ⚠️  .env missing — copy from another host (chmod 600)" >&2; }

cat <<EOF

================================================================
SETUP DONE.

Next:

  1) Bring up Chrome + VNC for manual login:
       ./scripts/control.sh chrome-start

  2) Connect a VNC viewer to <this-host>:5901 with the password you
     set in ~/.vnc/passwd (run \`x11vnc -storepasswd\` if not set).
     Click through the Flipster + Gate logins inside the viewer.
     Sessions persist across reboots in /tmp/chrome-{flipster,gate}.

  3) Once logged in, start the trading stack:
       ./scripts/control.sh start
       ./scripts/control.sh paper-start
       ./scripts/control.sh gl-start          # gate_lead live
       ./scripts/control.sh sr-start          # spread_revert live

  Status anytime:  ./scripts/control.sh status
  Stop everything: ./scripts/control.sh stop  (Chrome stays up)

================================================================
EOF

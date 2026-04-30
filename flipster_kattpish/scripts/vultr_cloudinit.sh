#!/usr/bin/env bash
# Cloud-init user-data for Vultr proxy instances. Runs once on first
# boot via Vultr's user-data hook (cloud-init runs anything starting
# with #!). Mirrors install_proxy.sh but self-contained so we don't
# need a separate SSH provisioning step.
set -e

PROXY_USER="fl"
PROXY_PASS="kxQp4r9tAmZ"
GATE1_IP="175.211.27.121"

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq tinyproxy ufw curl

cat >/etc/tinyproxy/tinyproxy.conf <<EOF
User tinyproxy
Group tinyproxy
Port 3128
Listen 0.0.0.0
Timeout 60
DefaultErrorFile "/usr/share/tinyproxy/default.html"
StatFile "/usr/share/tinyproxy/stats.html"
LogFile "/var/log/tinyproxy/tinyproxy.log"
LogLevel Info
PidFile "/run/tinyproxy/tinyproxy.pid"
MaxClients 100
Allow $GATE1_IP
BasicAuth $PROXY_USER $PROXY_PASS
ConnectPort 443
ConnectPort 80
EOF

systemctl enable --now tinyproxy
systemctl restart tinyproxy
sleep 1

ufw allow ssh >/dev/null 2>&1 || true
ufw allow from $GATE1_IP to any port 3128 proto tcp >/dev/null 2>&1 || true
echo y | ufw enable >/dev/null 2>&1 || true

# Mark cloud-init complete for our health-check
touch /var/log/proxy_ready

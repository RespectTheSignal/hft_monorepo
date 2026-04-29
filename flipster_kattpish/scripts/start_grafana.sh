#!/usr/bin/env bash
# Start Grafana with the project's provisioning tree. Default port 3002 (3000
# and 3001 are taken by other node apps on this host); override via env if needed.
set -euo pipefail

GRAFANA_HOME="${GRAFANA_HOME:-$HOME/grafana/grafana-12.4.2}"
PROJECT="${PROJECT:-$HOME/projects/flipster}"

export GF_PATHS_HOME="$GRAFANA_HOME"
export GF_PATHS_DATA="$PROJECT/grafana/data"
export GF_PATHS_LOGS="$PROJECT/grafana/data/logs"
export GF_PATHS_PLUGINS="$PROJECT/grafana/data/plugins"
export GF_PATHS_PROVISIONING="$PROJECT/grafana/provisioning"

export GF_SERVER_HTTP_PORT="${GF_SERVER_HTTP_PORT:-3002}"
export GF_SECURITY_ADMIN_USER=admin
export GF_SECURITY_ADMIN_PASSWORD=admin
export GF_ANALYTICS_REPORTING_ENABLED=false
export GF_ANALYTICS_CHECK_FOR_UPDATES=false
export GF_AUTH_ANONYMOUS_ENABLED=true
export GF_AUTH_ANONYMOUS_ORG_ROLE=Viewer

mkdir -p "$GF_PATHS_DATA" "$GF_PATHS_LOGS" "$GF_PATHS_PLUGINS"

exec "$GRAFANA_HOME/bin/grafana" server \
  --homepath "$GRAFANA_HOME" \
  --config "$GRAFANA_HOME/conf/defaults.ini"

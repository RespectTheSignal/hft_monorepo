#!/usr/bin/env bash
set -euo pipefail

cd /home/gate1/projects/quant/hft_monorepo/strategy_flipster_python

set -a
source .env
set +a

DRY_RUN=0 \
STRATEGY=basis_meanrev \
CANONICALS=ETH \
MAX_POSITION=50 \
OPEN_ORDER_SIZE=10 \
CLOSE_ORDER_SIZE=20 \
PORTFOLIO_MAX=50 \
PYTHONPATH=src .venv/bin/python -m strategy_flipster.main config.toml

#!/usr/bin/env bash
# Start a live-paper bingx_lead bot.
#
# Reads tick stream from central QuestDB (READ_FROM_QUESTDB=1), no live WS.
# Whitelist + blacklist applied. Position log tagged mode='bingx_live_paper_v1'.
# Logs to logs/bingx_live.log.
#
# Run in foreground:
#   ./scripts/live_paper_bingx.sh
# Or detach:
#   nohup ./scripts/live_paper_bingx.sh > /dev/null 2>&1 &
#
# Stop with:
#   pkill -f 'release/collector.*BINGX_LEAD'
set -euo pipefail
PROJECT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT"

LOG="$PROJECT/logs/bingx_live.log"
mkdir -p "$PROJECT/logs"

# Load .env for QDB_HOST etc.
set -a
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
  if [[ "$line" =~ ^[[:space:]]*([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
    export "${BASH_REMATCH[1]}=${BASH_REMATCH[2]}"
  fi
done < .env
set +a

# Strategy gate: only bingx_lead.
export PAPER_BOT=1
export GATE_LEAD=0
export HL_LEAD=0
export MEXC_LEAD=0
export BINGX_LEAD=1
export SPREAD_REVERT=0
export HL_SPREAD_REVERT=0

# Live (not backtest)
unset BACKTEST_START_TS BACKTEST_END_TS BACKTEST_TAG
unset BACKTEST_BINGX_SWEEP BACKTEST_BINGX_GRID

# Tick source: central QuestDB poll for follower exchanges, but
# Binance WS direct for the leader. The data_publisher → QDB write
# step adds ~50-200ms of latency we don't need on the signal venue.
# Direct WS shaves that off, which directly improves limit-fill rate
# and trade alpha for lead-lag strategies.
export USE_ZMQ=0
export READ_FROM_QUESTDB=1
export BINANCE_WS_DIRECT=1
# BingX WS direct: same idea — pull BingX bookticker live instead of polling
# the central QuestDB (which adds 50-100ms over WAN from SG → KR). Without
# this, strategy sees fresh Binance vs stale BingX → lag filter rejects
# most signals.
export BINGX_WS_DIRECT=1
# Subset to the strategy's whitelist + a few high-volatility alts for
# universe coverage. Each chunk of 10 = 1 WS connection. Empty value
# falls back to full Binance USDT-perp universe (~500 symbols, ~50 WS).
export BINANCE_WS_SYMBOLS="LABUSDT,UBUSDT,DASHUSDT,BIOUSDT,DUSDT,BRUSDT,GUAUSDT,KOMAUSDT,MERLUSDT,SIRENUSDT,REZUSDT,RLCUSDT,ONUSDT,PENDLEUSDT,DUSKUSDT,HUSDT,PTBUSDT,BROCCOLIF3BUSDT,GENIUSUSDT,TAKEUSDT,GWEIUSDT,HEMIUSDT,EVAAUSDT,CATIUSDT,TRIAUSDT,SHELLUSDT,RAVEUSDT,CHRUSDT,TOWNSUSDT,FORMUSDT,LYNUSDT,SAPIENUSDT,PIEVERSEUSDT,IRYSUSDT,ELSAUSDT,SQDUSDT,GRASSUSDT,ICNTUSDT,XNYUSDT,ORDIUSDT,HIGHUSDT,HANAUSDT,GRIFFAINUSDT,BEATUSDT,FLOCKUSDT,ONDOUSDT,KGENUSDT,PENGUUSDT,JTOUSDT,BASUSDT,BANANAS31USDT,SPXUSDT,PNUTUSDT,FARTCOINUSDT,BTCUSDT,ETHUSDT,SOLUSDT,HYPEUSDT,BNBUSDT,XRPUSDT,DOGEUSDT,LINKUSDT,AVAXUSDT,TONUSDT,SUIUSDT,APTUSDT,TIAUSDT,TAOUSDT,WIFUSDT,POPCATUSDT,AAVEUSDT,UNIUSDT,LTCUSDT,BCHUSDT,ADAUSDT,DOTUSDT,NEARUSDT,ATOMUSDT,TRUMPUSDT,NVDAUSDT,TSLAUSDT,AAPLUSDT,AMZNUSDT,METAUSDT,MSFTUSDT,GOOGLUSDT,AMDUSDT,MUUSDT,INTCUSDT,SPYUSDT,QQQUSDT,ORCLUSDT,AVGOUSDT,SNDKUSDT,COINUSDT,HOODUSDT,PLTRUSDT,MSTRUSDT,QCOMUSDT,TSMUSDT,XAUUSDT,XAGUSDT,XPDUSDT,XPTUSDT"

# Strategy params — picked from BACKTEST_BINGX_GRID sweep (2026-05-06).
# (anchor_s=2, exit_bp=7) was the top variant: +5.93 bp avg, 64% win,
# +$3,708 / 24h on $200 size — vs the prior (3, 5) at +3.14 bp, +$2,327.
# Faster anchor catches Binance shocks while still fresh; wider TP rides
# more of the catch-up move.
export GL_MIN_MOVE_BP=30
export GL_ANCHOR_S=2
export GL_EXIT_BP=7
export GL_STOP_BP=8
export GL_HOLD_MAX_S=2
export GL_COOLDOWN_S=3
export GL_SIZE_USD=200
export GL_ACCOUNT_ID=BX_LIVE_v1

# BINANCE_WS_DIRECT makes Binance quotes ~200ms fresher than BingX (20ms QDB
# poll). The default lag filter (min_lag_bp=0) interprets that diff as
# "BingX already ahead" and skips ~99% of signals. Allow a small negative
# lag so the strategy's freshness assumption survives the WS-vs-poll skew.
# With BOTH Binance WS direct AND BingX WS direct, the WS skew that
# justified -5 vanishes — both feeds are fresh. Restore default 0 so the
# strategy can again use lag direction as a signal-quality gate (only
# trade when BingX is genuinely BEHIND Binance in the move's direction).
export GL_MIN_LAG_BP=0.0

# Velocity filter — restore default 8 bp now that BingX is fresh too.
# With both feeds tight, "BingX already moved 8bp/500ms in our direction"
# is a real "lag is gone" signal worth filtering on.
export GL_FLIP_VEL_MAX_BP=8.0

# Coord wide-spread filter: pairs_core default is 8 bp, calibrated for
# Flipster majors. BingX wide-spread alts (LAB, TRIA, BR averaged
# −24..−60 bp net in live test on 2026-05-06) bleed to crossing-cost on
# every market entry+exit pair. 10 bp keeps the strategy in symbols where
# 3.2 bp taker fee + ~5 bp crossing has any chance of being beaten by
# the +5.93 bp/trade backtest edge. Below 8 = old default; above 12 =
# all the chop alts come back in. 10 is the conservative middle.
export SYM_STATS_SPREAD_THRESHOLD_BP=10.0

# LIGHTER_LEAD parallel paper variant. Lighter has 0/0 fees + 175 markets
# (majors-heavy) so we use lower thresholds: 10bp move trigger / 3bp TP /
# 5bp stop. Listens on the Binance leader, fires entries on Lighter ticks.
# account_id=LH_PAPER_v1 — separate from BX_LIVE_v1 so the lighter_executor
# (paper mode) only consumes its own signals.
export LIGHTER_LEAD=1
# v2: hypothesis-driven config from 2026-05-06 cross-correlation analysis.
# Binance leads Lighter by ~1s with corr +0.55-0.65 on 6 specific alts:
# AVAX, LINK, SUI, WIF, POPCAT, BNB. (BTC/ETH/SOL/HYPE/PENGU showed
# simultaneous corr 0.6-0.8 → no exploitable lag.) The 1s lag = ~1-3bp
# move on Lighter, so threshold is set low (5bp Binance shock) and exits
# are tight (2bp TP, 4bp stop). 0/0 fees on Lighter means even +1bp net
# is profitable — but spread crossing is real, so we still need positive
# bp moves to clear it.
export GL_LH_ACCOUNT_ID=LH_PAPER_v2
export GL_LH_SIZE_USD=20
export GL_LH_MIN_MOVE_BP=5
export GL_LH_EXIT_BP=2
export GL_LH_STOP_BP=4
export GL_LH_ANCHOR_S=1
export GL_LH_HOLD_MAX_S=3
export GL_LH_COOLDOWN_S=2
export GL_LH_WHITELIST=AVAX,LINK,SUI,WIF,POPCAT,BNB
# We also need the Lighter WS feed live so the strategy sees follower ticks.
export LIGHTER_WS_DIRECT=1

# Whitelist (54 winners) + blacklist (15 losers) from bingx_24h_v1 backtest
export GL_WHITELIST="LAB,UB,DASH,BIO,D,BR,GUA,KOMA,MERL,SIREN,REZ,RLC,ON,PENDLE,DUSK,H,PTB,BROCCOLIF3B,GENIUS,TAKE,GWEI,HEMI,EVAA,CATI,TRIA,SHELL,RAVE,CHR,TOWNS,FORM,LYN,SAPIEN,PIEVERSE,IRYS,ELSA,SQD,GRASS,ICNT,XNY,ORDI,HIGH,HANA,GRIFFAIN,BEAT,FLOCK,ONDO,KGEN,PENGU,JTO,BAS,BANANAS31,SPX,PNUT,FARTCOIN"
export GL_BLACKLIST="4,B,SKYAI,AIOT,GIGGLE,IDOL,MEGA,MUBARAK,STBL,Q,IN,MAV,MINA,SOLV,BAT"

# Mode tag for position_log filtering
export GL_MODE_TAG=bingx_live_paper_v1

# Distinct IPC sockets so we don't collide with backtests
export SIGNAL_PUB_ADDR="ipc:///tmp/flipster_kattpish_signal_live.sock"
export FILL_SUB_ADDR="ipc:///tmp/flipster_kattpish_fill_live.sock"

echo "[live] starting bingx_lead live paper, log=$LOG"
echo "[live] whitelist size: $(echo $GL_WHITELIST | tr ',' '\n' | wc -l)"
echo "[live] blacklist size: $(echo $GL_BLACKLIST | tr ',' '\n' | wc -l)"
exec ./target/release/collector 2>&1 | tee "$LOG"

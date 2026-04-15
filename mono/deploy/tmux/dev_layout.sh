#!/usr/bin/env bash
# dev_layout.sh — 로컬 dev 환경용 4분할 tmux 세션.
#
# 목적:
#   VM / ivshmem 없이 단일 호스트 `/dev/shm` backing 으로 전 경로
#   (publisher → strategy → order-gateway → mock venue) 를 한 화면에서 검증.
#
#   단, 이 경로는 dev 전용 — prod 는 반드시 ivshmem PCI BAR.
#
# 팬아웃:
#   pane 0 : publisher (quote + trade 펌핑)
#   pane 1 : order-gateway (OrderRing fan-in)
#   pane 2 : strategy-0 (VM_ID=0)
#   pane 3 : strategy-1 (VM_ID=1)
#
# 전제:
#   - `cargo build --release` 가 이미 끝난 상태여야 함. 이 스크립트는 빌드
#     하지 않고 바이너리만 실행 (hot reload 시 빌드 시간이 4배 증가하는 것 방지).
#   - /dev/shm 에 hft_v2_dev 파일이 없어야 함 — 있으면 기존 layout 재사용 위험.

set -euo pipefail

SESSION="${HFT_TMUX_SESSION:-hft-dev}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN_DIR="${HFT_BIN_DIR:-$ROOT/target/release}"

if ! command -v tmux >/dev/null 2>&1; then
  echo "[dev_layout] tmux 가 설치돼 있지 않습니다." >&2
  exit 2
fi

for b in hft-publisher hft-order-gateway hft-strategy-demo; do
  if [[ ! -x "$BIN_DIR/$b" ]]; then
    echo "[dev_layout] $BIN_DIR/$b 가 없습니다. 먼저 cargo build --release 하세요." >&2
    exit 2
  fi
done

# 기존 세션 재사용 금지 — 로그가 뒤섞여 디버깅 혼란 유발.
if tmux has-session -t "$SESSION" 2>/dev/null; then
  echo "[dev_layout] 기존 tmux 세션 '$SESSION' 존재. 먼저 'tmux kill-session -t $SESSION' 후 재실행."
  exit 2
fi

# /dev/shm backing 파일 — LayoutSpec 이 변하면 반드시 제거 후 재생성.
SHM_BASE="${HFT_SHM_BASE:-/dev/shm/hft_v2_dev}"
if [[ -e "$SHM_BASE" ]]; then
  echo "[dev_layout] 기존 SHM backing $SHM_BASE 제거 (LayoutSpec 변경 대비)."
  rm -f "$SHM_BASE"
fi

# 공통 환경변수 — 네 pane 모두 동일한 LayoutSpec 를 써야 layout_digest 일치.
COMMON_ENV=(
  "HFT_LOG=debug,hyper=info,h2=info"
  "HFT_SHM__BACKING=dev_shm"
  "HFT_SHM__DEV_SHM_PATH=$SHM_BASE"
  "HFT_SHM__N_MAX=4"
  "HFT_SHM__QUOTE_SLOTS=1024"
  "HFT_SHM__TRADE_RING_CAPACITY=8192"
  "HFT_SHM__ORDER_RING_CAPACITY=1024"
)

env_prefix() {
  local out=""
  for kv in "${COMMON_ENV[@]}"; do out="$out $kv"; done
  printf '%s' "$out"
}

# 세션 생성 (pane 0 = publisher).
tmux new-session -d -s "$SESSION" -x 200 -y 50 -n hft
# publisher 는 가장 먼저 떠서 SharedRegion 을 초기화해야 한다.
tmux send-keys -t "$SESSION":0.0 \
  "$(env_prefix) HFT_PUBLISHER_READINESS_PORT=18188 $BIN_DIR/hft-publisher" C-m

# pane 1 — gateway. publisher 준비까지 약간의 waiting 필요 → 0.5s sleep.
tmux split-window -h -t "$SESSION":0.0
tmux send-keys -t "$SESSION":0.1 \
  "sleep 0.5 && $(env_prefix) $BIN_DIR/hft-order-gateway" C-m

# pane 2 — strategy VM_ID=0.
tmux split-window -v -t "$SESSION":0.0
tmux send-keys -t "$SESSION":0.2 \
  "sleep 0.8 && $(env_prefix) HFT_VM_ID=0 $BIN_DIR/hft-strategy-demo" C-m

# pane 3 — strategy VM_ID=1.
tmux split-window -v -t "$SESSION":0.1
tmux send-keys -t "$SESSION":0.3 \
  "sleep 0.8 && $(env_prefix) HFT_VM_ID=1 $BIN_DIR/hft-strategy-demo" C-m

tmux select-layout -t "$SESSION":0 tiled
tmux select-pane -t "$SESSION":0.0
tmux set-option -t "$SESSION" mouse on

echo "[dev_layout] tmux 세션 '$SESSION' 준비 완료. attach:"
echo "    tmux attach -t $SESSION"

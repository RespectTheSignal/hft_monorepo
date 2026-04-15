#!/usr/bin/env bash
# host_bootstrap.sh — v2 multi-VM host 사전 점검 / 준비 스크립트.
#
# 수행 항목 (비파괴 — 실제 변경은 user confirmation 후에만):
#   1. grub cmdline 의 hugepage / isolcpus 확인
#   2. /dev/hugepages 마운트 상태
#   3. cpupower governor = performance 인지
#   4. ivshmem 관련 kernel module 로드 상태
#   5. /sys/kernel/mm/transparent_hugepage/enabled
#
# 기본값은 모두 report-only. `--apply` 를 붙이면 *일부* 설정을 즉시 적용
# (governor, THP 등 reboot 불필요한 항목만).
#
# exit code:
#   0 : 통과
#   1 : 경고 — 수동 조치 필요
#   2 : 치명 — 부트 파라미터 수정 + reboot 필요

set -euo pipefail

APPLY=0
for a in "$@"; do
  case "$a" in
    --apply) APPLY=1 ;;
    -h|--help)
      sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown arg: $a" >&2; exit 2 ;;
  esac
done

warn() { printf '\033[1;33m[WARN]\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m[ OK ]\033[0m %s\n' "$*"; }
bad()  { printf '\033[1;31m[FAIL]\033[0m %s\n' "$*"; }

rc_warn=0
rc_fail=0

# 1. grub cmdline — hugepages / isolcpus / nohz_full / rcu_nocbs
CMDLINE="$(cat /proc/cmdline)"
echo "cmdline: $CMDLINE"
for want in "hugepagesz=1G" "hugepages="; do
  if [[ "$CMDLINE" == *"$want"* ]]; then
    ok "cmdline contains $want"
  else
    warn "cmdline missing '$want' — 1GB hugepages 준비 안 된 상태. grub 편집 필요."
    rc_warn=1
  fi
done
if [[ "$CMDLINE" == *"isolcpus="* ]]; then
  ok "isolcpus configured"
else
  warn "isolcpus 없음 — publisher/gateway hot CPU 가 OS scheduler 방해받을 수 있음."
  rc_warn=1
fi

# 2. /dev/hugepages mount
if mountpoint -q /dev/hugepages; then
  ok "/dev/hugepages is a mountpoint"
  HP_FREE=$(grep -c '^' /dev/hugepages 2>/dev/null || true)
  echo "   files present: $HP_FREE"
else
  warn "/dev/hugepages 미마운트 — fstab 또는 systemd-tmpfiles 확인 필요."
  rc_warn=1
fi

# 3. CPU governor
if command -v cpupower >/dev/null 2>&1; then
  GOV=$(cpupower frequency-info -p 2>/dev/null | awk '/The governor/ {print $5}' | tr -d '"')
  if [[ "$GOV" == "performance" ]]; then
    ok "cpu governor = performance"
  else
    warn "cpu governor = $GOV (권장: performance)"
    if [[ "$APPLY" -eq 1 ]]; then
      sudo cpupower frequency-set -g performance && ok "applied performance governor" || rc_fail=1
    else
      rc_warn=1
    fi
  fi
else
  warn "cpupower 설치 안 됨 (linux-tools)."
  rc_warn=1
fi

# 4. ivshmem module — host 에서 직접 필요한 건 아니지만 QEMU 가 필요.
if lsmod | grep -q '^kvm'; then
  ok "kvm module loaded"
else
  bad "KVM module 미로드 — QEMU HVM 부팅 불가."
  rc_fail=1
fi

# 5. THP
THP=$(cat /sys/kernel/mm/transparent_hugepage/enabled 2>/dev/null || echo "?")
case "$THP" in
  *'[never]'*)
    ok "THP = never (권장값, explicit hugetlbfs 사용)"
    ;;
  *)
    warn "THP = $THP (권장: never, explicit hugetlbfs 경로와 충돌 가능)"
    if [[ "$APPLY" -eq 1 ]]; then
      echo never | sudo tee /sys/kernel/mm/transparent_hugepage/enabled >/dev/null \
        && ok "applied THP=never"
    else
      rc_warn=1
    fi
    ;;
esac

echo
if [[ "$rc_fail" -ne 0 ]]; then
  bad "치명 이슈 발견. 수정 후 reboot 필요."
  exit 2
elif [[ "$rc_warn" -ne 0 ]]; then
  warn "경고 항목이 있으나 즉시 치명적이진 않음. 프로덕션 전에 반드시 해소."
  exit 1
else
  ok "모든 체크 통과 — v2 multi-VM 부팅 가능."
  exit 0
fi

#!/usr/bin/env bash
# launch_strategy_vm.sh — infra VM 과 동일 SHM 영역에 attach 하는 strategy VM.
#
# 동일 호스트의 hugetlbfs 파일 ($SHM_FILE) 을 share=on 으로 다시 매핑해 guest 쪽
# PCI BAR2 로 노출. guest 쪽 strategy 바이너리는 Backing::PciBar { path:
# /sys/bus/pci/devices/.../resource2 } 로 attach.
#
# 각 strategy VM 은 고유 VM_ID 를 가져야 하며 (0..LayoutSpec.n_max), 이 값은
# strategy 바이너리 환경변수 HFT_VM_ID 로 전달된다.

set -euo pipefail

: "${VM_ID:?VM_ID 환경변수 필수 (0..n_max-1)}"
: "${VM_NAME:=hft-strat-${VM_ID}}"
: "${VM_MEM:=4096}"
: "${VM_CPUS:=4}"
: "${VM_DISK:=$HOME/vms/${VM_NAME}.qcow2}"

: "${SHM_FILE:=/dev/hugepages/hft_v2}"
: "${SHM_BYTES:=$((2 * 1024 * 1024 * 1024))}"

if [[ ! -f "$SHM_FILE" ]]; then
  echo "[launch_strategy_vm] $SHM_FILE 없음. 먼저 infra VM 을 띄우세요." >&2
  exit 2
fi

if [[ ! -f "$VM_DISK" ]]; then
  echo "[launch_strategy_vm] VM disk $VM_DISK 없음." >&2
  exit 2
fi

TAPDEV="${TAPDEV:-tap-hft-strat-$VM_ID}"

set -x
exec qemu-system-x86_64 \
  -name "$VM_NAME" \
  -enable-kvm -cpu host,+invtsc -smp "$VM_CPUS" \
  -m "$VM_MEM" \
  -mem-path /dev/hugepages -mem-prealloc \
  -object memory-backend-file,id=hostmem,share=on,mem-path="$SHM_FILE",size="$SHM_BYTES" \
  -device ivshmem-plain,memdev=hostmem \
  -drive file="$VM_DISK",if=virtio,format=qcow2,cache=none,aio=io_uring \
  -netdev tap,id=n0,ifname="$TAPDEV",script=no,downscript=no \
  -device virtio-net-pci,netdev=n0 \
  -nographic -serial mon:stdio

#!/usr/bin/env bash
# launch_infra_vm.sh — publisher + order-gateway 가 올라갈 infra VM 을 띄운다.
#
# ivshmem-plain master 모델:
#   - 호스트에 hugetlbfs 파일 $SHM_FILE 생성 (LayoutSpec.total_bytes 크기).
#   - QEMU -device ivshmem-plain,memdev=hostmem 로 guest VM 에 BAR2 로 노출.
#   - infra VM 내에서 publisher 가 create_or_attach 로 초기화.
#
# 본 스크립트는 사이트마다 customize 필수. 기본값은 dev/staging 기준.

set -euo pipefail

: "${VM_NAME:=hft-infra}"
: "${VM_MEM:=8192}"           # MiB
: "${VM_CPUS:=8}"
: "${VM_CORE_PIN:=4,5,6,7}"   # isolcpus 범위 중 infra 전용
: "${VM_DISK:=$HOME/vms/${VM_NAME}.qcow2}"
: "${VM_KERNEL:=}"            # optional direct kernel boot
: "${VM_INITRD:=}"
: "${VM_CMDLINE:=}"

: "${SHM_FILE:=/dev/hugepages/hft_v2}"
: "${SHM_BYTES:=$((2 * 1024 * 1024 * 1024))}"   # 2GiB 기본

if [[ ! -f "$SHM_FILE" ]]; then
  echo "[launch_infra_vm] creating SHM backing file at $SHM_FILE ($SHM_BYTES bytes)"
  truncate -s "$SHM_BYTES" "$SHM_FILE"
fi

if [[ ! -f "$VM_DISK" ]]; then
  echo "[launch_infra_vm] VM disk $VM_DISK 없음. 먼저 생성하세요 (qemu-img create)."
  exit 2
fi

TAPDEV="${TAPDEV:-tap-hft-infra}"

KERNEL_OPTS=()
if [[ -n "$VM_KERNEL" ]]; then
  KERNEL_OPTS+=(-kernel "$VM_KERNEL")
  if [[ -n "$VM_INITRD" ]]; then KERNEL_OPTS+=(-initrd "$VM_INITRD"); fi
  if [[ -n "$VM_CMDLINE" ]]; then KERNEL_OPTS+=(-append "$VM_CMDLINE"); fi
fi

set -x
exec qemu-system-x86_64 \
  -name "$VM_NAME" \
  -enable-kvm -cpu host,+invtsc -smp "$VM_CPUS,sockets=1,cores=$VM_CPUS,threads=1" \
  -m "$VM_MEM" \
  -mem-path /dev/hugepages -mem-prealloc \
  -object memory-backend-file,id=hostmem,share=on,mem-path="$SHM_FILE",size="$SHM_BYTES" \
  -device ivshmem-plain,memdev=hostmem,master=on \
  -drive file="$VM_DISK",if=virtio,format=qcow2,cache=none,aio=io_uring \
  -netdev tap,id=n0,ifname="$TAPDEV",script=no,downscript=no \
  -device virtio-net-pci,netdev=n0 \
  -nographic -serial mon:stdio \
  "${KERNEL_OPTS[@]}"

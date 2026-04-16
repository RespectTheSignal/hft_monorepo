# ADR 0003 — Multi-VM 배포와 ivshmem 채택

Status: **Accepted** (2026-04-15)
Supersedes: 없음. 보완: ADR-0001 monorepo-layout, SHM_DESIGN.md
Owner: jeong

## Context

운영 배포가 "단일 물리 호스트 + 단일 OS 내 intra-host SHM" 이 아니라, **단일
물리 호스트 + KVM/QEMU 하이퍼바이저로 분리된 N 개 VM** 이라는 사실이
2026-04-15 에 확정됐다. Publisher/order-gateway 는 infra 측에, strategy 는
VM 당 1개로 수평 확장. `/dev/shm` 은 VM 경계를 넘지 못하므로 Phase 2 Track C
에서 만든 SHM IPC 는 그대로 동작하지 않는다.

요구사항:
- 기존 wire 레이아웃 (OrderFrame/QuoteSlot/TradeFrame/SymbolTable) 재활용 극대화.
- sub-μs 수준의 인터-VM 지연.
- 전략 VM 추가/교체가 운영 표준 플로우로 가능.

## Decision

1. **Inter-VM 공유메모리 = ivshmem-plain.**  hugetlbfs 백엔드 파일 하나를
   infra VM + 모든 strategy VM 에 PCI BAR 로 attach.  인터럽트 없음, 순수 폴링.
2. **단일 shared region file** 안에 sub-region 배치:
   header → symbol table → MD quote slots (SPMC/seqlock) → MD trade ring (SPMC) →
   N_MAX 개 order rings (각 SPSC, 전략 VM 전용).
3. **Publisher/order-gateway 는 infra VM.** host OS 직접 구동 금지.
4. **OrderRing fan-in 은 N×SPSC** — gateway 가 다수 링을 polling.  한 링에
   두 producer 금지.
5. **wire 레이아웃은 v2 digest 로 잠금.** 변경은 전체 infra reboot 이벤트.
6. ZMQ 경로는 유지하되 **fallback 전용**.

보충 메모 (2026-04-17):
- `OrderFrame` 자체 크기/필드는 유지하되, `aux[5]` 해석은 `kind`-dependent union 으로
  확장됐다. `Place` 는 `PlaceAuxMeta(level, reduce_only, text_tag)` 를, `Cancel` 은
  기존 `exchange_order_id` ASCII 를 유지한다.

## Alternatives considered

- **vhost-vsock**: 5–15μs. 구현 단순하지만 HFT 지연 목표 미달.
- **virtio-net + ZMQ**: 20–50μs. 성능 부적합.
- **SR-IOV/DPDK**: 물리 NIC 경유 또는 커널-바이패스 복잡도. 동일 머신 내부
  통신에 오버엔지니어링.
- **ivshmem-doorbell**: 인터럽트가 jitter 를 늘려 HFT 에 역효과.

## Consequences

Positive:
- `hft-shm` 의 layout/frame/ring 코드는 **변경 없음**. backend (mmap source) 만
  추상화 추가.
- 지연 목표 (p99 < 1μs 인터-VM) 현실적으로 달성 가능.
- 전략 격리 (VM 단위) + 성능 (SHM) 동시 확보.

Negative:
- QEMU/libvirt 셋업 복잡도 증가. 호스트 튜닝 (hugepages, isolcpus, IRQ) 필수.
- N_MAX 를 boot 시 고정 → 동적 확장은 host reboot 이벤트.
- ivshmem 영역 전체가 RW 로 보이므로 전략의 메모리 훼손 가능성 → MPK 등 후속 연구.

Operational:
- 상세 구현/운영 규칙은 `docs/architecture/MULTI_VM_TOPOLOGY.md` 에 집약.
- 위반 시 무효: ivshmem-doorbell 사용, 한 ring 에 두 producer, hugepage 미설정.

## References

- `docs/architecture/MULTI_VM_TOPOLOGY.md` (authoritative)
- `docs/architecture/SHM_DESIGN.md` (layout 원본)
- `INFRA_REQUIREMENTS.md` (OS/HW 요구사항)

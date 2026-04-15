# Multi-VM Topology — 실제 배포 구조 (Authoritative)

> **이 문서는 Phase 2 Track C 이후의 최종 배포 구조다.** 기존
> `docs/architecture/SHM_DESIGN.md` 는 "단일 호스트 내 단일 OS" 가정으로 작성되었고,
> 그 레이아웃/프레임 정의는 모두 유효하지만 **IPC 경계는 이 문서가 대체한다.**
> 코드/인프라/설정 관련 판단은 항상 이 문서를 1차 레퍼런스로 삼는다.

작성: 2026-04-15. 상태: **LOCKED** (변경 시 ADR-0003 업데이트 필요)

---

## 0. TL;DR — 지켜야 할 5가지

1. **SHM 백엔드는 ivshmem-plain.** 인터럽트 쓰지 않는다. 강제 폴링.
2. **Publisher/OrderGateway 는 infra VM 에 둔다.** 호스트 OS 에 직접 프로세스 올리지 않는다.
3. **Strategy VM 은 N_MAX 로 사전 고정.** ivshmem 영역은 boot 시 크기 결정이므로 런타임 증감 불가.
4. **wire layout (OrderFrame/QuoteSlot/TradeFrame/SymbolTable) 은 호스트-게스트 간 바이트 단위로 동일.** 레이아웃 변경은 전체 infra 재부팅 이벤트.
5. **MD = 1-writer→N-readers (SPMC/seqlock), Orders = N-writers→1-reader (N×SPSC).** 한 영역에 여러 writer 를 두지 않는다.

---

## 1. 배포 토폴로지

```
┌─────────────────────────────────────────────────────────────────────────┐
│  Physical Host (Linux + KVM/QEMU)                                       │
│  NUMA node 0 전용 (모든 VM, 모든 hugepage)                               │
│                                                                          │
│  ┌─────────────────────────────────┐                                    │
│  │ Infra VM  (vCPU 2-7 고정)       │                                    │
│  │  - publisher  (exchange WS →    │                                    │
│  │                parse → MD 영역)  │                                    │
│  │  - order-gateway (order ring 들  │                                    │
│  │                   → REST)        │                                    │
│  │  - 모니터링/로깅 sidecar         │                                    │
│  └──────────────┬──────────────────┘                                    │
│                 │ ivshmem PCI BAR (mmap)                                │
│                 ▼                                                        │
│   ┌─────────────────────────────────────────────────────────┐           │
│   │  Shared Region  (hugetlbfs, 1GB pages, NUMA node 0)     │           │
│   │  /dev/hugepages/hft_shm_v2                              │           │
│   │                                                          │           │
│   │  [0] Region header  (magic/version/layout digest)       │           │
│   │  [1] Symbol table   (ro after warmup)                   │           │
│   │  [2] MD quote slots (SPMC, seqlock)                     │           │
│   │  [3] MD trade ring  (SPMC, mpsc-style cursor per reader)│           │
│   │  [4] Order rings    (N_MAX × SPSC, one per strategy VM) │           │
│   └─────────────────────────────────────────────────────────┘           │
│                 ▲                                                        │
│                 │ ivshmem PCI BAR (mmap)                                │
│  ┌──────────────┴──────────────┬──────────────┬──────────────┐          │
│  │  Strategy VM 1              │  Strategy VM 2│  ... VM N   │          │
│  │   vCPU 8-11 고정            │   vCPU 12-15 │   vCPU ...  │          │
│  │  - MD read (quote+trade)    │   ...        │             │          │
│  │  - order write → ring[1]    │   ring[2]    │   ring[N]   │          │
│  └─────────────────────────────┴──────────────┴──────────────┘          │
│                                                                          │
│  Control/Admin plane: virtio-net (SSH/metrics, 지연 무관)               │
└─────────────────────────────────────────────────────────────────────────┘
```

### 1.1 컴포넌트 위치

| 컴포넌트 | 위치 | 이유 |
|---|---|---|
| Publisher | infra VM | 외부 WS 종단, crash 격리 |
| Order-gateway | infra VM | 외부 REST 종단, creds 격리 |
| Strategy N개 | strategy VM N개 | 격리/수평확장, VM당 1 프로세스 |
| 모니터링/로그 | infra VM | 관리 일원화 |
| ivshmem 백엔드 파일 | host hugetlbfs | VM 이 attach |

### 1.2 경계 통신 매트릭스 (허용된 것만)

| 발신 | 수신 | 채널 | 용도 |
|---|---|---|---|
| publisher | 전 strategy | ivshmem (MD quote/trade) | 시세 broadcast |
| strategy_i | order-gateway | ivshmem (order ring[i]) | 주문/취소 |
| infra VM ↔ 외부 | 거래소 서버 | 물리 NIC (TCP/WSS) | 시세+주문 |
| 모든 VM ↔ 운영자 | admin/metrics | virtio-net, SSH, Prometheus | 모니터링 |
| **strategy ↔ strategy** | **금지** | — | 전략 독립성 유지 |
| **strategy → host** | **금지** | — | host 오염 금지 |
| **strategy → publisher** | **금지** | — | 단방향 MD |

---

## 2. 공유 메모리 영역 세부

### 2.1 하나의 파일, 여러 sub-region

단일 `/dev/hugepages/hft_shm_v2` 파일 (≥ 1GB, 1GB page 사용 권장).  ivshmem
device 당 하나의 파일만 backing 가능하므로 영역들은 **단일 파일 내부의 offset 분할**.

```
file offset    size         region
──────────────────────────────────────────────────────────
0x00000000     4KB          RegionHeader (magic, digest, offsets)
0x00001000     256KB        SymbolTable        (open-addressing, N_sym=10k)
0x00041000     ~1.3MB       MdQuoteSlots       (N_slot=10k × 128B)
0x00181000     ~16MB        MdTradeRing        (131072 × 128B = 16MB, SPMC)
0x01181000     ~64MB        OrderRings[0..N_MAX-1]  (ring=8192 × 128B = 1MB each)
...
align up to 1G page boundary
```

offset 은 `RegionHeader` 에 기록. 코드 쪽은 header 읽어 슬라이스 뜸.

### 2.2 RegionHeader (새 구조, v2)

```
struct RegionHeader {                      // 4KB, cacheline-aligned
  magic:              u64,                 // 0x48465453484D5632 "HFTSHMV2"
  version:            u32,                 // 2
  layout_digest:      [u8; 32],            // SHA-256 of the layout spec (see §2.3)
  created_ns:         u64,
  writer_pid:         u32,
  region_size:        u64,
  sym_off / sym_len:  u64 × 2,
  quote_off / quote_len: u64 × 2,
  trade_off / trade_len: u64 × 2,
  order_ring_count:   u32,                 // = N_MAX
  order_ring_size:    u64,                 // single ring bytes
  order_rings_off:    u64,                 // base offset of ring 0
  _reserved:          [u8; ...],           // 4KB 까지 0
}
```

**Attach 시 MUST**: magic + version + layout_digest 일치 확인. 불일치면 에러로
기동 실패 (silent corruption 방지).

### 2.3 Layout digest

`layout_digest` 는 다음을 UTF-8 JSON canonical 로 묶어 SHA-256:

```
{ "version": 2, "OrderFrame": 128, "QuoteSlot": 128, "TradeFrame": 128,
  "SymbolTable_capacity": 10000, "OrderRing_capacity": 8192,
  "MdQuoteSlots_count": 10000, "MdTradeRing_capacity": 131072,
  "OrderRing_count_max": 16 }
```

**규칙**: 이 JSON 이 코드 상수와 불일치하면 **컴파일 타임 test 에서 실패**해야 한다
(`const_assert_eq!`).  런타임에서는 publisher 가 header 를 쓰는 값과 strategy 가
코드에 박은 digest 비교.

### 2.4 각 sub-region 의 의미

- **SymbolTable**: `hft-shm::symbol_table` 그대로. writer=publisher, reader=전원.
  publisher 가 warmup 동안 채우고, 런타임에 새 심볼 등장 시에만 추가.
- **MdQuoteSlots**: `hft-shm::quote_slot` seqlock, 1-writer(publisher) →
  N-readers(strategies + gateway 모니터).  현 `QuoteSlotWriter/Reader` 그대로.
- **MdTradeRing**: `hft-shm::spmc_ring::TradeRingReader/Writer` 그대로.
  각 reader 는 **독립 cursor** 를 자기 프로세스 heap 에 유지 (shared 영역에
  쓰지 않는다 — race 방지).
- **OrderRings[i]**: 전략 VM `i` 전용 SPSC 링. producer=strategy VM i,
  consumer=gateway.  Cancel/Place 모두 현 `OrderFrame` 128B.

---

## 3. 호스트 인프라 요구사항 (MUST)

### 3.1 커널 파라미터 (grub cmdline)

```
default_hugepagesz=1G hugepagesz=1G hugepages=8
isolcpus=2-31 nohz_full=2-31 rcu_nocbs=2-31
intel_idle.max_cstate=0 processor.max_cstate=1 idle=poll
intel_pstate=disable mitigations=off
transparent_hugepage=never
clocksource=tsc tsc=reliable
```

- `hugepages=8` → 8 × 1GB. 총 공유영역 + 게스트 RAM 예비.  필요하면 증가.
- `isolcpus` 는 infra VM + strategy VM 에 할당할 코어 범위 **전체** 포함.
- `mitigations=off` 는 성능 최우선 환경에서만. 규제/보안 요구 시 삭제.
- TSC invariant CPU 필수 — `/proc/cpuinfo` 의 `constant_tsc nonstop_tsc`
  플래그 확인. AMD Zen 3+ / Intel Haswell+ 이면 OK.

### 3.2 hugetlbfs 마운트

```
mount -t hugetlbfs -o pagesize=1G,size=8G none /dev/hugepages
```

`/etc/fstab` 에 영구 등록. 권한은 이후 ivshmem-server 프로세스만 쓰도록 제한.

### 3.3 NUMA

**전원 노드 0 에 고정.** 체크리스트:
- `numactl --hardware` 로 토폴로지 확인.
- hugepages 할당도 node 0 전용:
  `echo 8 > /sys/devices/system/node/node0/hugepages/hugepages-1048576kB/nr_hugepages`
- QEMU 실행 시 `-numa node,memdev=mem0 -object memory-backend-file,id=mem0,mem-path=/dev/hugepages/...,size=XG,prealloc=on,policy=bind,host-nodes=0`.

### 3.4 IRQ affinity

- 인터럽트를 isolated core **밖으로** 몰아냄:
  ```
  for irq in /proc/irq/*/smp_affinity; do echo 3 > $irq; done   # cpu 0-1 만
  ```
- NIC RSS 큐는 infra VM 전용 코어로 핀.
- Hyperthread 는 페어 단위로 관리 — isolated core 에서는 두 스레드 모두 isolate.

### 3.5 전원 관리

- BIOS: Turbo OFF (jitter 감소), C-states 비활성, Power Profile = Performance.
- 영구:  `cpupower frequency-set -g performance`.

---

## 4. KVM/QEMU 요구사항 (MUST)

### 4.1 ivshmem device 설정 (infra VM + 각 strategy VM 공통)

```
-object memory-backend-file,id=hft_shm,mem-path=/dev/hugepages/hft_shm_v2,\
        size=512M,share=on,prealloc=on,policy=bind,host-nodes=0
-device ivshmem-plain,memdev=hft_shm,master=on    # infra VM 만 master
-device ivshmem-plain,memdev=hft_shm              # strategy VM
```

- `ivshmem-plain` (인터럽트 없음) 강제. `ivshmem-doorbell` 금지.
- 모든 VM 이 **같은 파일** 을 attach.
- `share=on` 필수 (파일 매핑 공유).
- `master=on` 은 infra VM 하나에만. crash 시 영역 재초기화 권한.

### 4.2 vCPU pinning & topology

```
-smp cpus=6,sockets=1,cores=6,threads=1
-cpu host,+invtsc,+tsc-deadline,-hypervisor
-overcommit cpu-pm=on mem-lock=on
```

- 각 vCPU 를 물리 코어에 1:1 핀 (`virsh vcpupin` 또는 qemu `-numa cpu,...`).
- vCPU 와 emulator 스레드 분리 (`emulatorpin`).
- Hyperthread 페어 겹치지 않게 — isolated 물리 코어 독점.

### 4.3 KVM 튜닝

```
/sys/module/kvm/parameters/halt_poll_ns = 500000
/sys/module/kvm/parameters/halt_poll_ns_grow = 0
/sys/module/kvm/parameters/halt_poll_ns_shrink = 0
```

→ vCPU 가 HLT 없이 짧게 폴링, VM exit 회피.

### 4.4 게스트 커널 파라미터 (각 VM grub)

```
isolcpus=<strategy worker CPUs> nohz_full=... rcu_nocbs=...
intel_idle.max_cstate=0 processor.max_cstate=1 idle=poll
transparent_hugepage=never clocksource=tsc tsc=reliable
```

게스트에서도 동일. vCPU 를 물리코어가 보듯 다룸.

### 4.5 게스트에서 ivshmem 접근

- 게스트 PCI 장치: `lspci | grep 1110` (Red Hat Qumranet → ivshmem)
- sysfs 경로: `/sys/bus/pci/devices/0000:XX:YY.Z/resource2`
- O_RDWR + mmap.  `MAP_SHARED | MAP_POPULATE`.
- Rust 측 `hft-shm::mmap::GuestBacking::PciBar(PathBuf)` 신규 백엔드.
- Python 측은 ctypes `mmap` 으로 동일 파일 open.

### 4.6 Clock

- 전 VM `kvm-clock` + `tsc=reliable`.
- ts_ns 는 TSC 기반 단조 증가 보장.
- 필요하면 PTP (`ptp_kvm`) 로 host-guest 오프셋 공유.

---

## 5. 코드 변경 계획

### 5.1 `hft-shm` 크레이트

**추가**:
- `mmap::Backing` enum:
  ```
  enum Backing {
      HugetlbFile { path: PathBuf, size: usize },  // host / infra VM 도 가능
      PciBar      { path: PathBuf },               // strategy VM 게스트
  }
  ```
- `RegionHeader` 구조체 + offset 계산 + digest 검증.
- `SharedRegion::open(backing) -> ShmRegion` — 단일 파일에서 sub-region 슬라이스.
- `MultiOrderRingReader { readers: Vec<OrderRingReader> }` — N 개 링 polling 래퍼.
  policy: `DedicatedThreads` | `RoundRobinSingleThread`.

**불변**:
- `layout.rs` (OrderFrame/QuoteSlot/TradeFrame/OrderKind/exchange_to_u8/from_u8).
- `spsc_ring.rs::OrderRingReader/Writer` (기존 구조체 그대로).
- `spmc_ring.rs::TradeRingReader/Writer`.
- `quote_slot.rs::QuoteSlotReader/Writer`.
- `symbol_table.rs`.

**이유**: 호스트-게스트 간 바이너리 호환 유지.

### 5.2 `hft-config::ShmConfig`

```
pub struct ShmConfig {
    pub enabled: bool,
    pub backend: ShmBackend,             // Hugetlbfs | PciBar
    pub region_path: PathBuf,            // 단일 파일 경로
    pub region_size: u64,                // hugetlbfs 생성 시
    pub order_ring_count_max: u32,       // = N_MAX, boot 시 고정
    pub role: ShmRole,                   // Publisher | Gateway | Strategy { vm_id: u16 }
}

pub enum ShmBackend { Hugetlbfs, PciBar }
pub enum ShmRole {
    Publisher,                           // quote/trade writer
    Gateway,                             // order rings reader
    Strategy { vm_id: u16 },             // quote/trade reader + order ring[vm_id] writer
}
```

역할에 따라 접근 권한을 정적 분리. runtime check 포함.

### 5.3 `services/publisher`

- `ShmPublisher` 는 role=Publisher 로 open.
- quote/trade writer 만 유지 — order ring 은 touch 안 함.
- symbol table 은 Publisher 가 write 권한.

### 5.4 `services/order-gateway`

- role=Gateway 로 open.
- `ShmOrderSubscriber` 를 `MultiOrderRingShmSubscriber` 로 일반화:
  - `readers: Vec<OrderRingReader>`, 길이 = `order_ring_count_max`.
  - Default policy = `DedicatedThreads` (N_MAX ≤ 8 가정).
  - fan-in 은 Router 쪽 crossbeam 채널이 이미 MPSC 이므로 그대로 수용.
- 현 `shm_sub.rs` 의 Place/Cancel 디코드 로직은 **100% 재사용**.

### 5.5 `services/strategy` (Rust or Python)

- role=Strategy{vm_id=k} 로 open.
- MD 경로: QuoteSlotReader + TradeRingReader.
- 주문 경로: **자기 VM 의 OrderRingWriter 하나만** open.  다른 링에 절대 쓰지 않음 (SPSC 불변조건).
- vm_id 는 VM 부트 인자 (`HFT_VM_ID=3`).

### 5.6 ZMQ 경로의 운명

- **유지**. ivshmem 장애 시 fallback 으로 동작.
- 단, **정상 경로에서는 둘 중 하나만 활성** — 양쪽 동시 활성 시 dedup 캐시로 방어.
- ZMQ 는 admin/low-freq 신호에 선호.

---

## 6. 운영 규칙 (MUST / MUST NOT)

### MUST
- 모든 VM 재시작 순서: **strategies 먼저 shutdown → publisher/gateway → host 파일 재생성 → publisher/gateway → strategies**. header digest 로 stale 참조 감지.
- 새 전략 배포 시: vm_id 를 pre-allocated 범위 `0..N_MAX` 에서 재사용 또는 미사용 슬롯에 배치.
- 레이아웃 변경 시: 버전 bump (v2 → v3) + 전 컴포넌트 재빌드 + 전 VM 재부팅.
- 주기적 자가점검: gateway 가 각 링의 producer heartbeat seq 증가 감시. 5초 정체 시 alert.

### MUST NOT
- `ivshmem-doorbell` 사용. 인터럽트는 jitter 원인.
- `hugepages=0` 인 환경에서 기동. 반드시 pre-allocated 1GB page.
- 한 OrderRing 에 두 VM 이 쓰기 (SPSC 불변조건 깨짐 → UB).
- strategy VM 간 ivshmem 을 거친 임의 메시지 교환. 허용된 영역은 read-only (MD) + 자기 소유 order ring 뿐.
- `TransparentHugePage` 활성 상태로 운영 (THP defrag 가 멈춤 유발).
- 호스트 OS 위에 직접 publisher/gateway 프로세스 기동.
- 전략 내부 로깅을 SHM 영역에 쓰기. 로그는 virtio-net 기반 syslog/OTLP.

---

## 7. Fallback / 장애 대응

| 장애 | 감지 | 대응 |
|---|---|---|
| ivshmem 파일 손상 | digest 불일치 | infra VM 재부팅 → publisher 가 새 header 쓰고 strategies 재연결 |
| publisher 프로세스 crash | gateway 의 header writer_pid ping 타임아웃 | systemd 가 재시작, symbol table 재warmup (idempotent) |
| strategy VM 한대 hang | 해당 order ring producer seq 정지 | 해당 VM 만 재부팅, 다른 VM 영향 X |
| order-gateway crash | Rust task join 실패 | systemd 재시작, 주문 방향은 드롭 — 전략에 back-pressure 로 전달 |
| host hugepage 부족 | QEMU 시작 실패 | 운영자 개입, hugepages 재할당 |
| 전체 SHM 경로 불가 | 2초 no-heartbeat | ZMQ fallback 으로 자동 전환 (양쪽 연결 유지 상태) |

---

## 8. 테스트 전략

### 8.1 Local dev (VM 없이)
- `hft-shm` 은 hugetlbfs 대신 `/tmp` 파일도 수락 (dev 전용 플래그).
- 단일 프로세스에서 publisher + gateway + strategy 스레드 구동 → 레이아웃 회귀 테스트.
- 기존 `tests/end_to_end.rs` 는 이 모드로 동작. **유지 필수.**

### 8.2 단일 호스트, 두 VM
- infra VM × 1 + strategy VM × 1 + ivshmem.
- 핵심 테스트: digest 검증, cross-VM 지연 측정, graceful shutdown.

### 8.3 N_MAX VM 부하 테스트
- N_MAX 개 strategy VM 동시 기동, 각 VM 당 10kHz place/cancel.
- gateway 의 ring drain 지연 p99 ≤ 5μs 목표.

### 8.4 장애 주입
- publisher kill → recovery 시간 측정.
- strategy VM freeze → 다른 VM 무영향 확인.
- hugepage 파일 강제 truncate → digest mismatch 감지.

---

## 9. 성능 목표 (이 토폴로지 기준)

| 구간 | p50 | p99 | 비고 |
|---|---|---|---|
| publisher WS parse → MD slot write | ~3 μs | 8 μs | 현재 Phase 2 Track A |
| MD slot write → strategy VM read | ~0.3 μs | 1 μs | ivshmem 폴링 |
| strategy decide → order ring write | 전략 의존 | — | HFT 로직 영역 |
| order ring write → gateway read | ~0.2 μs | 1 μs | SPSC 폴링 |
| gateway → exchange REST (LAN) | ~150 μs | 500 μs | 외부 네트워크 |

tick-to-order (전 단) 목표: **p50 < 200 μs**, **p99 < 800 μs** (내부 구간만, 외부 REST 제외 시 < 50 μs).

---

## 10. 미해결 / 후속 결정 필요

- **ivshmem-server 도입 여부**: doorbell 을 안 쓰면 서버 불필요. 단, hot-plug
  (VM 재기동 중 자동 attach) 지원이 필요하면 재검토.
- **VM 간 권한 분리**: 현재는 ivshmem 영역 전체가 RW 로 보임. MPK (Memory
  Protection Keys) 나 QEMU 단계에서 영역 별 RO 매핑 가능한지 조사 (strategy 가
  symbol table 에 쓰기 못하도록).
- **Huge page 1G vs 2M**: 현 설계 1G 가정. 메모리 압박 환경에선 2M 로 다운그레이드
  가능하나 TLB 미스 증가 → p99 악화 측정 필요.
- **VM live migration**: 본 구조는 단일 호스트 전제. multi-host 확장 시
  ivshmem 대신 RDMA / DPDK 재설계.
- **Python strategy vs Rust strategy**: Python 은 GIL + ctypes 오버헤드로
  SHM 폴링 루프가 대략 0.5–1μs 느려짐. 전략 유형별로 언어 선택.

---

## 11. 체크리스트 (새 환경 부트스트랩)

Host:
- [ ] grub cmdline 에 hugepages, isolcpus, idle=poll 반영
- [ ] hugetlbfs 마운트, nr_hugepages 확인
- [ ] NUMA node 0 자원 고정
- [ ] IRQ smp_affinity 를 low core 로
- [ ] BIOS turbo/C-state 조정
- [ ] cpupower performance

QEMU/libvirt:
- [ ] infra VM XML: ivshmem-plain master=on, vCPU pin, host-passthrough CPU
- [ ] strategy VM XML: ivshmem-plain, vCPU pin, 동일 mem-path
- [ ] halt_poll_ns 설정

코드:
- [ ] `hft-shm` backend enum + RegionHeader + digest
- [ ] `hft-config::ShmConfig` v2 스키마
- [ ] publisher/gateway role 분리
- [ ] `MultiOrderRingShmSubscriber` N_MAX 링 polling
- [ ] strategy 측 ctypes reader/writer (Python) 또는 Rust helper
- [ ] digest mismatch fatal test

운영:
- [ ] vm_id 할당 문서화
- [ ] systemd unit (infra VM 의 publisher/gateway)
- [ ] Prometheus metric: ring depth, digest ok, heartbeat age
- [ ] 장애 주입 playbook

---

관련 문서:
- `docs/architecture/SHM_DESIGN.md` — 레이아웃/프레임 정의 (유효)
- `docs/adr/0003-multi-vm-ivshmem-topology.md` — 결정 기록 (본 문서 요약)
- `INFRA_REQUIREMENTS.md` (root) — OS/하드웨어 요구사항 상세판

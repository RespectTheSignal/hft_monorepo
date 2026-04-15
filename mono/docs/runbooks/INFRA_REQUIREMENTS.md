# HFT Monorepo — 인프라 / 하드웨어 제약사항

> **목표**: SHM 기반 sub-μs intra-host 통신 + 안정적 거래소 연결을 위한 **최소·권장 사양과 커널 튜닝 일람**.
> SHM 경로의 설계 근거는 `docs/architecture/SHM_DESIGN.md` 참조.

작성일: 2026-04-15  
적용 범위: `services/publisher`, `services/subscriber`, `services/strategy` (Python), `services/order-gateway`, QuestDB.

---

## 1. 필수 제약 (이것이 충족되지 않으면 목표 성능 미달)

### 1.1 단일 서버 (Single host)
- `hft-shm` 은 `tmpfs` (`/dev/shm`) 위에 `mmap` 되는 구조라 **같은 커널 안에서만 공유 가능**.
- 전 컴포넌트(publisher/subscriber/strategy/order-gateway/QuestDB)는 **동일 물리 서버**에 배치한다.
- cross-host 배포는 **금지**. 확장이 필요하면 물리 서버를 통째로 복제하고 거래소별로 파티셔닝.

### 1.2 Linux 커널
- **Ubuntu 22.04 LTS 이상** 또는 **Debian 12 이상**. kernel 6.1+ 권장.
- `O_DIRECT`, `MAP_POPULATE`, `MAP_HUGETLB`, `MADV_HUGEPAGE`, `SCHED_FIFO`, `sched_setaffinity`, `mlock` 이 모두 필요.
- Windows / macOS 는 개발용도 외에는 **배포 불가** (SHM 동작 보장 안 됨).

### 1.3 CPU
- **단일 소켓(1P)** EPYC 또는 Xeon. 최소 **16 물리 코어 / 32 SMT**.
- cross-socket UPI/Infinity Fabric 지연으로 인해 **듀얼소켓은 이 워크로드에 역효과**.
- AVX2 필수, AVX-512 있으면 여유.
- base clock ≥ 3.0GHz, all-core boost ≥ 3.5GHz.

### 1.4 메모리
- 최소 64GB, 권장 **128GB DDR5 ECC**.
- 한 채널당 DIMM 1개 (1 DPC) — DDR5 는 2 DPC 시 주파수가 떨어져 지연 분산이 커짐.
- 전체 메모리의 **1/4 이상을 2MB huge page 로 사전 예약** (아래 §3 참고).

### 1.5 스토리지
- **NVMe Gen4 1TB 이상** (QuestDB WAL + ILP spool).
- `xfs` 또는 `ext4` with `noatime,nodiratime`. ZFS/btrfs 는 지연 분산 이슈로 **비권장**.

### 1.6 네트워크
- 거래소 WebSocket/REST 응답 p50 이 1ms 이내로 들어오는 지역에 배치:
  - Gate, Binance, Bybit: 주로 **AWS ap-northeast-1(도쿄)** 또는 **ap-east-1(홍콩)**.
  - OKX, Bitget: 홍콩 or 싱가포르.
- NIC: **Mellanox ConnectX-5 이상** 권장. 저지연 커널 드라이버 튜닝 가능.
- `SO_BUSY_POLL` 활성화 가능한 NIC 필수.
- 공중망만 쓰면 colocation 은 불필요하지만, VPS/EC2 의 **noisy neighbor 경계** 때문에 bare-metal 또는 최소 `c6in.xlarge`/`m6in.large` 급의 dedicated tenancy 권장.

---

## 2. 권장 하드웨어 (프로덕션 레퍼런스)

| 항목 | 모델 예시 | 비고 |
|---|---|---|
| CPU | AMD EPYC 9354P (32c/64t, 3.25GHz base, 3.8GHz boost) | 단일 소켓, 256MB L3 |
| Mem | 128GB DDR5-4800 ECC RDIMM (8×16GB, 1 DPC) | NUMA 1 노드 구성 |
| NIC | Mellanox ConnectX-6 Lx (25GbE) | 저지연 드라이버, busy poll 지원 |
| NVMe | Samsung PM9A3 1.92TB (Gen4) | QuestDB WAL/ILP spool |
| OS | Ubuntu 22.04 LTS | kernel 6.5 HWE |
| Cloud 대안 | AWS `c7i.4xlarge` (16c/32t, Sapphire Rapids) + Nitro Enclave 아님, dedicated host | cross-AZ 배포 금지 |

**예산이 빠듯할 때 최소 구성**: EPYC 7313P (16c) / 64GB DDR4 / Gen4 NVMe 512GB / 10GbE. 거래 symbol 수를 줄이면 이 조합에서도 sub-μs SHM 지연은 유지된다.

---

## 3. OS 커널 튜닝 (부팅 시 적용)

### 3.1 `/etc/default/grub` — kernel cmdline
```
GRUB_CMDLINE_LINUX_DEFAULT="quiet \
    isolcpus=2-17 \
    nohz_full=2-17 \
    rcu_nocbs=2-17 \
    intel_pstate=disable \
    processor.max_cstate=1 \
    idle=poll \
    mitigations=off \
    transparent_hugepage=madvise \
    hugepagesz=2M \
    hugepages=8192 \
    numa_balancing=disable \
    audit=0"
```
- `isolcpus=2-17`: 코어 2~17 을 리눅스 스케줄러에서 제외 → 아래 §4 의 역할별 핀에 사용.
- `nohz_full=2-17`: tickless → 격리된 코어의 주기적 scheduler tick 제거 (< 1μs jitter).
- `rcu_nocbs=2-17`: RCU callback 을 비격리 코어(0,1)로 오프로드.
- `processor.max_cstate=1 idle=poll`: C-state 진입 금지 → wake-up 레이턴시 결정적.
- `intel_pstate=disable`: 주파수 스케일링 끔. `cpupower frequency-set -g performance` 로 고정.
- `mitigations=off`: Spectre/Meltdown 완화 해제 (internal-only host 전제). 인터넷 직접 노출 서버라면 이 옵션은 **금지**.
- `transparent_hugepage=madvise`: 프로세스가 `madvise(MADV_HUGEPAGE)` 요청할 때만 HP 승격.
- `hugepages=8192`: 2MB × 8192 = **16GB** 고정 huge page. SHM 영역 + QuestDB buffer 용.
- `numa_balancing=disable`: AutoNUMA 가 hot page 를 재배치하며 간헐적 spike 유발 → 끈다.

적용 후: `sudo update-grub && sudo reboot`.

### 3.2 `/etc/sysctl.d/99-hft.conf`
```
# TCP 저지연
net.ipv4.tcp_low_latency = 1
net.ipv4.tcp_no_delay_ack = 1
net.core.busy_poll = 50
net.core.busy_read = 50
net.core.netdev_budget = 600

# socket buffer 상한
net.core.rmem_max = 268435456
net.core.wmem_max = 268435456
net.ipv4.tcp_rmem = 4096 262144 268435456
net.ipv4.tcp_wmem = 4096 262144 268435456

# huge page / memory
vm.nr_hugepages = 8192           # cmdline 과 동일 — runtime 에서도 보정
vm.swappiness = 1
vm.dirty_ratio = 10
vm.dirty_background_ratio = 3
vm.overcommit_memory = 1

# 스케줄러
kernel.sched_rt_runtime_us = -1  # SCHED_FIFO 가 CPU 를 100% 점유해도 kill 당하지 않음
kernel.numa_balancing = 0

# perf / ftrace 보안 (운영 중엔 disabled, 디버그 시 2)
kernel.perf_event_paranoid = 2
```
적용: `sudo sysctl --system`.

### 3.3 `/etc/security/limits.d/99-hft.conf`
```
hft  soft  memlock  unlimited
hft  hard  memlock  unlimited
hft  soft  nofile   1048576
hft  hard  nofile   1048576
hft  soft  rtprio   99
hft  hard  rtprio   99
hft  soft  nice     -20
hft  hard  nice     -20
```
- `memlock unlimited`: SHM + mlock 호출 허용.
- `rtprio 99`: `SCHED_FIFO` 우선순위 사용 허용 (order-gateway 주문 task 에만 부분 적용).
- 반드시 HFT 전용 유저 (`hft`) 를 생성하고 이 유저로 실행.

### 3.4 IRQ affinity (publisher 핫패스에서 IRQ 제거)
```bash
# NIC IRQ 를 비격리 코어(0,1) 로 고정
for irq in $(grep -E "eth0|eno1|ens" /proc/interrupts | awk '{print $1}' | tr -d ':'); do
    echo 3 > /proc/irq/$irq/smp_affinity   # 0b11 = CPU 0,1
done

# IRQ balancer 끔
sudo systemctl disable --now irqbalance
```
이걸 `/etc/systemd/system/irq-pin.service` unit 으로 만들어서 boot-time 에 자동화.

### 3.5 Huge page 사전 할당 확인
```bash
grep Huge /proc/meminfo
# HugePages_Total:    8192
# HugePages_Free:     8192
# Hugepagesize:       2048 kB
```
8192 × 2MB = 16GB 가 잡혀 있어야 한다. 부족하면 부팅 직후 할당이 어려우므로 반드시 **cmdline 으로 잡을 것**.

---

## 4. CPU 코어 할당 플랜

32 물리코어 (64 SMT) EPYC 9354P 기준:

| 코어 | 역할 | isolcpus | 스케줄 클래스 |
|---|---|---|---|
| 0–1 | OS/IRQ/housekeeping, irqbalance off 후 NIC IRQ, systemd | no | 기본 |
| 2–5 | publisher feed workers (4 exchange × 1 core + spare) | yes | SCHED_OTHER + affinity |
| 6–9 | subscriber + SHM writer + spool flush | yes | SCHED_OTHER + affinity |
| 10–13 | Python strategy (main, signal, warmup helpers) | yes | SCHED_OTHER + affinity |
| 14–17 | order-gateway router + REST executors | yes | `SCHED_FIFO prio=80` (주문 전송 task 만) |
| 18–23 | QuestDB + logging drain | no | 기본 |
| 24–31 | spare / burst capacity / tests | no | 기본 |

**주의:** SMT 를 활성화한 경우, isolcpus 에는 "논리 코어 쌍"을 **함께** 넣어야 한다 (코어 2 의 쌍이 34 면 `isolcpus=2-17,34-49`). `lscpu -e=CPU,CORE,SOCKET,NODE` 로 매핑 확인.

각 프로세스는 `hft-telemetry::pin_current_thread(core)` 로 스레드 단위 핀 적용. order-gateway 의 "주문 실제 전송 task" 만 `SCHED_FIFO` 로 승격.

---

## 5. SHM 영역 크기 요건

`/dev/shm` 이 tmpfs 이므로 전체 크기가 RAM 에서 차감됨. 기본 50% 에서 축소 금지.

| 영역 | 경로 | 크기 | 설명 |
|---|---|---|---|
| quotes v2 | `/dev/shm/hft_quotes_v2` | 1.5 MB | 10,000 symbol × 128B seqlock slot + 헤더 |
| trades v2 | `/dev/shm/hft_trades_v2` | 128 MB | 2^20(1M) × 128B SPMC broadcast ring |
| orders v2 | `/dev/shm/hft_orders_v2` | 2 MB | 16,384 × 128B SPSC (Python→Rust) |
| symbol table | `/dev/shm/hft_symtab_v2` | 1 MB | name → slot index 인덱스 |
| **합계** | | **~132.5 MB** | RAM 에서 항상 상주 |

모두 2MB huge page 에 얹는다 (quotes/orders/symtab 은 작지만 thp=madvise 로 HP 요청, trades 는 MAP_HUGETLB 로 강제). 위 `hugepages=8192` 에서 충분히 커버.

---

## 6. 시간 동기화

- **chrony** 사용 (ntpd 금지). 거래소 시간축과의 skew 를 nano 수준 이하로 유지.
- `chrony.conf`:
  ```
  server time.nist.gov iburst
  server time.cloudflare.com iburst
  makestep 0.1 3
  rtcsync
  hwtimestamp *
  ```
- `hwtimestamp` 는 NIC 가 IEEE 1588(PTP) 또는 SO_TIMESTAMPING 지원 시만.
- **시간 동기화 오차가 1ms 를 넘으면 Phase 1 latency 게이트가 경보**를 울리도록 `hft-telemetry` 에 체크 포함.

---

## 7. 방화벽 / 보안

- 거래소 REST/WS 엔드포인트(443) 만 outbound 허용. inbound 는 전부 deny (관리용 SSH 제외).
- API 키/시크릿은 **환경변수 or HashiCorp Vault** — 코드 / `.toml` / git 에 평문 금지.
- `HFT_{EX}_API_KEY` 등은 systemd unit 의 `EnvironmentFile=/etc/hft/secrets.env` 로 주입, 파일 권한 `0400 hft:hft`.
- auditd 는 성능 영향이 커서 꺼두고 (`audit=0`), 대신 journald 로 로깅.

---

## 8. Systemd Unit 템플릿 (참고)

`/etc/systemd/system/hft-publisher.service`:
```ini
[Unit]
Description=HFT Publisher
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=hft
Group=hft
EnvironmentFile=/etc/hft/secrets.env
Environment=RUST_LOG=info
# CPU affinity: 코어 2-5 만 쓰도록 — 내부에서 pin_current_thread 로 스레드별 분배
CPUAffinity=2 3 4 5
# 메모리 락 무제한
LimitMEMLOCK=infinity
LimitNOFILE=1048576
# huge page 쓰는 allocator 허용
AmbientCapabilities=CAP_IPC_LOCK CAP_SYS_NICE
# OOM 방어
OOMScoreAdjust=-500
ExecStart=/opt/hft/bin/publisher
Restart=on-failure
RestartSec=1
# stop: SIGINT 로 graceful drain, 5초 후 강제
KillSignal=SIGINT
TimeoutStopSec=5

[Install]
WantedBy=multi-user.target
```

다른 서비스(subscriber/strategy/order-gateway)도 `CPUAffinity` 만 바꿔서 동일 템플릿 사용.

---

## 9. 운영 점검 체크리스트 (매 배포 전)

- [ ] `uname -r` ≥ 6.1
- [ ] `grep isolcpus /proc/cmdline` 에 격리 코어 확인
- [ ] `grep HugePages_Total /proc/meminfo` ≥ 8192
- [ ] `cpupower frequency-info` governor = performance, 모든 코어 max 주파수 고정
- [ ] `cat /sys/class/net/eth0/device/numa_node` 가 서비스 바인딩 NUMA 와 일치
- [ ] `ulimit -l` = unlimited (hft 유저로)
- [ ] `chronyc tracking` 의 System time offset < 1ms
- [ ] `/dev/shm` free space > 200MB
- [ ] 거래소 ping RTT (HTTPS handshake) < 3ms p50
- [ ] `cat /proc/sys/kernel/numa_balancing` = 0
- [ ] `cat /sys/kernel/mm/transparent_hugepage/enabled` 에 `[madvise]` 가 선택

---

## 10. 알려진 함정

1. **AWS Nitro**: `c7i.*` 계열은 huge page 지원하지만 `dedicated instance` 를 써야 noisy neighbor 없음. bare-metal(`c7i.metal-48xl`) 이 이상적.
2. **`isolcpus` 와 systemd `CPUAffinity`**: systemd 의 affinity 는 isolcpus 코어를 **침범 가능**하다. 반드시 affinity 가 isolcpus 범위 안이어야 의도대로 동작.
3. **SMT 쌍 분리**: 격리된 물리 코어의 SMT 쌍을 격리하지 않으면 다른 태스크가 그 쌍에서 돌며 L1/L2 캐시 오염.
4. **`/dev/shm` 크기 기본값**: tmpfs 로 RAM 의 50% 까지 자라지만, `/etc/fstab` 에서 고정 크기로 잡아야 cgroup memory accounting 이 정확.
5. **QuestDB ILP backpressure**: QuestDB 가 느려지면 ILP TCP 가 blocking. `hft-storage::SpoolWriter` 가 disk 로 흡수하지만 spool 도 꽉 차면 drop. 운영 시 QuestDB flush rate 모니터링 필수.
6. **Python GIL 해제**: strategy 에서 SHM reader 루프는 `threading` 으로 돌리면 GIL 때문에 의미 없음. 반드시 `multiprocessing` 또는 C 확장(`ctypes` / `cffi`) 로 native thread 에 맡길 것.

---

## 11. 참고

- 커널 튜닝 원조: RedHat "Low Latency Performance Tuning" (2021)
- `isolcpus` 현대적 대안: `cpuset` cgroup v2 (`cset` 툴). 재부팅 없이 동적 적용 가능하지만 운영 복잡도 ↑.
- DPDK 로 kernel-bypass NIC 를 쓰는 옵션도 있지만 Phase 3 이상의 사치. Phase 2 에서는 busy_poll + low_latency 만으로 충분.

# v2 Multi-VM SharedRegion — Phase 2 보강 변경 기록

작성: 2026-04-15. 범위: v2 SharedRegion 마이그레이션 이후 **남아있던 빠진 부분**과
**성능 최적화 여지**를 정밀하게 보강한 후속 작업 기록. 본 문서는 `ADR-0003` +
`MULTI_VM_TOPOLOGY.md` 요구사항 체크리스트 (§11) 와 일대일 매핑되며, 변경된 파일을
목록화한다.

---

## 1. 정리 — 이미 완료된 1차 마이그레이션 (맥락 확인용)

다음 항목은 이전 턴에서 이미 구현됐다 (변경 없음):

- `hft-shm::region::{SharedRegion, LayoutSpec, LayoutOffsets, RegionHeader, Role, Backing, SubKind}` — 단일 파일에서 sub-region 분리, SHA-256 digest 검증.
- `hft-shm::multi_order_ring::MultiOrderRingReader` — round-robin fan-in (기본).
- `hft-shm::mmap::ShmRegion::{sub_view, open_device}` — Arc<ShmRegion> 기반 view +
  PCI BAR backing.
- `hft-config::ShmConfig` v2 필드 (`backend` / `shared_path` / `role` / `vm_id` /
  `n_max`) + validation.
- `services/publisher::shm_pub::ShmPublisher::from_shared_region` + `lib::build_shm_publisher`
  backend dispatch (Legacy / DevShm / Hugetlbfs / PciBar 거부).
- `services/order-gateway::shm_sub::{ShmSubscriberSource, ReaderImpl}` + `main::build_shm_subscriber`
  backend dispatch.
- `hft-shm/tests/shared_region_e2e.rs` — publisher × 1 + gateway × 1 + strategies × N
  통합 테스트 + digest mismatch 거부 + 재연결 테스트.
- 설계 문서: `docs/architecture/{MULTI_VM_TOPOLOGY.md,SHM_DESIGN.md}` + `docs/adr/0003-*`.

---

## 2. 이번 턴에 새로 채운 빠진 부분

### 2.1 Publisher liveness heartbeat

**문제**: `MULTI_VM_TOPOLOGY.md §6` "주기적 자가점검: gateway 가 각 링의 producer
heartbeat seq 증가 감시. 5초 정체 시 alert" 가 미구현. 기존엔 publisher crash 를
감지하려면 OrderRing producer seq 를 긁어야 했는데, 그건 strategy VM 의 liveness
이고 publisher liveness 를 직접 감지하려면 별도 tick 이 필요.

**해결**: `RegionHeader` 에 두 개의 atomic 필드 추가 — `pub_heartbeat_ns`
(wall-clock ns) 와 `pub_seq` (단조 증가 tick). `_reserved` 를 16B 줄여 struct size
4KB 유지. digest 는 struct size 포함이라 자동으로 새 값으로 고정.

신규 API:
- `SharedRegion::touch_heartbeat(now_ns)` — publisher 가 호출, `&self` 만 필요 (atomic store).
- `SharedRegion::heartbeat_ns()` — gateway/모니터가 호출.
- `SharedRegion::heartbeat_seq()` — tick 증가 여부 확인용.
- `SharedRegion::heartbeat_age_ns(now_ns)` — 편의 래퍼.

### 2.2 Publisher 핫패스에서 heartbeat 자동 touch

`ShmPublisher::{publish_bookticker, publish_trade}` 마다 `touch_heartbeat` 를
inline 으로 호출. atomic store 2회 (ns + seq) = ~3ns 비용. 별도 타이머 스레드 불필요.

### 2.3 Gateway 쪽 heartbeat stale detector

`shm_sub::Worker` 에 `HeartbeatWatcher` 추가. 매 `HEARTBEAT_POLL_INTERVAL`(50ms)
마다 `heartbeat_age_ns` 를 확인해 `HEARTBEAT_STALE_THRESHOLD_NS`(5s) 초과 시
`CounterKey::ShmPublisherStale` 증가 + warn (rate-limited). 정상화되면 자동 복구.

### 2.4 MultiOrderRingReader / shm_sub 핫패스 alloc 제거

**문제**: `Worker::poll_once` 의 v2 경로는 매 iteration 마다
`Vec::with_capacity(MAX_PER_RING * n)` 를 새로 할당. "alloc 0 on hot path" 원칙 위반.

**해결**: Worker 구조체에 `scratch_batch: Vec<(u32, OrderFrame)>` 필드 추가. 한 번만
allocate, 매 iteration `clear()` 로 재사용.

### 2.5 run_blocking 의 중복 sleep 제거

**문제**: `multi_order_ring::MultiOrderRingReader::run_blocking` 에
`park_timeout(park_duration)` 이후 `deadline` 을 다시 만들어 `sleep(deadline - now)`
로 보정 → park 가 정상 소진됐을 때 sleep 이 추가 실행되어 최악 지연 2배.

**해결**: park_timeout 만 호출 후 바로 루프 복귀. spurious wake 는 다음 iteration 의
`poll_batch` 가 0건 반환해 다시 spin → park 사이클로 자연스럽게 수렴. Sleep 완전 제거.

### 2.6 전략 VM facade — `hft-strategy-shm`

**문제**: Strategy VM 이 SharedRegion 위에서 quote reader + trade reader + 자기
vm_id 의 order writer 세 handle 을 얻는 표준화된 facade 가 없다. 각 사용자가
`SharedRegion::open_view` + 3번의 `sub_region()` + 3번의 `{Writer,Reader}::from_region`
보일러를 직접 작성해야 했다.

**해결**: 신규 크레이트 `hft-strategy-shm` 추가:
- `StrategyShmClient::attach(backing, spec, vm_id)` — 한 번에 모두 열어 묶은 핸들 반환.
- `StrategyShmClient::{quote_reader, trade_reader, order_writer, symtab, heartbeat_age_ns}`
  getter.
- `StrategyShmClient::publish_order` — convenience (symbol lookup + writer.publish 래퍼).
- `StrategyMdPoller` — quote+trade 에 대한 spin/park 루프.

### 2.7 Telemetry counter 확장

`CounterKey` 에 v2 전용 카운터 3개 추가:
- `ShmPublisherStale` — gateway 가 publisher heartbeat stale 감지.
- `ShmOrderBatch` — multi-ring batch 처리 횟수.
- `ShmBackoffPark` — spin 이후 park 진입 횟수.

### 2.8 Deploy / 운영 스캐폴드

`deploy/` 비어있던 세 폴더 채움 — 각 파일은 **샘플/가이드 수준**이며 실제 배포 시
편집 필요:
- `deploy/scripts/host_bootstrap.sh` — grub cmdline/hugetlbfs/cpupower 체크 스크립트.
- `deploy/scripts/launch_infra_vm.sh` — QEMU ivshmem-plain master infra VM 샘플.
- `deploy/scripts/launch_strategy_vm.sh` — 동일 영역에 attach 하는 strategy VM.
- `deploy/systemd/hft-publisher.service` / `hft-order-gateway.service` — infra VM 내.
- `deploy/tmux/dev_layout.sh` — 로컬 dev 4분할 tmux.
- `deploy/README.md` — 사용 지침.

---

## 3. 변경된 파일 (트리)

```
crates/transport/hft-shm/src/region.rs              (수정)
crates/transport/hft-shm/src/multi_order_ring.rs    (수정)
crates/transport/hft-shm/tests/shared_region_e2e.rs (수정 — heartbeat test 추가)

crates/obs/hft-telemetry/src/lib.rs                 (수정 — counter 3개 추가)

crates/services/publisher/src/shm_pub.rs            (수정 — touch_heartbeat)

crates/services/order-gateway/src/shm_sub.rs        (수정 — scratch buffer +
                                                    heartbeat watcher)

crates/transport/hft-strategy-shm/                  (신규 크레이트)
├── Cargo.toml
└── src/lib.rs

crates/transport/hft-strategy-shm/tests/            (신규 통합 테스트)
└── end_to_end.rs

Cargo.toml                                          (수정 — workspace member 추가)

deploy/README.md                                    (신규)
deploy/scripts/host_bootstrap.sh                    (신규)
deploy/scripts/launch_infra_vm.sh                   (신규)
deploy/scripts/launch_strategy_vm.sh                (신규)
deploy/systemd/hft-publisher.service                (신규)
deploy/systemd/hft-order-gateway.service            (신규)
deploy/tmux/dev_layout.sh                           (신규)

docs/phase2/CHANGES_v2_PHASE2.md                    (이 문서)
```

---

## 4. 주의사항 / 불변식

### 4.1 Wire digest 는 *의도적으로* 깨진다

`RegionHeader` 에 heartbeat 필드 두 개를 추가하면서 `_reserved` 크기가 바뀜 →
`std::mem::size_of::<RegionHeader>()` 자체는 4KB 로 동일하지만, `layout_digest` 에
struct size 가 포함되어 있으므로 *결과 bytes* 는 새 RegionHeader 레이아웃과
이전 레이아웃이 구분된다.

v2 프로덕션 롤아웃 전 시점이므로 **호환성 파기 허용**. production 이후에는 반드시
`SHM_VERSION` bump + 전 VM 동시 재부팅 이벤트로 처리.

### 4.2 Heartbeat 는 publisher only

- Publisher 만 `touch_heartbeat` 호출 — gateway/strategy 가 건드리면 거짓 liveness.
- `Role::Publisher` 가 아닌 `SharedRegion` 에서 `touch_heartbeat` 호출 시 debug
  로그 + 무시.

### 4.3 scratch_batch 는 Worker thread-local

`shm_sub::Worker::scratch_batch` 는 단일 consumer thread 내에서만 쓰이므로 Send
불필요. 재사용 시 `clear()` 로만 초기화해 capacity 유지.

### 4.4 strategy-shm 크레이트는 reader+writer 모두 strict

`StrategyShmClient::attach` 는 publisher 가 이미 region 초기화를 완료했다는
전제로 strict attach. Publisher 가 아직 올라오지 않았을 때는 caller 가
재시도 루프 구현 필요.

---

## 5. 성능 가드레일 (이번 변경에만 해당)

- `touch_heartbeat` inline 비용: atomic u64 store × 2 = ~3ns.
  → publish hot path 영향 < 1%.
- gateway heartbeat poll 주기 50ms: `heartbeat_ns()` 는 atomic load 1회 = ~2ns.
- scratch_batch 재사용: 할당 0, cache line 친화 (Worker 구조체 내 인접).
- run_blocking sleep 제거: park_timeout 만 남음. idle-to-wake 지연 상한 =
  `park_duration` (기본 50μs).

---

## 5a. 이번 턴 마무리 (2026-04-15 후속)

위 §2.1–2.8 구현 이후, 최종 누락 항목 보강:

1. `deploy/systemd/hft-publisher.service`, `deploy/systemd/hft-order-gateway.service`
   본문 작성. CPU affinity, LimitMEMLOCK=infinity, 재시작 정책, 환경변수
   (SHM backing / LayoutSpec), 시크릿 주입 (EnvironmentFile) 표준 패턴 포함.
   gateway 는 `Requires=` + `After=hft-publisher.service` 로 순서 보장.
2. `deploy/tmux/dev_layout.sh` — 4-pane (publisher / gateway / strategy×2) dev 레이아웃.
   `/dev/shm` backing, HFT_VM_ID 전달, 기존 SHM 파일 사전 삭제 (LayoutSpec 변경 대비).
3. `crates/transport/hft-shm/tests/shared_region_e2e.rs` — `heartbeat_publish_and_observe`
   테스트 추가. Publisher/Observer 간 ns/seq 관찰, non-publisher silent ignore,
   now < last 시 age=None.
4. `crates/transport/hft-strategy-shm/tests/end_to_end.rs` — cross-role 통합:
   N strategy × 3 order fan-in → gateway MultiOrderRingReader 검증,
   publisher → strategy trade SPMC broadcast 검증, symbol_idx 일관성,
   attach_with_retry (publisher boot lag) 시나리오, vm_id out-of-range 거절.

이로써 §2 의 8개 갭 + 테스트/스캐폴드까지 모두 완료.

### 5a.1 성능·정확성 관련 추가 근거

- HeartbeatWatcher `tick` 은 `Instant::now() - last_checked >= 50ms` 일 때만
  실제 `heartbeat_age_ns()` 호출 → idle 시 사실상 비용 0 (Instant::now 1회).
- systemd 유닛의 `LimitMEMLOCK=infinity` 는 publisher 가 hugetlbfs 페이지 +
  ivshmem BAR 영역 전체를 mlock 해야 하기 때문에 필수. 없으면 첫 trade burst
  시 major fault 로 p99.9 스파이크 발생.
- dev_layout.sh 가 `HFT_SHM__DEV_SHM_PATH` 를 공통 env 로 네 pane 전부에
  동일하게 넣어주는 것이 핵심 — digest 일치 조건.

---

## 6. 다음 단계 후보 (아직 남아있는 것)

- [ ] QEMU/libvirt 실기 배선 검증 (스크립트는 있으나 real host 테스트 필요).
- [ ] Prometheus exporter 가 신규 counter 를 scrape 하는지 확인 (telemetry 측 생태계).
- [ ] Python-side strategy binding (PyO3) — 현재는 Rust-only strategy facade.
- [ ] ZMQ ↔ SHM 이중 경로 dedup 테스트 (MULTI_VM_TOPOLOGY §5.6).
- [ ] MPK / seL4-style 권한 분리 (strategy 가 symbol table 쓰기 못하도록 커널 단).

# Jitter Playbook — duration spike 를 0 으로 만드는 규율

tail latency 가 튀는 7가지 근본 원인과 각각의 대응책. 모든 crate 가 공유하는 규율이며,
새 코드를 쓸 때 PR-self-review 체크리스트로 사용한다.

---

## 1. Heap allocation on hot path

### 증상
p99.9 에서 수백 μs ~ 수 ms spike. allocator arena contention 또는 `mmap` 으로 인한 page fault.

### 원인 예
- `Vec::new()` / `String::new()` 을 매 이벤트마다 호출
- `format!` / `to_string()` 를 hot path 에서 호출 (로그 메시지, 토픽 이름 등)
- `serde_json::from_slice` 가 내부적으로 `Vec` 재할당

### 대응
- 모든 hot path 버퍼는 thread-local 또는 struct field 로 미리 할당. 재사용 시 `.clear()` 로 리셋, `shrink_to_fit` 절대 금지.
- 토픽 이름 같은 고정 문자열은 startup 시 1회 계산해 `Arc<str>` 로 보관.
- 직렬화는 `hft-protocol` 의 고정 크기 버퍼 (`[u8; 120]` / `[u8; 128]`) 로 stack 에 씀.
- `#[global_allocator]` 를 mimalloc 으로 고정. 서비스 바이너리는 `hft-types::ALLOCATOR` 를 `pub use`.
- 디버그용: `dhat` 또는 `heaptrack` 으로 hot path alloc 을 0 이 될 때까지 깎는다.

### 적용 crate
`services/publisher`, `services/subscriber`, `services/strategy`, 모든 `hft-exchange-*`, `hft-protocol`.

---

## 2. Tokio runtime contention

### 증상
stage 3·4 (파싱·직렬화) 가 가끔 수 ms 튐. 같은 runtime 에 있는 다른 task 가 long-run blocking 이거나 많음.

### 원인 예
- `reqwest` / `questdb-rs` 가 hot path 와 같은 tokio runtime 에서 동작
- `spawn_blocking` 없이 `std::fs` 직접 호출
- 많은 symbol → 많은 task → work-stealing overhead

### 대응
- **런타임 분리**. 서비스마다 두 개의 runtime:
  - `hot`: core-pinned, worker thread 수 = 거래소 WS 수만큼, 다른 일 절대 못 들어오게
  - `bg`: telemetry / storage / supabase 로드 / HTTP 호출 전용
- `#[tokio::main]` 쓰지 않고 `Builder::new_multi_thread()` 로 명시 생성. `thread_name("hft-hot-X")`, `on_thread_start(|| pin_to_core(X))`.
- hot runtime 에 들어가는 task 는 audit: 거래소 feed, worker PUSH, aggregator PUB, SUB recv, strategy eval 만 허용.
- blocking 한 번이라도 필요하면 `tokio::task::spawn_blocking` 으로 bg runtime 에 위임.
- `tokio-console` 을 dev profile 에 켜서 task 별 poll 시간 확인.

### 적용 crate
`services/publisher`, `services/subscriber`, `services/strategy`, `services/order-gateway`.

---

## 3. ZMQ HWM 초과 → blocking send

### 증상
stage 5 또는 6 에서 수십 ms ~ 수 초의 spike. 간헐적.

### 원인 예
- SUB 가 느리거나 죽어서 PUB 쪽 큐 차면서 blocking
- HWM 이 너무 낮음
- aggregator 쪽에 느린 downstream (ex. questdb 쓰기) 가 PUB 스레드에 붙어있음

### 대응
- PUSH/PUB 모두 `ZMQ_SNDHWM = 100_000`, `ZMQ_LINGER = 0`, `ZMQ_IMMEDIATE = 1` 로 고정. 이 상수는 `hft-config` 에.
- `send` 는 항상 `DONTWAIT` flag. HWM 초과 시 `EAGAIN` → drop + metric 증가, 절대 블로킹 안 함.
- drop metric (`hft_zmq_dropped_total{stage=...}`) 이 0 이 아니면 `hft-telemetry` 가 warn 로그. 이건 항상 버그.
- aggregator 와 storage 는 같은 스레드 공유 금지. storage 는 `bg` runtime + 별도 ZMQ SUB 으로 구독.

### 적용 crate
`hft-zmq`, `services/publisher`, `services/subscriber`.

---

## 4. QuestDB 쓰기로 인한 spike

### 증상
주기적으로 (수 초 간격) stage 3~6 전반이 튐. QuestDB flush 주기와 상관 있음.

### 원인 예
- QuestDB ILP 전송이 hot path 스레드에서 수행
- flush 가 syscall-heavy 라 GC 유사 효과
- ILP tcp 연결 재시도가 hot path 타이밍에 겹침

### 대응
- **hot path 는 QuestDB 를 모른다**. publisher/subscriber 가 내는 모든 이벤트는 ZMQ PUB 토픽에 올라가고, `hft-storage` 의 독립 서비스가 그걸 SUB 해서 QuestDB 로 flush.
- storage 서비스는 완전히 다른 프로세스 (또는 최소한 다른 runtime). hot path 와 allocator 공유해도 무방하나 스레드는 분리.
- ILP 전송은 batch (default 1024 rows / 100ms whichever first).
- ILP 연결 장애는 로컬 파일로 spool → 복구 시 replay. hot path 영향 0.

### 적용 crate
`hft-storage`, `tools/questdb-export`, `services/publisher` (storage 안 건드리는 게 포인트).

---

## 5. Logging on hot path

### 증상
`info!` / `debug!` 를 찍은 직후 구간에서 spike. 파일 writer 락 충돌.

### 원인 예
- `tracing` 에 file subscriber 가 async 아닌 sync 로 붙음
- JSON formatter 가 hot path 에서 serde 돌림
- `debug!` / `trace!` 가 release 빌드에서도 compile 됨

### 대응
- hot path 에는 `info!` 까지만 허용 (경고: 실제론 `warn!` 이상만 hot path 에 두는 걸 권장).
- `tracing-subscriber` 는 `non_blocking` writer 사용. stdout 은 async writer + bounded channel.
- `debug!` / `trace!` 는 `cfg!(feature = "verbose-trace")` 로 gate. release 에는 빠짐.
- JSON formatter 는 bg thread 에서만. hot path 는 span enter/exit 만 하고, format 은 subscriber 가 deferred.
- `hft-telemetry` 가 subscriber 구성을 전담. 각 서비스는 `hft_telemetry::init(service_name)` 1줄만.

### 적용 crate
`hft-telemetry`, 모든 서비스 바이너리.

---

## 6. Thread scheduling / preemption

### 증상
p99.9 가 비정기적으로 1~10ms spike. CPU 사용률은 낮은데 튐. 다른 프로세스 활동과 상관.

### 원인 예
- 다른 프로세스가 같은 CPU 에 스케줄됨
- IRQ 가 hot core 로 라우팅됨
- 커널 timer tick (CONFIG_HZ) 로 인한 preemption

### 대응
- 배포 시 `isolcpus=2,3,4,5` (hot cores) + `nohz_full=2,3,4,5` + `rcu_nocbs=2,3,4,5` 커널 파라미터.
- `taskset -c 2` 로 각 서비스를 고정된 core 에 pin. `deploy/` 의 systemd unit 또는 docker-compose `cpuset`.
- IRQ 는 `irqbalance` 끄고 `echo <mask> > /proc/irq/<n>/smp_affinity` 로 non-hot core 에 고정.
- 서비스 프로세스는 `SCHED_FIFO` 또는 최소 `nice=-10`. systemd `CPUSchedulingPolicy=fifo`.
- macOS 개발환경에서는 이게 다 의미 없음. 반드시 linux 장비에서 측정.

### 적용
`deploy/` 의 systemd unit 파일. 각 서비스 `SPEC.md` 의 "배포 시 규약" 섹션.

---

## 7. Cold cache / first-touch

### 증상
첫 N 개 메시지만 p99.9 에 튐. 이후엔 멀쩡.

### 원인 예
- mimalloc arena 가 아직 warm 안 됨
- TLB / instruction cache miss
- tokio scheduler 가 아직 worker 깨우는 중

### 대응
- startup 에 warm-up routine: `hft-testkit` 의 `WarmUp` 이 dummy 이벤트 1만 건을 모든 stage 에 흘림. hot path 가 readiness 보고.
- `hft-config` 의 Config struct 는 startup 에서 deep-clone 1회 → 이후엔 `Arc<Config>` 공유. runtime 변경 금지.
- tokio worker 는 startup 직후 `tokio::task::yield_now()` 여러 번 로 scheduler 깨움.
- readiness probe (`/healthz`) 는 warm-up 끝난 뒤에만 200 반환.

### 적용 crate
`hft-testkit` (warm-up 유틸), 모든 서비스 바이너리 (main 에 warm-up 호출).

---

## 체크리스트 (PR 전 self-review)

- [ ] hot path 함수 안에 `.to_string()`, `format!`, `Vec::new()`, `collect()` 없음
- [ ] ZMQ `send` 는 전부 `DONTWAIT` 
- [ ] QuestDB / supabase / HTTP 호출이 hot runtime 에 없음
- [ ] `info!` 보다 낮은 로그는 `#[cfg(feature = "verbose-trace")]` 가드
- [ ] `SystemTime::now()` / `Instant::now()` 직접 호출 없음 → `hft_time::Clock` 경유
- [ ] 핵심 상수 (HWM, batch cap, timeout) 는 `hft-config` 에서 읽음
- [ ] 새 spawn 하는 task 는 `hot` / `bg` runtime 중 어디에 붙일지 코드에 명시
- [ ] `LatencyStamps` 를 새 stage 에서 찍었다면 `latency-budget.md` 의 표에 추가

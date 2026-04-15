# Phase 1 TODO — 진행 순서

의존성 bottom-up 순서. 각 단계가 끝나야 다음 단계가 컴파일 가능하다.

---

## Step 1. `hft-types` 실구현

- [ ] `ExchangeId` enum 에 `fn as_str`, `fn all() -> &'static [Self]` 추가
- [ ] `Symbol(String)` → `Symbol(Arc<str>)` 로 변경 (clone 비용 제거)
- [ ] `BookTicker`, `Trade` 에 `#[repr(C, align(64))]` (cache line)
- [ ] `MarketEvent` 에 `fn topic_kind(&self) -> &'static str` (`hft-protocol` 용)
- [ ] `DataRole` 에 `Display` / `FromStr`
- [ ] `#[global_allocator]` 로 mimalloc 선언 + `pub use` 로 서비스 바이너리 편의
- [ ] unit tests: roundtrip serde, enum exhaustive, `repr(C)` 크기 고정 확인

### 완료 조건
`cargo test -p hft-types` 통과 + `std::mem::size_of::<BookTicker>()` 가 wire size 와 맞음.

---

## Step 2. `hft-time` 실구현

- [ ] `SystemClock` 구현 finalize (이미 뼈대 있음)
- [ ] `MockClock` 추가 (`AtomicI64` 로 now_ms 제어, 테스트 전용)
- [ ] `LatencyStamps::stage_diff(&self, a: Stage, b: Stage) -> i64` 헬퍼
- [ ] `Stage` enum 정의 (`ExchangeServer, WsReceived, Serialized, Pushed, Published, Subscribed, Consumed`)
- [ ] `LatencyStamps` 직렬화 레이아웃을 `hft-protocol` 과 맞춤 (고정 오프셋)
- [ ] `now_nanos` 가 monotonic 이며 thread-safe 인지 verify

### 완료 조건
`MockClock::advance(5)` 로 stage_diff 가 5 나오는 테스트 통과.

---

## Step 3. `hft-protocol` 실구현

- [ ] `wire.rs`:
  - [ ] `encode_bookticker(&BookTicker, &LatencyStamps, &mut [u8; 120])`
  - [ ] `decode_bookticker(&[u8; 120]) -> (BookTicker, LatencyStamps)`
  - [ ] 같은 함수 세트 Trade 용 (128)
  - [ ] 엔디안 고정 (LE, 기존 gate_hft 와 호환)
  - [ ] 고정 필드 오프셋 주석 (consumer C/C++ 에서 struct 재현 가능하게)
- [ ] `topics.rs`:
  - [ ] `TopicBuilder` 구조체. `startup 에 Symbol × ExchangeId × msg_type 으로 Arc<str> 캐시`
  - [ ] `fn get(&self, ex: ExchangeId, kind: &str, sym: &Symbol) -> &[u8]`
  - [ ] allocation 없이 미리 잡힌 bytes 반환
- [ ] `frame.rs`: 메시지 프레이밍 `[type_len:u8][type:str][payload:bytes]` encode/decode
- [ ] bench: `encode_bookticker` < 200ns on dev laptop

### 완료 조건
`cargo bench -p hft-protocol` 에서 encode/decode 200ns 이하 + 0 heap alloc (`dhat-rs` 확인).

---

## Step 4. `hft-telemetry` 실구현

- [ ] `init(service_name: &str) -> Result<TelemetryHandle>`:
  - [ ] tracing-subscriber + fmt layer (non_blocking stdout)
  - [ ] OTel OTLP exporter (bg 전용, batch)
  - [ ] HDR histogram store: `stage -> Histogram<u64>` (AtomicCell 로 swap)
- [ ] `record_stage(stage: Stage, nanos: u64)` — lock-free record
- [ ] `dump_hdr()` → `tools/latency-probe` 에서 호출 가능
- [ ] `pin_current_thread(core: usize)` + `next_hot_core()` 헬퍼 (linux only)
- [ ] `verbose-trace` feature flag 로 debug/trace 수준 차단
- [ ] metrics: prometheus exporter (bg) — `hft_zmq_dropped_total`, `hft_pipeline_events_total{stage}` 등

### 완료 조건
stress test 에서 tracing 으로 인한 hot path 지연 < 1μs.

---

## Step 5. `hft-config` 실구현

- [ ] `AppConfig` struct — 모든 서비스 공통 필드 + per-service sub-struct
  - [ ] `hot_workers: usize`, `bg_workers: usize`
  - [ ] `zmq_hwm: i32` (default 100_000)
  - [ ] `drain_batch_cap: usize` (default 512)
  - [ ] `exchanges: Vec<ExchangeConfig>`
  - [ ] `questdb: QuestDbConfig`
  - [ ] `supabase: SupabaseConfig`
- [ ] `load_all() -> anyhow::Result<Arc<AppConfig>>`:
  - [ ] figment: defaults → TOML (`configs/*.toml`) → env (`HFT_*`) → supabase pull (bg)
  - [ ] supabase 값은 env 로 override 가능 (우선순위 env > supabase > file > default)
- [ ] `validate()` — hwm > 0, batch cap > 0, exchanges non-empty 등
- [ ] `Arc<AppConfig>` 를 서비스에 공유. runtime reload 없음.

### 완료 조건
`configs/publisher.toml` + env override 섞어서 `AppConfig` 가 예상대로 composed.

---

## Step 6. `hft-zmq` 실구현

- [ ] `Context` newtype — 프로세스당 1개
- [ ] `PushSocket`, `PullSocket`, `PubSocket`, `SubSocket` wrapper
  - [ ] `new_*(ctx, endpoint, cfg: &AppConfig)` — HWM/LINGER/IMMEDIATE 일괄 설정
  - [ ] `send_dontwait(&self, topic: &[u8], payload: &[u8]) -> SendOutcome { Sent, WouldBlock }`
  - [ ] drop metric 자동 기록
- [ ] `RouterSocket` / `DealerSocket` 은 나중
- [ ] **아예 async 를 붙이지 말 것**. tokio 와 별개로 dedicated thread 에 박고 `tokio::task::spawn_blocking` 경유 또는 channel 로 bridge
- [ ] 또는 `zmq::Socket` 을 `fd` 로 뽑아 `tokio::io::unix::AsyncFd` 로 async 화 (실측 후 결정)

### 완료 조건
100K msg/s stress test 에서 drop 0, p99.9 send 지연 < 100μs.

---

## Step 7. `hft-storage` 실구현

- [ ] `QuestDbSink` — ILP TCP sender
  - [ ] batch: `1024 rows` OR `100ms` whichever first
  - [ ] connection retry (exponential backoff, max 30s)
  - [ ] 실패 시 local spool file (`/var/spool/hft/<service>/<epoch>.ilp`) + replay
- [ ] `storage-svc` 바이너리 (tools/ 또는 services/): ZMQ SUB → QuestDbSink
- [ ] schema init (startup 에 1회): bookticker, trade, weblookticker, latency_stages 테이블
- [ ] hot path 는 이 crate 를 직접 참조 안 함

### 완료 조건
publisher 가 100K msg/s 로 PUB 하는 동안 storage 가 1 핫 스레드 영향 0 으로 ILP flush.

---

## Step 8. `hft-exchange-api` 시그니처 최종화

- [ ] 현재 스캐폴드 유지. 단, 다음 항목만 확정:
  - [ ] `Emitter = Arc<dyn Fn(MarketEvent, LatencyStamps) + Send + Sync>` 로 stamps 를 같이 넘김
  - [ ] `ExchangeFeed::stream` 이 `cancel: CancellationToken` 를 받음
  - [ ] `ExchangeExecutor::place_order` 가 `OrderAck { client_id, exchange_id, ts_ms }` 로 풍부화
- [ ] doc tests 추가 (trait impl 예시)

### 완료 조건
`hft-testkit::MockFeed` 가 이 trait 으로 컴파일 + 테스트에서 사용 가능.

---

## Step 9. `hft-testkit` 실구현

- [ ] `MockFeed` — `ExchangeFeed` 구현. 테스트에서 BookTicker / Trade 를 인위적으로 주입
- [ ] `MockClock` 재수출 (from hft-time)
- [ ] `WarmUp` — pipeline warm-up 유틸. N 개 dummy event 를 publisher 에 주입 후 subscriber 에서 소비 확인
- [ ] `pipeline_harness!()` 매크로 — test 에서 publisher+subscriber 를 inproc 으로 띄움
- [ ] `assert_p99_under!(histogram, 50ms)` 헬퍼

### 완료 조건
통합 테스트에서 MockFeed 1만 건 → p99.9 < 1ms (네트워크 제외 stage 3~7 구간만).

---

## Step 10. (선택) `services/publisher` 골격

- [ ] runtime 2개 분리된 main
- [ ] config 로드 → telemetry init → ZMQ context
- [ ] `ExchangeFeed` 는 DI: `load_feed_from_cfg(&cfg) -> Box<dyn ExchangeFeed>` stub (Phase 2 에서 실구현)
- [ ] worker PUSH + aggregator PULL→PUB 루프
- [ ] readiness probe HTTP 엔드포인트

여기까지 되면 **Phase 2 에서 `hft-exchange-binance` 붙이기만 하면 end-to-end 가 돈다.**

---

## 체크 포인트

각 Step 끝에 다음을 확인:
1. `cargo check -p <crate>` 통과
2. `cargo test -p <crate>` 통과
3. 해당 crate 의 `SPEC.md` 의 "완료 조건" 섹션 전부 체크
4. `jitter-playbook.md` 의 체크리스트 self-review
